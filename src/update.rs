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

//! Self-update: periodically check GitHub Releases for a newer version of this
//! app and, when the user asks, download + launch the platform installer.
//!
//! A background thread checks on startup and every 8 hours, publishing the
//! latest release into a shared slot the GUI reads. The GUI decides whether to
//! prompt (newer than the running build, and not a version the user skipped).

use anyhow::{bail, Result};
use serde::Deserialize;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::process;

/// `owner/repo` to query. Overridable via `OPENC3_COSMOS_APP_GITHUB_REPO` for forks or
/// testing. Release tags are `v<semver>`.
const DEFAULT_REPO: &str = "OpenC3/openc3-cosmos-app";
/// Re-check cadence after the initial startup check.
const CHECK_INTERVAL: Duration = Duration::from_secs(8 * 60 * 60);

fn repo() -> String {
    std::env::var("OPENC3_COSMOS_APP_GITHUB_REPO")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_REPO.to_string())
}

/// This build's version (from Cargo.toml).
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// A downloadable installer asset attached to a release.
#[derive(Debug, Clone)]
pub struct Asset {
    pub name: String,
    pub url: String,
}

/// The latest published release, normalized.
#[derive(Debug, Clone)]
pub struct Release {
    /// Semver-ish version with the tag prefix stripped (e.g. "0.2.0").
    pub version: String,
    /// The release page URL (used as the "install" fallback).
    pub url: String,
    pub assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct GhRelease {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    #[serde(default)]
    name: String,
    #[serde(default)]
    browser_download_url: String,
}

/// Query GitHub for the latest (non-prerelease, non-draft) release. Returns the
/// normalized `Release`; any network/parse failure is an error the caller treats
/// as "no update info this cycle".
pub fn latest_release() -> Result<Release> {
    let url = format!("https://api.github.com/repos/{}/releases/latest", repo());
    let body = fetch(&url)?;
    let gh: GhRelease = serde_json::from_slice(&body)?;
    let version = strip_tag(&gh.tag_name);
    if version.is_empty() {
        bail!("release '{}' has no usable version", gh.tag_name);
    }
    Ok(Release {
        version,
        url: gh.html_url,
        assets: gh
            .assets
            .into_iter()
            .map(|a| Asset {
                name: a.name,
                url: a.browser_download_url,
            })
            .collect(),
    })
}

/// GET a URL with curl (GitHub's API requires a User-Agent). Windowless.
fn fetch(url: &str) -> Result<Vec<u8>> {
    let mut cmd = Command::new("curl");
    cmd.args([
        "-fsSL",
        "--max-time",
        "20",
        "-H",
        "Accept: application/vnd.github+json",
        "-H",
        "User-Agent: openc3-cosmos-app",
        url,
    ]);
    let out = process::capture(&mut cmd)?;
    if !out.status.success() {
        bail!("update check failed ({})", out.status);
    }
    Ok(out.stdout)
}

/// Normalize a release tag to a bare version. Our tags (and the COSMOS project
/// tags) are `v<ver>`; a plain `<ver>` is also accepted.
fn strip_tag(tag: &str) -> String {
    let t = tag.trim();
    t.strip_prefix(['v', 'V']).unwrap_or(t).to_string()
}

/// True if `latest` is a newer X.Y.Z than `current`.
pub fn is_newer(latest: &str, current: &str) -> bool {
    triple(latest) > triple(current)
}

/// True when `version` is at least `minimum`, comparing the numeric X.Y.Z core.
pub fn is_at_least(version: &str, minimum: &str) -> bool {
    triple(version) >= triple(minimum)
}

fn triple(v: &str) -> (u64, u64, u64) {
    let core = v.split(['-', '+']).next().unwrap_or(v);
    let mut it = core.split('.').map(|p| p.trim().parse::<u64>().unwrap_or(0));
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    )
}

/// Check now, then every [`CHECK_INTERVAL`], storing the latest release in
/// `shared` for the GUI to consume. Best-effort — failures log at debug and are
/// retried next cycle.
pub fn spawn_checker(shared: Arc<Mutex<Option<Release>>>) {
    std::thread::spawn(move || loop {
        match latest_release() {
            Ok(rel) => {
                if let Ok(mut slot) = shared.lock() {
                    *slot = Some(rel);
                }
            }
            Err(e) => crate::logging::debug("update", &format!("release check failed: {e:#}")),
        }
        std::thread::sleep(CHECK_INTERVAL);
    });
}

/// Result of a manual "check for updates now" request (app + COSMOS).
pub enum CheckOutcome {
    /// A newer version of this app is available.
    AppUpdate(Release),
    /// A newer COSMOS version is available.
    CosmosUpdate(CosmosRelease),
    /// Everything is up to date.
    UpToDate,
    /// The check failed (offline, rate-limited, etc.); carries a short reason.
    Failed(String),
}

/// Run a check immediately (app first, then COSMOS) and classify it. Unlike the
/// periodic checkers this always reports a result (for user feedback) and ignores
/// the skipped-version preferences (the user explicitly asked).
pub fn check_now(cosmos_env: &std::path::Path, enterprise: bool, token: &str) -> CheckOutcome {
    match latest_release() {
        Ok(rel) if is_newer(&rel.version, current_version()) => {
            return CheckOutcome::AppUpdate(rel)
        }
        Ok(_) => {}
        Err(e) => return CheckOutcome::Failed(format!("{e:#}")),
    }
    match cosmos_check(cosmos_env, enterprise, token) {
        Some(c) => CheckOutcome::CosmosUpdate(c),
        None => CheckOutcome::UpToDate,
    }
}

// ---------------------------------------------------------------------------
// COSMOS itself (cosmos-project / cosmos-enterprise-project tags)
// ---------------------------------------------------------------------------

/// A newer COSMOS release available on the appropriate project repo.
#[derive(Debug, Clone)]
pub struct CosmosRelease {
    /// Raw git tag to download (e.g. "v7.3.0").
    pub tag: String,
    /// Normalized version for display/compare (e.g. "7.3.0").
    pub version: String,
}

const COSMOS_TAGS_API: &str = "https://api.github.com/repos/OpenC3/cosmos-project/tags";
const COSMOS_ENT_TAGS_API: &str =
    "https://repos.openc3.com/api/v1/repos/OpenC3/cosmos-enterprise-project/tags";

#[derive(Deserialize)]
struct GhTag {
    #[serde(default)]
    name: String,
}

/// The highest-semver tag on cosmos-project (Core) or cosmos-enterprise-project
/// (Enterprise — private Forgejo repo, needs the access token).
pub fn cosmos_latest(enterprise: bool, token: &str) -> Result<CosmosRelease> {
    let body = if enterprise {
        if token.trim().is_empty() {
            bail!("COSMOS Enterprise update check needs a repos.openc3.com access token");
        }
        crate::download::to_bytes_auth(COSMOS_ENT_TAGS_API, token)?
    } else {
        fetch(COSMOS_TAGS_API)?
    };
    let tags: Vec<GhTag> = serde_json::from_slice(&body)?;
    tags.into_iter()
        .map(|t| t.name)
        .filter(|n| !n.is_empty())
        .map(|tag| {
            let version = strip_tag(&tag);
            (tag, version)
        })
        .filter(|(_, v)| !v.is_empty())
        .max_by(|a, b| triple(&a.1).cmp(&triple(&b.1)))
        .map(|(tag, version)| CosmosRelease { tag, version })
        .ok_or_else(|| anyhow::anyhow!("no tags found on the COSMOS project repo"))
}

/// The installed COSMOS version, from `OPENC3_TAG` in the cosmos `.env`.
pub fn installed_cosmos_version(env_path: &std::path::Path) -> Option<String> {
    let map = crate::env_file::parse(env_path).ok()?;
    map.get("OPENC3_TAG")
        .map(|v| strip_tag(v))
        .filter(|v| !v.is_empty())
}

/// A COSMOS release newer than what's installed, or None (not installed, up to
/// date, or the check failed).
pub fn cosmos_check(env_path: &std::path::Path, enterprise: bool, token: &str) -> Option<CosmosRelease> {
    let installed = installed_cosmos_version(env_path)?;
    let latest = cosmos_latest(enterprise, token).ok()?;
    if is_newer(&latest.version, &installed) {
        Some(latest)
    } else {
        None
    }
}

/// Periodic COSMOS update checker (startup + every 8h), storing a newer release
/// in `shared`. Cycles are no-ops when COSMOS isn't installed or in dev mode
/// (which runs "latest" from source). `dev_mode` is read live each cycle so
/// toggling Development Mode takes effect without a restart.
pub fn spawn_cosmos_checker(
    shared: Arc<Mutex<Option<CosmosRelease>>>,
    env_path: std::path::PathBuf,
    enterprise: bool,
    token: String,
    dev_mode: Arc<AtomicBool>,
) {
    std::thread::spawn(move || loop {
        if !dev_mode.load(Ordering::Relaxed) && env_path.exists() {
            if let Some(rel) = cosmos_check(&env_path, enterprise, &token) {
                if let Ok(mut slot) = shared.lock() {
                    *slot = Some(rel);
                }
            }
        }
        std::thread::sleep(CHECK_INTERVAL);
    });
}

/// Run a single COSMOS check off-thread and publish any newer release into
/// `shared`. Used to recheck immediately when the user switches out of
/// Development Mode (rather than waiting for the next periodic cycle).
pub fn spawn_cosmos_check_once(
    shared: Arc<Mutex<Option<CosmosRelease>>>,
    env_path: std::path::PathBuf,
    enterprise: bool,
    token: String,
) {
    std::thread::spawn(move || {
        if env_path.exists() {
            if let Some(rel) = cosmos_check(&env_path, enterprise, &token) {
                if let Ok(mut slot) = shared.lock() {
                    *slot = Some(rel);
                }
            }
        }
    });
}

/// Install a release: download the installer matching this platform/arch and
/// launch it (the OS installer takes over — a running app can't cleanly replace
/// itself). If no asset matches, open the release page so the user can pick one.
pub fn install_release(rel: &Release) -> Result<()> {
    match select_asset(&rel.assets) {
        Some(asset) => {
            let dest = std::env::temp_dir().join(&asset.name);
            crate::install::progress(format!("Downloading {}…", asset.name));
            crate::download::to_file(&asset.url, &dest)?;
            crate::install::progress("Launching the installer…");
            launch_installer(&dest)?;
            crate::install::progress(
                "Installer launched. Complete it, then restart OpenC3 COSMOS to run the new version.",
            );
            Ok(())
        }
        None => {
            crate::install::progress("No matching installer for this platform; opening the release page…");
            crate::commands::open_browser(&rel.url)
        }
    }
}

/// Pick the asset for this platform+arch by filename extension + arch keyword.
/// Best-effort — names come from cargo-packager (e.g. `..._aarch64.dmg`).
fn select_asset(assets: &[Asset]) -> Option<&Asset> {
    let arch: &[&str] = if cfg!(target_arch = "aarch64") {
        &["aarch64", "arm64"]
    } else {
        &["x86_64", "amd64", "x64"]
    };
    // Extensions in preference order for this OS.
    let exts: &[&str] = if cfg!(target_os = "macos") {
        &[".dmg"]
    } else if cfg!(target_os = "windows") {
        &[".msi"]
    } else {
        &[".deb", ".appimage", ".tar.gz"]
    };
    let windows = cfg!(target_os = "windows");
    for ext in exts {
        if let Some(a) = assets.iter().find(|a| {
            let n = a.name.to_lowercase();
            // Windows only ships one arch (x86_64), so don't require an arch tag.
            n.ends_with(ext) && (windows || arch.iter().any(|k| n.contains(k)))
        }) {
            return Some(a);
        }
    }
    None
}

/// Launch a downloaded installer with the platform's default handler.
fn launch_installer(path: &std::path::Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("open"); // mounts the .dmg
        c.arg(path);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        // `start "" <file>` runs the file's associated installer (msiexec for .msi).
        let mut c = Command::new("cmd");
        c.args(["/C", "start", ""]).arg(path);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = Command::new("xdg-open"); // e.g. opens the .deb in the software center
        c.arg(path);
        c
    };
    process::run(&mut cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        // Prerelease/build suffixes are ignored (we compare the X.Y.Z core).
        assert!(is_newer("0.2.0-rc1", "0.1.0"));
        assert!(!is_at_least("7.3.9", "7.4.0"));
        assert!(is_at_least("7.4.0", "7.4.0"));
        assert!(is_at_least("7.4.1", "7.4.0"));
        assert!(is_at_least("8.0.0", "7.4.0"));
    }

    #[test]
    fn tag_prefix_stripping() {
        assert_eq!(strip_tag("v0.2.0"), "0.2.0");
        assert_eq!(strip_tag("V1.2.3"), "1.2.3");
        assert_eq!(strip_tag("v7.3.0"), "7.3.0");
        assert_eq!(strip_tag("3.4.5"), "3.4.5");
        assert_eq!(strip_tag(""), "");
    }
}
