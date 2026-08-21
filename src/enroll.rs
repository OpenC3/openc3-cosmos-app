// Copyright 2026 OpenC3, Inc.
// All Rights Reserved.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
// See LICENSE.md for more details.
//
// This file may also be used under the terms of a commercial license
// if purchased from OpenC3, Inc.

//! openc3-cosmos-app's control-plane identity and one-time enrollment with a bridge.
//!
//! openc3-cosmos-app authenticates to the COSMOS `bridge_microservice` hub with its own
//! persistent Iroh identity. Only its **public** `EndpointId` ever leaves the
//! host; the private key is stored locally under `<root>/bridge/`.
//!
//! Enrollment is one-time and yields the hub ticket, persisted in
//! `<root>/bridge/current.json` (openc3-cosmos-app pairs with a single bridge). Two
//! paths:
//! * **Auto** (co-located, default): openc3-cosmos-app reaches COSMOS over the trusted
//!   local Docker control plane and runs the `bridgeenroll` CLI to register its
//!   public key and read back the hub ticket. That local access is the
//!   out-of-band trust anchor that makes zero-touch pairing secure.
//! * **Manual** (remote COSMOS): the user pastes an enrollment token (generated
//!   on the COSMOS Admin → Bridges page) into openc3-cosmos-app; [`enroll_with_token`]
//!   redeems its one-time code over the hub's `api/enroll` ALPN.

use anyhow::{bail, Context as _, Result};
use base64::Engine as _;
use iroh::SecretKey;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::bridge::{self, BridgeClient};
use crate::context::Context;
use crate::docker;

/// The bridge openc3-cosmos-app is currently paired with, persisted across launches.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Current {
    bridge: String,
    ticket: String,
    /// How this pairing was obtained. Auto tickets come from `bridgeenroll` over
    /// local Docker and are cheap to re-derive, so a context switch may clear
    /// them; a manual token (redeemed from Admin → Bridges) is user-supplied and
    /// must be preserved. Defaults to false (auto) for tickets written before
    /// this field existed.
    #[serde(default)]
    manual: bool,
}

/// Where the ticket used for a connection attempt came from. Only an
/// auto-enrolled cached ticket is safe to replace without user input.
enum TicketSource {
    Environment,
    CachedAuto { bridge: String },
    CachedManual,
    AutoEnrolled,
}

struct ResolvedTicket {
    ticket: String,
    source: TicketSource,
}

/// A manual enrollment token's decoded payload (base64url JSON), produced by the
/// COSMOS Admin Bridges page / `bridgetoken` CLI.
#[derive(Debug, Deserialize)]
struct EnrollToken {
    bridge: String,
    ticket: String,
    code: String,
}

fn bridge_dir(root: &Path) -> PathBuf {
    root.join("bridge")
}

fn identity_path(root: &Path) -> PathBuf {
    bridge_dir(root).join("identity.key")
}

fn current_path(root: &Path) -> PathBuf {
    bridge_dir(root).join("current.json")
}

/// A concise, single-line form of an error's top-level message for the GUI
/// status line (the full chain always goes to the log). Keeps the real reason
/// visible without letting a multi-line stderr blow up the status text.
fn brief(err: &anyhow::Error) -> String {
    let msg = err.to_string();
    let line = msg.lines().next().unwrap_or("").trim();
    const MAX: usize = 160;
    if line.chars().count() > MAX {
        format!("{}…", line.chars().take(MAX - 1).collect::<String>())
    } else {
        line.to_string()
    }
}

/// Lowercase hex-encode bytes.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Decode exactly 32 bytes of hex.
fn decode_key(s: &str) -> Result<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 {
        bail!("expected 64 hex chars, got {}", s.len());
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).context("invalid hex in key")?;
    }
    Ok(out)
}

/// Load openc3-cosmos-app's persisted Iroh identity, generating and saving one on first
/// use. openc3-cosmos-app persists its OWN private key here (unlike the ephemeral keys
/// it later mints for host microservices).
fn load_or_create_secret(root: &Path) -> Result<SecretKey> {
    let path = identity_path(root);
    if path.exists() {
        let contents = std::fs::read_to_string(&path).context("reading bridge identity")?;
        return Ok(SecretKey::from_bytes(&decode_key(&contents)?));
    }
    let secret = SecretKey::generate();
    std::fs::create_dir_all(bridge_dir(root)).ok();
    std::fs::write(&path, hex(&secret.to_bytes())).context("writing bridge identity")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(secret)
}

/// openc3-cosmos-app's public identity as hex (its Iroh `EndpointId`).
fn public_key_hex(secret: &SecretKey) -> String {
    hex(secret.public().as_bytes())
}

fn read_current(root: &Path) -> Option<Current> {
    let contents = std::fs::read_to_string(current_path(root)).ok()?;
    serde_json::from_str(&contents).ok()
}

/// Forget the cached hub ticket so the next connect re-runs enrollment from
/// scratch against the current context. Used when Development Mode changes which
/// COSMOS compose context the bridge talks to: an auto-enrolled ticket may point
/// at a different COSMOS (or none), so we drop it and let auto-enroll resolve the
/// correct one. A **manual** token (redeemed from Admin → Bridges) is preserved —
/// it's user-supplied, not derivable from the local context, so clearing it would
/// silently break a remote pairing. Best-effort — a missing file is success.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn forget_cached_ticket(root: &Path) {
    if let Some(current) = read_current(root) {
        if current.manual {
            crate::logging::info(
                "bridge",
                "Keeping manually-paired bridge ticket (not clearing on context change)",
            );
            return;
        }
    }
    match std::fs::remove_file(current_path(root)) {
        Ok(()) => crate::logging::info("bridge", "Cleared cached bridge ticket; will re-enroll"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => crate::logging::warn("bridge", &format!("could not clear cached bridge ticket: {e}")),
    }
}

fn write_current(root: &Path, bridge: &str, ticket: &str, manual: bool) -> Result<()> {
    std::fs::create_dir_all(bridge_dir(root)).ok();
    let current = Current {
        bridge: bridge.to_string(),
        ticket: ticket.to_string(),
        manual,
    };
    std::fs::write(current_path(root), serde_json::to_string_pretty(&current)?)
        .context("persisting current bridge")?;
    Ok(())
}

/// Resolve the hub ticket to connect with: an explicit `OPENC3_BRIDGE_TICKET`
/// (override), else a previously paired bridge (`current.json`), else auto-enroll
/// over the local Docker control plane. The bridge defaults to `DEFAULT` (every
/// scope has a DEFAULT bridge); `OPENC3_BRIDGE_NAME` overrides it. On failure
/// returns a short human reason (shown in the GUI) explaining why it isn't paired.
fn resolve_ticket(ctx: &Context, app_public_key_hex: &str) -> Result<ResolvedTicket, String> {
    if let Ok(ticket) = std::env::var("OPENC3_BRIDGE_TICKET") {
        if !ticket.is_empty() {
            return Ok(ResolvedTicket {
                ticket,
                source: TicketSource::Environment,
            });
        }
    }
    if let Some(current) = read_current(&ctx.paths.root) {
        let source = if current.manual {
            TicketSource::CachedManual
        } else {
            TicketSource::CachedAuto {
                bridge: current.bridge,
            }
        };
        return Ok(ResolvedTicket {
            ticket: current.ticket,
            source,
        });
    }
    // Auto-enroll on first launch with the scope's DEFAULT bridge (co-located
    // COSMOS via local Docker). A remote/unmanaged COSMOS instead pairs with a
    // manual token, which lands in current.json and is picked up above.
    let name = std::env::var("OPENC3_BRIDGE_NAME")
        .ok()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "DEFAULT".to_string());
    let ticket = auto_enroll(ctx, &name, app_public_key_hex).map_err(|e| {
        // Full detail to the log; a short, real reason for the GUI. COSMOS is
        // confirmed up by the time we get here, so surface what actually failed
        // (usually the bridgeenroll CLI output) rather than "is COSMOS running?".
        crate::logging::warn("bridge", &format!("auto-enroll with '{name}' failed: {e:#}"));
        format!("enrolling bridge '{name}' failed: {}", brief(&e))
    })?;
    let _ = write_current(&ctx.paths.root, &name, &ticket, false); // auto
    crate::logging::info("bridge", &format!("auto-enrolled with '{name}'"));
    Ok(ResolvedTicket {
        ticket,
        source: TicketSource::AutoEnrolled,
    })
}

/// Register openc3-cosmos-app's public key with COSMOS and read back the hub ticket by
/// running the `bridgeenroll` CLI in the cmd-tlm-api container (local Docker).
fn auto_enroll(ctx: &Context, bridge_name: &str, app_public_key_hex: &str) -> Result<String> {
    let mut cmd = docker::compose(ctx)?;
    cmd.arg("run")
        .arg("--rm")
        .arg("--no-deps")
        .arg("openc3-cosmos-cmd-tlm-api")
        .arg("ruby")
        .arg("/openc3/bin/openc3cli")
        .arg("bridgeenroll")
        .arg(bridge_name)
        .arg(app_public_key_hex);
    let out = docker::capture(cmd)?;
    if !out.status.success() {
        bail!(
            "bridgeenroll failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    // The CLI prints only the ticket on stdout.
    let ticket = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if ticket.is_empty() {
        bail!("bridgeenroll returned no ticket");
    }
    Ok(ticket)
}

/// Redeem a manual enrollment token (from the COSMOS Admin Bridges page) for a
/// remote COSMOS. Decodes the token, redeems its one-time code over the hub's
/// `api/enroll` ALPN using openc3-cosmos-app's identity, and persists the pairing.
/// Returns the bridge name on success.
pub fn enroll_with_token(ctx: &Context, token: &str) -> Result<String> {
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token.trim())
        .context("enrollment token is not valid base64")?;
    let parsed: EnrollToken =
        serde_json::from_slice(&raw).context("enrollment token has an unexpected format")?;
    let secret = load_or_create_secret(&ctx.paths.root)?;
    bridge::enroll(secret, &parsed.ticket, &parsed.code).context("redeeming enrollment token")?;
    write_current(&ctx.paths.root, &parsed.bridge, &parsed.ticket, true)?; // manual
    crate::logging::info("bridge", &format!("enrolled with '{}' via token", parsed.bridge));
    Ok(parsed.bridge)
}

/// Load/create openc3-cosmos-app's identity, resolve the hub ticket (enrolling if
/// needed), and connect a [`BridgeClient`]. Returns `(hub_ticket, client)`, or
/// `None` if no bridge is configured or the connection fails. The returned
/// ticket is what host microservices use (via `OPENC3_BRIDGE_TICKET`) to dial
/// the hub for the data path.
/// On failure returns a short human reason (shown in the GUI) for why openc3-cosmos-app
/// isn't paired with COSMOS.
pub fn connect_bridge(ctx: &Context) -> Result<(String, BridgeClient), String> {
    let secret = load_or_create_secret(&ctx.paths.root).map_err(|e| {
        crate::logging::warn("bridge", &format!("could not load control identity: {e:#}"));
        "identity error".to_string()
    })?;
    let public_key = public_key_hex(&secret);
    let resolved = resolve_ticket(ctx, &public_key)?;
    match connect_and_validate(secret.clone(), &resolved.ticket) {
        Ok(client) => Ok((resolved.ticket, client)),
        Err(first_error) => {
            // A cached auto-enrollment can outlive the hub identity embedded in
            // its ticket (for example after switching compose contexts or
            // recreating COSMOS data). First try it as-is; only an Iroh peer
            // certificate mismatch triggers replacement. Manual tickets and an
            // explicit environment override remain user-owned and untouched.
            if peer_certificate_error(&first_error) {
                if let TicketSource::CachedAuto { bridge } = resolved.source {
                    crate::logging::warn(
                        "bridge",
                        "cached bridge ticket has a stale peer certificate; re-enrolling",
                    );
                    forget_cached_ticket(&ctx.paths.root);
                    let fresh = auto_enroll(ctx, &bridge, &public_key).map_err(|e| {
                        crate::logging::warn(
                            "bridge",
                            &format!("re-enrollment with '{bridge}' failed: {e:#}"),
                        );
                        format!("re-enrolling bridge '{bridge}' failed: {}", brief(&e))
                    })?;
                    write_current(&ctx.paths.root, &bridge, &fresh, false).map_err(|e| {
                        crate::logging::warn(
                            "bridge",
                            &format!("could not cache refreshed bridge ticket: {e:#}"),
                        );
                        format!("could not save refreshed bridge enrollment: {}", brief(&e))
                    })?;
                    return connect_and_validate(secret, &fresh)
                        .map(|client| (fresh, client))
                        .map_err(connection_error);
                }
            }
            Err(connection_error(first_error))
        }
    }
}

/// Build a client and perform one harmless API poll so certificate/identity
/// errors are detected during connection rather than after the operator has
/// accepted the client as configured.
fn connect_and_validate(secret: SecretKey, ticket: &str) -> Result<BridgeClient> {
    let client = BridgeClient::connect(secret, ticket)?;
    client
        .fetch_host_microservices()
        .context("validating bridge connection")?;
    Ok(client)
}

fn peer_certificate_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        let message = cause.to_string().to_ascii_lowercase();
        message.contains("invalid peer certificate") || message.contains("unknownissuer")
    })
}

fn connection_error(error: anyhow::Error) -> String {
    crate::logging::warn(
        "bridge",
        &format!("failed to connect to bridge_microservice: {error:#}"),
    );
    format!(
        "can't reach the bridge hub — it may still be starting, or its ticket is \
         stale after a COSMOS restart; press Retry ({})",
        brief(&error)
    )
}

#[cfg(test)]
mod tests {
    use super::peer_certificate_error;

    #[test]
    fn recognizes_peer_certificate_errors() {
        let error = anyhow::anyhow!(
            "connect api/host_microservices: cryptographic handshake failed: error 48: invalid peer certificate: UnknownIssuer"
        );
        assert!(peer_certificate_error(&error));
    }

    #[test]
    fn does_not_treat_transient_connection_errors_as_stale_certificates() {
        let error = anyhow::anyhow!("connect api/host_microservices: timed out");
        assert!(!peer_certificate_error(&error));
    }
}
