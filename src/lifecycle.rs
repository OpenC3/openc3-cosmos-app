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

//! Turn OS termination requests into the app's graceful shutdown, so host
//! microservices are stopped cleanly instead of being orphaned.
//!
//! * **Unix:** SIGTERM, SIGINT and SIGHUP (e.g. `kill <pid>`, Ctrl-C, the
//!   terminal or session closing) set a flag instead of killing the process.
//!   A second signal of the same kind exits immediately, so a stuck shutdown
//!   can still be forced.
//! * **Windows:** console control events (Ctrl-C/Ctrl-Break, closing the
//!   console, logoff, shutdown) set the flag. For close/logoff/shutdown the OS
//!   terminates us as soon as the handler returns, so the handler waits
//!   (bounded) for [`cleanup_finished`].
//!
//! The front-end polls [`requested`] and runs its normal shutdown, then calls
//! [`cleanup_finished`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
#[cfg(windows)]
use std::time::Duration;

fn flag() -> &'static Arc<AtomicBool> {
    static FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();
    FLAG.get_or_init(|| Arc::new(AtomicBool::new(false)))
}

fn finished() -> &'static (Mutex<bool>, Condvar) {
    static DONE: OnceLock<(Mutex<bool>, Condvar)> = OnceLock::new();
    DONE.get_or_init(|| (Mutex::new(false), Condvar::new()))
}

/// Install the termination handlers. Idempotent; best effort (a failure just
/// leaves the default "terminate now" behavior).
pub fn install() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        #[cfg(unix)]
        {
            use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
            for signal in [SIGTERM, SIGINT, SIGHUP] {
                // Order matters: the conditional shutdown sees the flag set by
                // a *previous* signal, so the second one exits.
                let _ = signal_hook::flag::register_conditional_shutdown(signal, 1, flag().clone());
                let _ = signal_hook::flag::register(signal, flag().clone());
            }
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
            // SAFETY: registering a valid `extern "system"` handler.
            unsafe {
                SetConsoleCtrlHandler(Some(console_handler), 1);
            }
        }
    });
}

/// Whether the OS asked us to terminate.
pub fn requested() -> bool {
    flag().load(Ordering::Relaxed)
}

/// Report that the graceful shutdown is complete (host microservices stopped),
/// releasing a waiting Windows console handler.
pub fn cleanup_finished() {
    let (lock, cvar) = finished();
    if let Ok(mut done) = lock.lock() {
        *done = true;
        cvar.notify_all();
    }
}

#[cfg(windows)]
unsafe extern "system" fn console_handler(ctrl_type: u32) -> windows_sys::core::BOOL {
    use windows_sys::Win32::System::Console::{CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT};
    flag().store(true, Ordering::Relaxed);
    if matches!(ctrl_type, CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT) {
        // The process is killed when we return (or after ~5s regardless), so
        // give the shutdown as much of that window as we can.
        let (lock, cvar) = finished();
        if let Ok(done) = lock.lock() {
            let _ = cvar.wait_timeout_while(done, Duration::from_millis(4500), |done| !*done);
        }
    }
    1 // handled: don't run the default handler (which exits immediately)
}
