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

//! Iroh client to the COSMOS `bridge_microservice` hub.
//!
//! openc3-cosmos-app does not run its own Iroh server. It is a **client** of the
//! COSMOS-side `bridge_microservice` (the hub), dialing it with a configured
//! bridge ticket, and uses control APIs over dedicated ALPNs:
//!
//! * `api/host_microservices` — poll the list of host microservices to spawn.
//! * `api/log` — forward host microservice stdout up so COSMOS logs it too.
//! * `api/discovery` / `api/discovery_result` — long-poll for discovery requests
//!   (e.g. "which serial ports exist?"), run them on this host with the Python
//!   `openc3.discovery` runner, and return the results.
//!
//! The raw interface data path does NOT flow through openc3-cosmos-app: each spawned
//! host interface dials the hub directly on `stream/<name>` and is paired there
//! with the matching COSMOS `bridge_interface`.

use anyhow::{bail, Context as _, Result};
use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, RelayMode, RelayUrl, SecretKey};
use iroh_tickets::endpoint::EndpointTicket;
use std::str::FromStr;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

/// ALPN for the host-microservice list API.
const API_HOST_MICROSERVICES: &[u8] = b"api/host_microservices";
/// ALPN for the log-forwarding API.
const API_LOG: &[u8] = b"api/log";
/// Bootstrap ALPN for manual enrollment (redeeming a one-time code).
const API_ENROLL: &[u8] = b"api/enroll";
/// ALPN for publishing the authorized host-microservice identities.
const API_AUTHORIZE: &[u8] = b"api/authorize";
/// ALPN for syncing the scope's plugin files (lib/, requirements, pyproject).
const API_FILES: &[u8] = b"api/files";
/// ALPN for fetching host InterfaceStatus (tapped by the hub from the control
/// channel), so openc3-cosmos-app can show host interface status in its bridge section.
const API_INTERFACE_STATUS: &[u8] = b"api/interface_status";
/// ALPN for long-polling discovery requests from COSMOS.
const API_DISCOVERY: &[u8] = b"api/discovery";
/// ALPN for returning one discovery result to COSMOS.
const API_DISCOVERY_RESULT: &[u8] = b"api/discovery_result";
/// Upper bound on one `api/discovery` long-poll. The hub answers within 20s
/// (with an empty list when idle), so this only trips on a stalled connection.
const DISCOVERY_POLL_TIMEOUT: Duration = Duration::from_secs(30);
/// Upper bound on running one discovery. COSMOS gives up after 15s, so a slower
/// result would never be seen.
const DISCOVERY_RUN_TIMEOUT: Duration = Duration::from_secs(12);
/// Delay before re-polling after a failed `api/discovery` poll (hub down).
const DISCOVERY_RETRY: Duration = Duration::from_secs(5);
/// Python module that runs a Discovery class and prints its results as JSON.
const DISCOVERY_RUNNER_MODULE: &str = "openc3.discovery.discovery_runner";
/// Upper bound on a small API response we will read.
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
/// Upper bound on one `api/interface_status` round trip. The operator polls it
/// about once a second between cycles; an unreachable hub must not stall it.
const INTERFACE_STATUS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
/// Upper bound on a file-sync payload (plugin files can be larger).
const MAX_FILES: usize = 512 * 1024 * 1024;

/// One file in a plugin-file sync delta. `content` is base64 (standard). The
/// hub also sends a `sha256` (ignored here — openc3-cosmos-app re-hashes from disk on
/// the next sync to build its manifest).
#[derive(Debug, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub content: String,
}

/// The hub's response to a file sync: changed/new files + paths to delete.
#[derive(Debug, Default, Deserialize)]
pub struct FilesDelta {
    #[serde(default)]
    pub files: Vec<FileEntry>,
    #[serde(default)]
    pub deletions: Vec<String>,
}

/// Relay to use, from `OPENC3_BRIDGE_RELAY`. No relay by default (co-located
/// pairing is direct); set the variable to a relay URL to reach a REMOTE hub.
/// Must match the relay the hub advertises in its ticket.
fn relay_mode_from_env() -> RelayMode {
    let url = std::env::var("OPENC3_BRIDGE_RELAY").unwrap_or_default();
    let url = url.trim();
    if url.is_empty() {
        return RelayMode::Disabled;
    }
    match RelayUrl::from_str(url) {
        Ok(relay) => RelayMode::custom([relay]),
        Err(e) => {
            crate::logging::warn(
                "bridge",
                &format!("ignoring invalid OPENC3_BRIDGE_RELAY '{url}': {e}"),
            );
            RelayMode::Disabled
        }
    }
}

/// Mint a fresh Iroh identity for a host microservice. Returns
/// `(secret_key_hex, public_key_hex)`. openc3-cosmos-app hands the secret to the child
/// and never persists it; the public key is authorized with the hub.
pub fn generate_host_key() -> (String, String) {
    let secret = SecretKey::generate();
    let hex = |bytes: &[u8]| -> String {
        use std::fmt::Write;
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            let _ = write!(s, "{b:02x}");
        }
        s
    };
    (hex(&secret.to_bytes()), hex(secret.public().as_bytes()))
}

/// Redeem a manual enrollment `code` with the hub over the `api/enroll` ALPN,
/// using `secret_key` as openc3-cosmos-app's identity — which the hub records as the
/// authorized control key on success. One-shot; binds a temporary endpoint.
pub fn enroll(secret_key: SecretKey, bridge_ticket: &str, code: &str) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime for enrollment")?;
    let ticket: EndpointTicket = bridge_ticket.parse().context("parsing bridge ticket")?;
    let addr = ticket.endpoint_addr().clone();
    runtime.block_on(async move {
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(secret_key)
            .relay_mode(relay_mode_from_env())
            .bind()
            .await?;
        let conn = endpoint
            .connect(addr, API_ENROLL)
            .await
            .map_err(|e| anyhow::anyhow!("connect api/enroll: {e}"))?;
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| anyhow::anyhow!("open_bi: {e}"))?;
        let request = serde_json::json!({ "code": code }).to_string();
        send.write_all(request.as_bytes())
            .await
            .map_err(|e| anyhow::anyhow!("write request: {e}"))?;
        let _ = send.finish();
        let data = recv
            .read_to_end(MAX_RESPONSE)
            .await
            .map_err(|e| anyhow::anyhow!("read response: {e}"))?;
        let response: serde_json::Value =
            serde_json::from_slice(&data).context("parsing enroll response")?;
        endpoint.close().await;
        if response.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            Ok(())
        } else {
            let err = response
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("enrollment rejected");
            bail!("{err}")
        }
    })
}

/// One host microservice openc3-cosmos-app should spawn, as described by COSMOS via the
/// `api/host_microservices` API. Only the fields the launcher needs are typed;
/// `config_params`/`options` are forwarded to the host runner as opaque JSON.
#[derive(Debug, Clone, Deserialize)]
pub struct HostSpec {
    pub name: String,
    #[serde(default)]
    pub stream: String,
    #[serde(default)]
    pub config_params: Vec<serde_json::Value>,
    #[serde(default)]
    pub options: Vec<serde_json::Value>,
    /// BRIDGE_PROTOCOLs to run on the host next to the device. Each entry is
    /// `[READ|WRITE|READ_WRITE, protocol filename/classname, params...]`.
    #[serde(default)]
    pub protocols: Vec<serde_json::Value>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub needs_dependencies: bool,
    /// The plugin's synced files directory (see [`crate::hostfiles`]), whose
    /// Python dependencies are installed for this microservice.
    #[serde(default)]
    pub plugin_files: Option<String>,
}

/// A host interface's status, as reported over the control channel and tapped by
/// the hub. Only the fields openc3-cosmos-app displays are captured; the rest is ignored.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct InterfaceStatus {
    /// The host microservice name, used to match this status to a microservice.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub state: String,
    /// Bytes received from the device (rx) and written to it (tx).
    #[serde(default)]
    pub rxbytes: u64,
    #[serde(default)]
    pub txbytes: u64,
}

/// A discovery request from COSMOS: the Python discovery class filename
/// followed by its parameters, e.g. `["openc3/discovery/serial_discovery.py"]`.
#[derive(Debug, Deserialize)]
pub struct DiscoveryRequest {
    pub id: String,
    pub discovery: Vec<String>,
}

/// How to run Python discovery on this host: the interpreter of a venv with
/// `openc3` installed, plus the environment (PYTHONPATH to synced plugin libs).
#[derive(Clone, Debug)]
pub struct DiscoveryRunner {
    pub python: PathBuf,
    pub work_dir: PathBuf,
    pub env: BTreeMap<String, String>,
}

/// Shared, updatable discovery runner. `None` until the operator has finished
/// provisioning the discovery venv; requests before then report an error.
pub type DiscoveryRunnerHandle = Arc<Mutex<Option<DiscoveryRunner>>>;

/// Client connection to the COSMOS `bridge_microservice` hub.
pub struct BridgeClient {
    runtime: Runtime,
    endpoint: Endpoint,
    addr: EndpointAddr,
    log_tx: mpsc::UnboundedSender<String>,
}

impl BridgeClient {
    /// Connect to the hub identified by `bridge_ticket` (its `EndpointTicket`),
    /// using `secret_key` as openc3-cosmos-app's control identity so the hub can verify
    /// it. Binds a local Iroh endpoint and starts the background log forwarder.
    pub fn connect(secret_key: SecretKey, bridge_ticket: &str) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("building tokio runtime for the bridge client")?;
        let ticket: EndpointTicket = bridge_ticket.parse().context("parsing bridge ticket")?;
        let addr = ticket.endpoint_addr().clone();
        let endpoint = runtime.block_on(async {
            Endpoint::builder(presets::N0)
                .secret_key(secret_key)
                .relay_mode(relay_mode_from_env())
                .bind()
                .await
        })?;

        let (log_tx, log_rx) = mpsc::unbounded_channel::<String>();
        runtime.spawn(log_forwarder(endpoint.clone(), addr.clone(), log_rx));

        Ok(Self {
            runtime,
            endpoint,
            addr,
            log_tx,
        })
    }

    /// Poll the hub for the list of host microservices this bridge should run.
    pub fn fetch_host_microservices(&self) -> Result<Vec<HostSpec>> {
        self.runtime.block_on(async {
            let conn = self
                .endpoint
                .connect(self.addr.clone(), API_HOST_MICROSERVICES)
                .await
                .map_err(|e| anyhow::anyhow!("connect api/host_microservices: {e}"))?;
            let (mut send, mut recv) = conn
                .open_bi()
                .await
                .map_err(|e| anyhow::anyhow!("open_bi: {e}"))?;
            // The client speaks first (the request); this surfaces the hub's
            // accept_bi. The request content is ignored by the hub.
            send.write_all(b"host_microservices")
                .await
                .map_err(|e| anyhow::anyhow!("write request: {e}"))?;
            let _ = send.finish();
            let data = recv
                .read_to_end(MAX_RESPONSE)
                .await
                .map_err(|e| anyhow::anyhow!("read response: {e}"))?;
            let specs: Vec<HostSpec> =
                serde_json::from_slice(&data).context("parsing host_microservices response")?;
            Ok(specs)
        })
    }

    /// Fetch the latest host InterfaceStatus per interface from the hub (which
    /// taps it from the control channel). Returns the status objects; each
    /// carries the microservice `name` used to match it to a host microservice.
    pub fn fetch_interface_status(&self) -> Result<Vec<InterfaceStatus>> {
        let fetch = async {
            let conn = self
                .endpoint
                .connect(self.addr.clone(), API_INTERFACE_STATUS)
                .await
                .map_err(|e| anyhow::anyhow!("connect api/interface_status: {e}"))?;
            let (mut send, mut recv) = conn
                .open_bi()
                .await
                .map_err(|e| anyhow::anyhow!("open_bi: {e}"))?;
            send.write_all(b"interface_status")
                .await
                .map_err(|e| anyhow::anyhow!("write request: {e}"))?;
            let _ = send.finish();
            let data = recv
                .read_to_end(MAX_RESPONSE)
                .await
                .map_err(|e| anyhow::anyhow!("read response: {e}"))?;
            // The hub returns { interface-name: status-object }.
            let map: std::collections::BTreeMap<String, InterfaceStatus> =
                serde_json::from_slice(&data).context("parsing interface_status response")?;
            Ok(map.into_values().collect())
        };
        self.runtime.block_on(async {
            tokio::time::timeout(INTERFACE_STATUS_TIMEOUT, fetch)
                .await
                .map_err(|_| anyhow::anyhow!("api/interface_status timed out"))?
        })
    }

    /// Publish the set of authorized host-microservice public keys to the hub.
    /// The hub only lets these identities onto the `host/<name>` data path.
    /// Sent each operator cycle so the hub's (in-memory) set stays current.
    pub fn authorize(&self, keys: Vec<String>) -> Result<()> {
        self.runtime.block_on(async {
            let conn = self
                .endpoint
                .connect(self.addr.clone(), API_AUTHORIZE)
                .await
                .map_err(|e| anyhow::anyhow!("connect api/authorize: {e}"))?;
            let (mut send, mut recv) = conn
                .open_bi()
                .await
                .map_err(|e| anyhow::anyhow!("open_bi: {e}"))?;
            let request = serde_json::json!({ "keys": keys }).to_string();
            send.write_all(request.as_bytes())
                .await
                .map_err(|e| anyhow::anyhow!("write request: {e}"))?;
            let _ = send.finish();
            // Drain the small ack so the hub sees a clean close.
            let _ = recv.read_to_end(MAX_RESPONSE).await;
            Ok(())
        })
    }

    /// Sync the scope's plugin files. Sends the local manifest (`path → sha256`);
    /// the hub replies with only changed/new files + a deletion list, so
    /// unchanged files are never re-sent. This is how host interfaces get plugin
    /// code (the host has no bucket/gem access).
    pub fn sync_files(&self, have: BTreeMap<String, String>) -> Result<FilesDelta> {
        self.runtime.block_on(async {
            let conn = self
                .endpoint
                .connect(self.addr.clone(), API_FILES)
                .await
                .map_err(|e| anyhow::anyhow!("connect api/files: {e}"))?;
            let (mut send, mut recv) = conn
                .open_bi()
                .await
                .map_err(|e| anyhow::anyhow!("open_bi: {e}"))?;
            let request = serde_json::json!({ "have": have }).to_string();
            send.write_all(request.as_bytes())
                .await
                .map_err(|e| anyhow::anyhow!("write request: {e}"))?;
            let _ = send.finish();
            let data = recv
                .read_to_end(MAX_FILES)
                .await
                .map_err(|e| anyhow::anyhow!("read response: {e}"))?;
            let delta: FilesDelta =
                serde_json::from_slice(&data).context("parsing files response")?;
            Ok(delta)
        })
    }

    /// A cloneable sender for forwarding host stdout lines up to COSMOS.
    pub fn log_sender(&self) -> mpsc::UnboundedSender<String> {
        self.log_tx.clone()
    }

    /// Start answering discovery requests from COSMOS in the background, using
    /// whatever `runner` currently holds. Runs until this client is dropped.
    pub fn start_discovery(&self, runner: DiscoveryRunnerHandle) {
        self.runtime
            .spawn(discovery_worker(self.endpoint.clone(), self.addr.clone(), runner));
    }
}

/// Long-poll the hub for discovery requests and answer each one concurrently.
async fn discovery_worker(endpoint: Endpoint, addr: EndpointAddr, runner: DiscoveryRunnerHandle) {
    loop {
        let requests = match tokio::time::timeout(
            DISCOVERY_POLL_TIMEOUT,
            poll_discovery(&endpoint, &addr),
        )
        .await
        {
            Ok(Ok(requests)) => requests,
            // Hub unreachable, not enrolled yet, or a stalled poll: back off.
            _ => {
                tokio::time::sleep(DISCOVERY_RETRY).await;
                continue;
            }
        };
        for request in requests {
            let endpoint = endpoint.clone();
            let addr = addr.clone();
            let current = runner.lock().ok().and_then(|runner| runner.clone());
            tokio::spawn(async move {
                let result = run_discovery(current, &request.discovery).await;
                if let Err(error) = post_discovery_result(&endpoint, &addr, &request.id, result).await {
                    crate::logging::warn("bridge", &format!("Failed to return discovery result: {error}"));
                }
            });
        }
    }
}

/// One `api/discovery` long-poll: returns pending requests (possibly none).
async fn poll_discovery(endpoint: &Endpoint, addr: &EndpointAddr) -> Result<Vec<DiscoveryRequest>> {
    let conn = endpoint
        .connect(addr.clone(), API_DISCOVERY)
        .await
        .map_err(|e| anyhow::anyhow!("connect api/discovery: {e}"))?;
    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .map_err(|e| anyhow::anyhow!("open_bi: {e}"))?;
    send.write_all(b"discovery")
        .await
        .map_err(|e| anyhow::anyhow!("write request: {e}"))?;
    let _ = send.finish();
    let data = recv
        .read_to_end(MAX_RESPONSE)
        .await
        .map_err(|e| anyhow::anyhow!("read response: {e}"))?;
    serde_json::from_slice(&data).context("parsing discovery response")
}

/// Send one discovery result (`results` list or `error`) back to the hub.
async fn post_discovery_result(
    endpoint: &Endpoint,
    addr: &EndpointAddr,
    id: &str,
    result: Result<serde_json::Value, String>,
) -> Result<()> {
    let request = match result {
        Ok(results) => serde_json::json!({ "id": id, "results": results }),
        Err(error) => serde_json::json!({ "id": id, "error": error }),
    };
    let conn = endpoint
        .connect(addr.clone(), API_DISCOVERY_RESULT)
        .await
        .map_err(|e| anyhow::anyhow!("connect api/discovery_result: {e}"))?;
    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .map_err(|e| anyhow::anyhow!("open_bi: {e}"))?;
    send.write_all(request.to_string().as_bytes())
        .await
        .map_err(|e| anyhow::anyhow!("write request: {e}"))?;
    let _ = send.finish();
    // Drain the small ack so the hub sees a clean close.
    let _ = recv.read_to_end(MAX_RESPONSE).await;
    Ok(())
}

/// Run `python -m openc3.discovery.discovery_runner <discovery...>` and return
/// its `results` list, or a short error for COSMOS to show.
async fn run_discovery(
    runner: Option<DiscoveryRunner>,
    discovery: &[String],
) -> Result<serde_json::Value, String> {
    let Some(runner) = runner else {
        return Err("discovery is not ready on the host yet (its Python environment is still being set up)".to_string());
    };
    if discovery.is_empty() {
        return Err("no discovery class given".to_string());
    }
    let mut cmd = std::process::Command::new(&runner.python);
    cmd.arg("-m")
        .arg(DISCOVERY_RUNNER_MODULE)
        .args(discovery)
        .envs(&runner.env)
        .current_dir(&runner.work_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    crate::process::no_window(&mut cmd);
    let mut cmd = tokio::process::Command::from(cmd);
    cmd.kill_on_drop(true);
    let output = match tokio::time::timeout(DISCOVERY_RUN_TIMEOUT, cmd.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => return Err(format!("failed to run discovery: {error}")),
        Err(_) => return Err(format!("discovery timed out after {}s", DISCOVERY_RUN_TIMEOUT.as_secs())),
    };
    parse_discovery_output(&output.stdout, &output.stderr)
}

/// Interpret the runner's stdout (`{"results": [...]}` or `{"error": "..."}`).
fn parse_discovery_output(stdout: &[u8], stderr: &[u8]) -> Result<serde_json::Value, String> {
    let parsed: Option<serde_json::Value> = String::from_utf8_lossy(stdout)
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .and_then(|line| serde_json::from_str(line).ok());
    match parsed {
        Some(value) if value.get("results").is_some() => Ok(value["results"].clone()),
        Some(value) if value.get("error").is_some() => {
            Err(value["error"].as_str().unwrap_or("discovery failed").to_string())
        }
        // No JSON at all, e.g. Python failed to start or openc3 isn't installed
        _ => {
            let stderr = String::from_utf8_lossy(stderr);
            let last = stderr.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or("no output");
            Err(format!("discovery failed: {last}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_discovery_results() {
        let results = parse_discovery_output(
            b"{\"results\": [{\"value\": \"COM3\", \"text\": \"COM3\"}]}\n",
            b"",
        )
        .unwrap();
        assert_eq!(results, serde_json::json!([{ "value": "COM3", "text": "COM3" }]));
    }

    #[test]
    fn parses_discovery_errors() {
        let error = parse_discovery_output(b"{\"error\": \"RuntimeError: no hidapi\"}\n", b"").unwrap_err();
        assert_eq!(error, "RuntimeError: no hidapi");
    }

    #[test]
    fn reports_stderr_when_no_json() {
        let error = parse_discovery_output(
            b"",
            b"Traceback (most recent call last):\nModuleNotFoundError: No module named 'openc3'\n",
        )
        .unwrap_err();
        assert_eq!(error, "discovery failed: ModuleNotFoundError: No module named 'openc3'");
    }

    #[test]
    fn discovery_not_ready_without_runner() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let error = runtime
            .block_on(run_discovery(None, &["openc3/discovery/serial_discovery.py".to_string()]))
            .unwrap_err();
        assert!(error.contains("not ready"));
    }
}

/// Maintain an `api/log` connection and forward queued log lines to the hub,
/// reconnecting on failure. Log delivery is best effort: a line may be dropped
/// when a connection is being (re)established.
async fn log_forwarder(
    endpoint: Endpoint,
    addr: EndpointAddr,
    mut rx: mpsc::UnboundedReceiver<String>,
) {
    // Block until there is a line to send, then (re)establish the connection.
    while let Some(mut line) = rx.recv().await {
        let Ok(conn) = endpoint.connect(addr.clone(), API_LOG).await else {
            continue; // couldn't connect; retry when the next line arrives
        };
        let Ok((mut send, _recv)) = conn.open_bi().await else {
            continue;
        };
        loop {
            let mut buf = line.into_bytes();
            buf.push(b'\n');
            if send.write_all(&buf).await.is_err() {
                break; // connection dropped; reconnect on the next line
            }
            match rx.recv().await {
                Some(next) => line = next,
                None => {
                    let _ = send.finish();
                    return; // channel closed: app shutting down
                }
            }
        }
    }
}
