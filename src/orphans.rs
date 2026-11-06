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

//! Last-line cleanup of host microservices that outlived the app run that
//! started them. [`crate::child_guard`] normally prevents that, but a child can
//! still survive, e.g. one stuck before its stdin watcher starts on macOS, or
//! one started by an older app version without the guards.
//!
//! Each operator records the PIDs of its live children in
//! `<microservices>/.owners/<app pid>`. When an operator starts, [`sweep`]
//! finds host-runner processes for *this* install (their command line names
//! the host runner module and this install's microservices directory) and
//! stops any not owned by a still-running instance. Checking owners keeps a
//! concurrently running instance (e.g. the GUI plus `openc3 microservices`
//! against the same root) from killing the other's children.
//!
//! Everything here blocks (subprocesses, file I/O): operator thread only.

use crate::operator::HOST_INTERFACE_MODULE;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::time::{Duration, Instant};

const OWNERS_DIR: &str = ".owners";
/// How long orphans get to exit after a graceful signal before a hard kill.
#[cfg(unix)]
const ORPHAN_GRACE: Duration = Duration::from_secs(3);

/// The live child PIDs of this process's operator, persisted for [`sweep`].
pub struct ChildRegistry {
    path: PathBuf,
    pids: BTreeSet<u32>,
}

impl ChildRegistry {
    pub fn new(microservices_dir: &Path) -> Self {
        Self {
            path: microservices_dir.join(OWNERS_DIR).join(std::process::id().to_string()),
            pids: BTreeSet::new(),
        }
    }

    pub fn add(&mut self, pid: u32) {
        if self.pids.insert(pid) {
            self.save();
        }
    }

    /// Replace the recorded set with exactly `pids` (the currently live children).
    pub fn set(&mut self, pids: BTreeSet<u32>) {
        if pids != self.pids {
            self.pids = pids;
            self.save();
        }
    }

    fn save(&self) {
        if self.pids.is_empty() {
            let _ = std::fs::remove_file(&self.path);
            return;
        }
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let body: String = self.pids.iter().map(|pid| format!("{pid}\n")).collect();
        // Write-then-rename so a concurrent sweep never reads a partial file.
        let tmp = self.path.with_extension("tmp");
        if std::fs::write(&tmp, body).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

/// A process that looks like one of our host runners.
#[derive(Debug, Clone, PartialEq)]
struct Candidate {
    pid: u32,
    ppid: u32,
}

/// Stop host-runner processes for this install that no live instance owns.
pub fn sweep(microservices_dir: &Path) {
    let me = std::process::id();
    let protected = live_owned_pids(microservices_dir, me);
    let candidates = host_processes(microservices_dir);
    let orphans = orphans(&candidates, &protected, me);
    if orphans.is_empty() {
        return;
    }
    let list: Vec<String> = orphans.iter().map(u32::to_string).collect();
    crate::logging::warn(
        "MicroserviceOperator",
        &format!(
            "Stopping {} host microservice process(es) left running by a previous run: {}",
            orphans.len(),
            list.join(", ")
        ),
    );
    terminate(&orphans);
}

/// Candidates not owned by a live instance (a child of an owned process, such
/// as the interpreter behind a Windows venv launcher, counts as owned).
fn orphans(candidates: &[Candidate], protected: &BTreeSet<u32>, me: u32) -> Vec<u32> {
    candidates
        .iter()
        .filter(|c| c.pid != me && !protected.contains(&c.pid) && !protected.contains(&c.ppid))
        .map(|c| c.pid)
        .collect()
}

/// PIDs registered by other, still-running instances. Registry files of dead
/// owners are removed; so is our own (left by a previous operator in this
/// process, which stops its children before exiting, so anything it lists that
/// is still alive is a stray).
fn live_owned_pids(microservices_dir: &Path, me: u32) -> BTreeSet<u32> {
    let mut protected = BTreeSet::new();
    let Ok(entries) = std::fs::read_dir(microservices_dir.join(OWNERS_DIR)) else {
        return protected;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(owner) = path.file_name().and_then(|n| n.to_str()).and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        if owner != me && process_alive(owner) {
            if let Ok(body) = std::fs::read_to_string(&path) {
                protected.extend(body.lines().filter_map(|l| l.trim().parse::<u32>().ok()));
            }
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
    protected
}

/// Host-runner processes belonging to this install. A process is ours when its
/// command line names the host runner module and our microservices directory
/// (the venv interpreter lives under it), or when its parent is such a process
/// (Windows: the base interpreter started by the venv `python.exe` launcher).
fn host_processes(microservices_dir: &Path) -> Vec<Candidate> {
    let dir = microservices_dir.to_string_lossy().to_string();
    let listed = list_processes(HOST_INTERFACE_MODULE);
    let direct: BTreeSet<u32> = listed
        .iter()
        .filter(|(_, _, cmd)| cmd.contains(HOST_INTERFACE_MODULE) && cmd.contains(&dir))
        .map(|(pid, _, _)| *pid)
        .collect();
    listed
        .into_iter()
        .filter(|(pid, ppid, cmd)| {
            cmd.contains(HOST_INTERFACE_MODULE) && (direct.contains(pid) || direct.contains(ppid))
        })
        .map(|(pid, ppid, _)| Candidate { pid, ppid })
        .collect()
}

/// `(pid, ppid, command line)` for processes whose command line mentions `needle`.
#[cfg(unix)]
fn list_processes(needle: &str) -> Vec<(u32, u32, String)> {
    let mut cmd = std::process::Command::new("ps");
    cmd.args(["-axww", "-o", "pid=", "-o", "ppid=", "-o", "command="]);
    let Ok(out) = crate::process::capture(&mut cmd) else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| line.contains(needle))
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid = parts.next()?.parse().ok()?;
            let ppid = parts.next()?.parse().ok()?;
            let command = parts.collect::<Vec<_>>().join(" ");
            Some((pid, ppid, command))
        })
        .collect()
}

#[cfg(windows)]
fn list_processes(needle: &str) -> Vec<(u32, u32, String)> {
    // WQL LIKE: `_` is a single-character wildcard, which still matches itself.
    let script = format!(
        "Get-CimInstance Win32_Process -Filter \"CommandLine like '%{needle}%'\" | \
         ForEach-Object {{ \"$($_.ProcessId)`t$($_.ParentProcessId)`t$($_.CommandLine)\" }}"
    );
    let mut cmd = std::process::Command::new("powershell");
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
    let Ok(out) = crate::process::capture(&mut cmd) else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let pid = parts.next()?.trim().parse().ok()?;
            let ppid = parts.next()?.trim().parse().ok()?;
            Some((pid, ppid, parts.next()?.to_string()))
        })
        .collect()
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks for existence/permission.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: plain Win32 calls; the handle is closed before returning.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(handle, &mut code);
        CloseHandle(handle);
        ok != 0 && code == STILL_ACTIVE as u32
    }
}

/// SIGTERM (the runner shuts down gracefully), then SIGKILL whatever remains.
#[cfg(unix)]
fn terminate(pids: &[u32]) {
    for pid in pids {
        // SAFETY: sending a signal to a pid we just matched.
        unsafe {
            libc::kill(*pid as libc::pid_t, libc::SIGTERM);
        }
    }
    let start = Instant::now();
    let mut remaining: Vec<u32> = pids.to_vec();
    while !remaining.is_empty() && start.elapsed() < ORPHAN_GRACE {
        std::thread::sleep(Duration::from_millis(100));
        remaining.retain(|pid| process_alive(*pid));
    }
    for pid in remaining {
        // SAFETY: as above.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
    }
}

/// Kill each orphan and its descendants. (Their stdin lifeline belonged to the
/// dead app, so there's no graceful route left on Windows.)
#[cfg(windows)]
fn terminate(pids: &[u32]) {
    for pid in pids {
        let mut cmd = std::process::Command::new("taskkill");
        cmd.args(["/PID", &pid.to_string(), "/T", "/F"]);
        let _ = crate::process::capture(&mut cmd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_processes_and_their_children_are_spared() {
        let candidates = vec![
            Candidate { pid: 10, ppid: 1 },  // orphan
            Candidate { pid: 20, ppid: 5 },  // owned by a live instance
            Candidate { pid: 21, ppid: 20 }, // interpreter under an owned launcher
            Candidate { pid: 99, ppid: 1 },  // ourselves (never)
        ];
        let protected: BTreeSet<u32> = [20].into_iter().collect();
        assert_eq!(orphans(&candidates, &protected, 99), vec![10]);
    }

    #[test]
    fn registry_file_tracks_live_children() {
        let dir = std::env::temp_dir().join(format!("openc3-orphans-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut registry = ChildRegistry::new(&dir);
        registry.add(111);
        registry.add(222);
        let path = dir.join(OWNERS_DIR).join(std::process::id().to_string());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "111\n222\n");
        registry.set([222].into_iter().collect());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "222\n");
        registry.set(BTreeSet::new());
        assert!(!path.exists(), "an empty registry removes its file");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
