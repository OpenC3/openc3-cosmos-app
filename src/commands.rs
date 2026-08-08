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

//! High-level command implementations corresponding to the CLI subcommands.
//! Together these reproduce the functionality of `openc3.sh`.

use crate::context::Context;
use crate::{docker, env_file, install, monitor, process};
use anyhow::{bail, Context as _, Result};
use std::io::Write;
use std::path::Path;
use std::process::Command;

/// In development mode, run the source checkout's `openc3.sh` (or `openc3.bat`
/// on Windows) with `subcommand`, from that directory. OPENC3_TAG /
/// OPENC3_ENTERPRISE_TAG overrides are inherited from the process environment
/// (the GUI sets them when dev mode is on).
fn run_dev(dev: &Path, subcommand: &str, args: &[String]) -> Result<()> {
    let (script_name, launcher): (&str, Option<&str>) = if cfg!(windows) {
        ("openc3.bat", None)
    } else {
        ("openc3.sh", Some("bash"))
    };
    let script = dev.join(script_name);
    if !script.exists() {
        bail!(
            "Development folder '{}' does not contain {}.",
            dev.display(),
            script_name
        );
    }
    let mut cmd = match launcher {
        Some(sh) => {
            let mut c = Command::new(sh);
            c.arg(&script);
            c
        }
        None => Command::new(&script),
    };
    cmd.arg(subcommand);
    for a in args {
        cmd.arg(a);
    }
    cmd.current_dir(dev);
    process::run(&mut cmd)
}

/// `build` — build all COSMOS containers from source (dev installs only).
pub fn build(ctx: &Context, flags: &[String]) -> Result<()> {
    if let Some(dev) = &ctx.dev_folder {
        return run_dev(dev, "build", flags);
    }
    if !ctx.paths.is_devel() {
        bail!("'build' is only available for development installs (no compose-build.yaml found).");
    }
    install::setup_cosmos(ctx)?;
    let mut cmd = docker::compose_with_build(ctx)?;
    cmd.arg("build");
    for f in flags {
        cmd.arg(f);
    }
    docker::run(cmd)
}

/// `run` — start the containers detached.
pub fn run(ctx: &Context) -> Result<()> {
    if let Some(dev) = &ctx.dev_folder {
        return run_dev(dev, "run", &[]);
    }
    check_not_root();
    install::setup_cosmos(ctx)?;
    // On first run Docker downloads all the container images, which can take
    // several minutes. Stream compose's progress through the notifier so the GUI
    // (and CLI) show what's happening instead of appearing hung.
    install::progress("Starting COSMOS containers (first run downloads images — this can take several minutes)…");
    docker::up(ctx, |line| install::progress(line))?;
    install::progress("COSMOS is starting. Access it at http://localhost:2900");
    Ok(())
}

/// `start` — build (dev) then run.
pub fn start(ctx: &Context, flags: &[String]) -> Result<()> {
    if let Some(dev) = &ctx.dev_folder {
        return run_dev(dev, "start", flags);
    }
    if ctx.paths.is_devel() {
        build(ctx, flags)?;
    }
    run(ctx)
}

/// `stop` — gracefully stop and tear down.
pub fn stop(ctx: &Context) -> Result<()> {
    if let Some(dev) = &ctx.dev_folder {
        return run_dev(dev, "stop", &[]);
    }
    docker::stop(ctx)
}

/// `restart` — stop then run.
pub fn restart(ctx: &Context) -> Result<()> {
    stop(ctx)?;
    run(ctx)
}

/// Local files that hold the user's config/secrets (COSMOS 7.3.0+ convention).
/// They live alongside the replaceable scaffolding and must survive an upgrade.
const PRESERVE_ON_UPGRADE: [&str; 2] = [".env.local", "compose.override.yaml"];

/// Upgrade the COSMOS install to `tag`: stop COSMOS, replace the cosmos folder
/// with the new version, then restart. The user's `.env.local` and
/// `compose.override.yaml` (config + secrets) are carried across the replace;
/// telemetry/config data lives in Docker volumes and is untouched.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn upgrade_cosmos(ctx: &Context, tag: &str, enterprise: bool, token: &str) -> Result<()> {
    let cosmos = &ctx.paths.cosmos;
    install::progress(format!("Upgrading COSMOS to {tag}…"));

    // Read the preserved files into memory before we wipe the folder.
    let preserved: Vec<(String, Vec<u8>)> = PRESERVE_ON_UPGRADE
        .iter()
        .filter_map(|name| std::fs::read(cosmos.join(name)).ok().map(|b| (name.to_string(), b)))
        .collect();

    install::progress("Stopping COSMOS…");
    let _ = stop(ctx); // best-effort — it may already be stopped

    install::progress("Replacing the COSMOS install with the new version…");
    std::fs::remove_dir_all(cosmos)
        .with_context(|| format!("removing the old COSMOS install at {}", cosmos.display()))?;
    // With the folder gone, install::cosmos downloads and extracts `tag` fresh.
    install::cosmos(ctx, tag, enterprise, token)?;

    // Restore the user's config/secrets over the fresh scaffolding.
    for (name, bytes) in &preserved {
        std::fs::write(cosmos.join(name), bytes)
            .with_context(|| format!("restoring {name}"))?;
        install::progress(format!("Preserved {name}."));
    }

    install::progress("Starting the upgraded COSMOS…");
    run(ctx)?;
    install::progress(format!("COSMOS upgraded to {tag}. It is restarting."));
    Ok(())
}

/// Open `url` in the host's default web browser.
#[cfg(feature = "gui")]
pub fn open_browser(url: &str) -> Result<()> {
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(url);
        c
    } else if cfg!(target_os = "windows") {
        // `start` is a cmd builtin; the empty "" is the window title argument.
        let mut c = Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    process::run(&mut cmd)
}

/// `status` — show a color-coded-style container status table.
pub fn status(ctx: &Context) -> Result<()> {
    use crate::monitor::RunState;
    match monitor::snapshot(ctx, true) {
        Ok(statuses) => {
            let running = statuses.iter().filter(|c| c.is_running()).count();
            println!("{} of {} containers running", running, statuses.len());
            for c in &statuses {
                let glyph = match c.run_state() {
                    RunState::Running => "●",
                    RunState::ExitedSuccess => "○",
                    RunState::ExitedFailure => "○",
                    RunState::Restarting => "↻",
                    RunState::Paused => "❚",
                    RunState::Unknown => "?",
                };
                println!(
                    "  {glyph} {:<34} {:<14} {:<24} {:>7} {:>12}  {}",
                    c.service,
                    c.tag_display(),
                    c.display_status(),
                    c.cpu_display(),
                    c.mem_display(),
                    c.ports_summary()
                );
            }
            if !statuses.is_empty() {
                let (cpu_total, mem_total) = monitor::totals(&statuses);
                let (label, blank) = ("Total", "");
                println!(
                    "    {label:<34} {blank:<14} {blank:<24} {cpu_total:>7} {mem_total:>12}"
                );
            }
            Ok(())
        }
        // Fall back to a raw `ps` if JSON parsing isn't available.
        Err(_) => docker::ps(ctx),
    }
}

/// `logs` — show (optionally follow) container logs.
pub fn logs(ctx: &Context, service: Option<&str>, follow: bool) -> Result<()> {
    docker::logs(ctx, service, follow)
}

/// `monitor` — headless loop printing health every few seconds.
pub fn monitor_loop(ctx: &Context) -> Result<()> {
    println!("Monitoring COSMOS containers (Ctrl-C to stop)...");
    loop {
        // Health only — no per-container CPU/mem stats needed here.
        match monitor::snapshot(ctx, false) {
            Ok(statuses) => {
                let unhealthy: Vec<_> = statuses.iter().filter(|c| !c.is_healthy()).collect();
                let summary = monitor::summarize(&statuses);
                if unhealthy.is_empty() {
                    println!("[ok] {summary}");
                } else {
                    let names: Vec<&str> =
                        unhealthy.iter().map(|c| c.service.as_str()).collect();
                    println!("[warn] {summary} — unhealthy: {}", names.join(", "));
                }
            }
            Err(e) => println!("[error] {e}"),
        }
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
}

/// `cleanup` — remove docker volumes and (optionally) local plugins.
pub fn cleanup(ctx: &Context, local: bool, force: bool) -> Result<()> {
    if !force {
        print!("Are you sure? Cleanup removes ALL docker volumes and COSMOS data! [y/N] ");
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).ok();
        if !answer.trim().eq_ignore_ascii_case("y") {
            println!("Aborted.");
            return Ok(());
        }
    }
    if let Some(dev) = &ctx.dev_folder {
        // Already confirmed above; pass `force` so openc3.sh doesn't re-prompt.
        let mut args = Vec::new();
        if local {
            args.push("local".to_string());
        }
        args.push("force".to_string());
        return run_dev(dev, "cleanup", &args);
    }
    docker::down_volumes(ctx)?;
    if local {
        let default_dir = ctx.paths.cosmos.join("plugins").join("DEFAULT");
        if default_dir.is_dir() {
            for entry in std::fs::read_dir(&default_dir)? {
                let entry = entry?;
                if entry.file_name() == "README.md" {
                    continue;
                }
                let path = entry.path();
                if path.is_dir() {
                    std::fs::remove_dir_all(&path).ok();
                } else {
                    std::fs::remove_file(&path).ok();
                }
            }
        }
    }
    Ok(())
}

/// `cli` / `cliroot` — run the Ruby CLI inside a one-off container.
pub fn cli(ctx: &Context, args: &[String], as_root: bool) -> Result<()> {
    let env = env_file::parse(&ctx.paths.env_file())
        .with_context(|| "reading the COSMOS .env file (is COSMOS installed?)")?;
    let cwd = std::env::current_dir()?;

    let mut cmd = docker::compose(ctx)?;
    cmd.arg("run").arg("-it").arg("--rm");
    if as_root {
        cmd.arg("--user=root");
    }
    cmd.arg("-v").arg(format!("{}:/openc3/local:z", cwd.display()));
    cmd.arg("-w").arg("/openc3/local");
    if ctx.enterprise {
        if let Some(user) = env.get("OPENC3_API_USER") {
            cmd.arg("-e").arg(format!("OPENC3_API_USER={user}"));
        }
    }
    if let Some(pw) = env.get("OPENC3_API_PASSWORD") {
        cmd.arg("-e").arg(format!("OPENC3_API_PASSWORD={pw}"));
    }
    cmd.arg("--no-deps")
        .arg("openc3-cosmos-cmd-tlm-api")
        .arg("ruby")
        .arg("/openc3/bin/openc3cli");
    for a in args {
        cmd.arg(a);
    }
    docker::run(cmd)
}

/// `test` — build then run a test suite (development installs only).
pub fn test(ctx: &Context, args: &[String]) -> Result<()> {
    if !ctx.paths.is_devel() {
        bail!("'test' requires a development install with the COSMOS source tree.");
    }
    install::setup_cosmos(ctx)?;
    let mut build = docker::compose_with_build(ctx)?;
    build.arg("build");
    docker::run(build)?;

    // Delegate to the repository's test script when present.
    let script = ctx
        .paths
        .cosmos
        .join("scripts")
        .join("linux")
        .join("openc3_test.sh");
    if script.exists() {
        let mut cmd = Command::new("bash");
        cmd.arg(&script).args(args).current_dir(&ctx.paths.cosmos);
        process::run(&mut cmd)
    } else {
        bail!(
            "test script not found at {}. Available in a full source checkout only.",
            script.display()
        );
    }
}

fn check_not_root() {
    #[cfg(unix)]
    {
        let is_root = std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim() == "0")
            .unwrap_or(false);
        if is_root {
            eprintln!(
                "WARNING: COSMOS should not be run as root; Local Mode file permissions will be affected."
            );
        }
    }
}
