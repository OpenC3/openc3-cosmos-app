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

//! Threading helpers that keep blocking work off the GPUI main thread.
//!
//! The rule for the whole GUI: **nothing that can block runs on the UI thread** —
//! no subprocesses (`docker`, `curl`, PowerShell), no network, no thread joins,
//! and no file I/O beyond what happened before the event loop started.
//!
//! Blocking work runs on a dedicated OS thread via [`run_blocking`] (not GPUI's
//! background executor: these jobs can run for minutes — image pulls, installs —
//! and would starve that small shared pool). The UI side `await`s the returned
//! future inside a `cx.spawn` task, so the result is applied the moment it's
//! ready, with no polling.

use crate::context::Context;
use crate::settings::Settings;
use std::future::Future;
use std::sync::mpsc;

/// Run `f` on a new OS thread and resolve with its result. Resolves to `None`
/// only if the thread panicked before producing a value.
pub fn run_blocking<R, F>(f: F) -> impl Future<Output = Option<R>>
where
    R: Send + 'static,
    F: FnOnce() -> R + Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    async move { rx.await.ok() }
}

enum SaveRequest {
    Save(Box<Settings>),
    Flush(mpsc::Sender<()>),
}

/// Persists settings on a dedicated writer thread so a save (a small JSON write,
/// but still disk I/O that can stall on a slow or network home directory) never
/// runs on the UI thread. Requests are applied in order and coalesced: only the
/// newest pending settings are written.
pub struct SettingsSaver {
    tx: mpsc::Sender<SaveRequest>,
}

impl SettingsSaver {
    pub fn new(ctx: Context) -> Self {
        let (tx, rx) = mpsc::channel::<SaveRequest>();
        std::thread::spawn(move || {
            while let Ok(request) = rx.recv() {
                let mut latest = None;
                let mut flushes = Vec::new();
                let mut handle = |request| match request {
                    SaveRequest::Save(settings) => latest = Some(settings),
                    SaveRequest::Flush(ack) => flushes.push(ack),
                };
                handle(request);
                // Coalesce whatever else queued up while we were writing.
                while let Ok(request) = rx.try_recv() {
                    handle(request);
                }
                if let Some(settings) = latest {
                    settings.save(&ctx);
                }
                for ack in flushes {
                    let _ = ack.send(());
                }
            }
        });
        Self { tx }
    }

    /// Queue `settings` to be written. Returns immediately.
    pub fn save(&self, settings: &Settings) {
        let _ = self.tx.send(SaveRequest::Save(Box::new(settings.clone())));
    }

    /// A blocking "wait until every queued save is on disk" for the shutdown
    /// path. Call it from a worker thread, never the UI thread.
    pub fn flusher(&self) -> impl FnOnce() + Send + 'static {
        let tx = self.tx.clone();
        move || {
            let (ack_tx, ack_rx) = mpsc::channel();
            if tx.send(SaveRequest::Flush(ack_tx)).is_ok() {
                let _ = ack_rx.recv();
            }
        }
    }
}
