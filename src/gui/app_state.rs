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

//! The application model: everything the GUI knows and every action it can
//! take, independent of any window. Views ([`super::main_view`],
//! [`super::logs_view`]) render from it and call its methods.
//!
//! Threading: every method here runs on the GPUI main thread and returns
//! promptly. Anything that may block — subprocesses, network, file I/O, thread
//! joins — is handed to [`run_blocking`] and its result applied when the
//! awaited future resolves. The only periodic work on the UI thread is
//! [`AppState::heartbeat`], which just reads atomics / briefly-held mutexes.

use super::worker::{run_blocking, SettingsSaver};
use crate::context::{Context, Paths, Runtime};
use crate::logging::LogRecord;
use crate::monitor::{self, ContainerStatus};
use crate::operator::{BridgeConnectionStatus, MicroserviceOperator, MicroserviceStatus};
use crate::settings::{Edition, Settings};
use crate::update::{CheckOutcome, CosmosRelease, Release};
use crate::{commands, install};
use gpui_kit::{AnyWindowHandle, Context as Cx, Entity, SharedString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Log source name for the app's own messages.
const APP: &str = "openc3-cosmos-app";

/// Minimum time the splash screen is shown.
const SPLASH_DURATION: Duration = Duration::from_millis(2500);
/// UI-thread housekeeping cadence (cheap: atomics and short mutex locks only).
const HEARTBEAT: Duration = Duration::from_millis(250);
/// Container status poll interval on the main page.
const STATUS_INTERVAL: Duration = Duration::from_secs(2);
/// Environment re-check interval while the setup page is showing.
const ENV_INTERVAL: Duration = Duration::from_millis(1500);

/// The three top-level pages of the application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Splash,
    Install,
    Main,
}

/// Minimum severity shown in the log table (like LogMessages.vue's level select).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevelFilter {
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
}

impl LogLevelFilter {
    pub const ALL: [LogLevelFilter; 5] = [
        LogLevelFilter::Debug,
        LogLevelFilter::Info,
        LogLevelFilter::Warn,
        LogLevelFilter::Error,
        LogLevelFilter::Fatal,
    ];

    /// Severity rank; a record shows when its level rank >= the filter's rank.
    pub fn rank(self) -> u8 {
        self as u8
    }

    pub fn label(self) -> &'static str {
        match self {
            LogLevelFilter::Debug => "DEBUG",
            LogLevelFilter::Info => "INFO",
            LogLevelFilter::Warn => "WARN",
            LogLevelFilter::Error => "ERROR",
            LogLevelFilter::Fatal => "FATAL",
        }
    }
}

/// Severity rank of a record's level string (unknown levels sort as INFO).
pub fn level_rank(level: &str) -> u8 {
    match level {
        "DEBUG" => 0,
        "WARN" => 2,
        "ERROR" => 3,
        "FATAL" => 4,
        _ => 1, // INFO and anything unrecognized
    }
}

/// Result of an environment check. Every field is gathered on a worker thread
/// (several involve subprocesses: `docker info`, PowerShell WMI queries).
#[derive(Debug, Clone)]
pub struct EnvCheck {
    /// A docker or podman daemon is installed *and running* (reachable).
    pub container_ok: bool,
    /// A docker or podman binary is installed (whether or not it's running).
    pub container_installed: bool,
    /// The isolated Python runtime is installed.
    pub python_ok: bool,
    /// The COSMOS environment (cosmos-project folder) is installed.
    pub cosmos_ok: bool,
    /// Windows: the optional features Docker's WSL2 backend needs (Virtual
    /// Machine Platform, Windows Hypervisor Platform, WSL) are all enabled.
    /// Always true off Windows.
    pub windows_features_ok: bool,
    /// Windows: hardware virtualization (VT-x/AMD-V) state from firmware.
    /// `Some(false)` means it's disabled in the BIOS/UEFI (a hard blocker the app
    /// can't fix); `Some(true)` enabled; `None` unknown. Always `None` off Windows.
    pub virtualization: Option<bool>,
    /// The detected container runtime (`docker compose` flavor, rootless, ids).
    pub runtime: Option<Runtime>,
    /// `OPENC3_TAG` from the installed COSMOS `.env`, if any.
    pub installed_cosmos: Option<String>,
}

impl EnvCheck {
    /// Gather the environment. **Blocking** — worker threads only.
    fn detect(paths: &Paths) -> Self {
        // If Docker was just installed and we were added to the docker group,
        // re-exec into it so Docker becomes usable this session (Linux only;
        // replaces the process, so it's fine from a worker thread; no-op
        // otherwise). Its group probe spawns `id`, so it must not run on the UI.
        crate::docker::relaunch_in_docker_group_if_needed();
        Self {
            container_ok: crate::context::container_engine_running(),
            container_installed: crate::context::container_engine_installed(),
            python_ok: paths.python_installed(),
            cosmos_ok: paths.cosmos_installed(),
            windows_features_ok: install::missing_windows_features().is_empty(),
            virtualization: install::virtualization_enabled(),
            runtime: Runtime::detect(),
            installed_cosmos: crate::update::installed_cosmos_version(&paths.env_file()),
        }
    }
}

/// Handles to a running host-microservice operator thread.
pub struct Operator {
    status: Arc<Mutex<Vec<MicroserviceStatus>>>,
    bridge_status: Arc<Mutex<BridgeConnectionStatus>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Operator {
    /// Start the host microservice operator on its own thread, wiring the bridge
    /// client. openc3-cosmos-app is a client of the COSMOS bridge_microservice
    /// hub; [`crate::enroll::connect_bridge`] resolves its identity and the hub
    /// ticket (auto-enrolling co-located, or using a previously redeemed token).
    fn start(ctx: &Context, retry: Arc<AtomicBool>, bridge_enabled: bool) -> Self {
        let mut operator =
            MicroserviceOperator::new(ctx.paths.python.clone(), ctx.paths.microservices.clone());
        let status = operator.status_handle();
        let bridge_status = operator.bridge_status_handle();
        let shutdown = operator.shutdown_handle();
        if !bridge_enabled {
            if let Ok(mut value) = bridge_status.lock() {
                value.message = "Bridge functionality disabled".to_string();
            }
            return Self { status, bridge_status, shutdown, thread: None };
        }
        // Auto-enroll runs in the operator loop once COSMOS has been up a while
        // (retried, bounded, re-armed on restart). Give it a connector + a cheap
        // COSMOS-up check, both capturing the context.
        let connect_ctx = ctx.clone();
        let ready_ctx = ctx.clone();
        operator.set_bridge_connector(
            Box::new(move || crate::enroll::connect_bridge(&connect_ctx)),
            // Classify COSMOS container readiness (running/uptime, or a specific
            // reason it isn't) so the unpaired status explains why. Uptime only —
            // no CPU/mem stats (the slow part).
            Box::new(move || crate::monitor::cosmos_readiness(&ready_ctx, false)),
        );
        operator.set_retry_flag(retry);
        let thread = Some(std::thread::spawn(move || operator.run()));
        Self { status, bridge_status, shutdown, thread }
    }

    /// Signal the operator to stop its microservices and wait for it to exit.
    /// **Blocking** (joins the thread) — worker threads only.
    fn stop(self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.thread {
            let _ = handle.join();
        }
    }
}

/// A long-running action (install, start, stop, …) executing on a worker thread.
pub struct RunningTask {
    pub label: SharedString,
    /// The latest progress line it reported (e.g. a `docker compose` pull line).
    pub activity: Option<String>,
}

enum TaskEvent<T> {
    Activity(String),
    Dialog(String),
    Done(anyhow::Result<T>),
}

pub struct AppState {
    pub ctx: Context,
    pub settings: Settings,
    pub page: Page,
    started_at: Instant,
    /// Latest environment check; `None` until the first one completes.
    pub env: Option<EnvCheck>,
    checking_env: bool,
    last_env_check: Instant,

    pub statuses: Vec<ContainerStatus>,
    /// False until the first status snapshot has been received.
    pub status_loaded: bool,
    refreshing_status: bool,
    last_status_refresh: Instant,
    /// Whether the Containers section is collapsed (starts collapsed).
    pub status_collapsed: bool,
    /// Whether COSMOS is up and serving its web UI; gates the Open button.
    pub cosmos_ready: bool,
    /// The installed COSMOS version (from `.env`), refreshed off-thread with the
    /// environment/status polls so views never read the file while rendering.
    pub installed_cosmos: Option<String>,

    operator: Option<Operator>,
    /// An operator start/stop/restart (or pairing) is running on a worker.
    operator_busy: bool,
    /// A restart requested while another was in flight (`true` = re-enroll).
    operator_restart_pending: Option<bool>,
    /// Whether the running operator was started with the bridge enabled.
    bridge_operator_enabled: bool,
    /// Shared with the operator; "Retry" sets it to re-arm bridge enrollment.
    bridge_retry: Arc<AtomicBool>,
    pub bridge_pairing: bool,
    pub microservices: Vec<MicroserviceStatus>,
    pub bridge_status: BridgeConnectionStatus,

    /// Snapshot of captured log records for the log table.
    pub log_records: Vec<LogRecord>,
    log_generation: u64,
    pub log_level: LogLevelFilter,
    pub log_search: String,
    pub log_paused: bool,

    /// The long-running action in progress, if any (the app is "busy").
    pub task: Option<RunningTask>,

    pub settings_open: bool,
    /// The effective Development-mode context — `(run folder, core folder)` —
    /// captured when Settings opened; if it differs on close the operator is
    /// restarted so the bridge re-enrolls against the new compose context.
    dev_context_on_open: (Option<std::path::PathBuf>, Option<std::path::PathBuf>),
    pub shutdown_confirm: bool,
    /// The "Quit?" prompt, used where there's no tray to hide to (Linux).
    pub quit_confirm: bool,
    pub cleanup_confirm: bool,
    /// A Windows feature was just enabled and needs a reboot.
    pub restart_pending: bool,
    /// A dismissible instruction popup (post-install NEXT STEPS).
    pub dialog: Option<String>,
    /// Quit has been requested; host microservices are being stopped.
    pub quitting: bool,

    /// Latest app release from the background checker (startup + every 8h).
    update_latest: Arc<Mutex<Option<Release>>>,
    pub update_available: Option<Release>,
    pub checking_update: bool,
    /// Latest newer-than-installed COSMOS release from its checker.
    cosmos_latest: Arc<Mutex<Option<CosmosRelease>>>,
    pub cosmos_update: Option<CosmosRelease>,
    /// Live dev-mode flag shared with the COSMOS checker (which skips dev mode).
    cosmos_dev_mode: Arc<AtomicBool>,

    /// Set by the single-instance watcher thread when a later launch asks us
    /// to surface our window.
    show_requested: Arc<AtomicBool>,
    saver: SettingsSaver,

    pub main_window: Option<AnyWindowHandle>,
    pub logs_window: Option<(AnyWindowHandle, Entity<super::logs_view::LogsView>)>,
}

impl AppState {
    /// Build the model. Only cheap, non-blocking work happens here; the
    /// environment check, operator start, and update checks run on workers.
    pub fn new(
        mut ctx: Context,
        settings: Settings,
        singleton: crate::single_instance::Guard,
        cx: &mut Cx<Self>,
    ) -> Self {
        // The persisted edition is the source of truth in the GUI.
        ctx.enterprise = settings.edition.is_enterprise();
        // Development mode: the run folder (openc3.sh/compose) drives the context;
        // OPENC3_DEVEL (the openc3 Python library) always comes from the core folder.
        ctx.dev_folder = dev_run_folder(&settings);
        apply_dev_env(settings.dev_mode, dev_core_folder(&settings).as_deref());

        // A later launch drops a marker file asking us to show our window. Poll
        // for it on a background thread (it's file I/O) and hand the UI a flag.
        // The thread owns the guard, keeping the single-instance lock held for
        // the life of the process.
        let show_requested = Arc::new(AtomicBool::new(false));
        {
            let flag = show_requested.clone();
            std::thread::spawn(move || loop {
                if singleton.take_show_request() {
                    flag.store(true, Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(300));
            });
        }

        // Background release checkers (run now, then every 8h).
        let update_latest = Arc::new(Mutex::new(None));
        crate::update::spawn_checker(update_latest.clone());
        let cosmos_latest = Arc::new(Mutex::new(None));
        let cosmos_dev_mode = Arc::new(AtomicBool::new(settings.dev_mode));
        crate::update::spawn_cosmos_checker(
            cosmos_latest.clone(),
            ctx.paths.env_file(),
            settings.edition.is_enterprise(),
            settings.enterprise_token.clone(),
            cosmos_dev_mode.clone(),
        );

        // UI-thread heartbeat: cheap housekeeping that folds shared state from
        // the worker threads into the model.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(HEARTBEAT).await;
            if this.update(cx, |state, cx| state.heartbeat(cx)).is_err() {
                break;
            }
        })
        .detach();

        let saver = SettingsSaver::new(ctx.clone());
        let mut state = Self {
            ctx,
            settings,
            page: Page::Splash,
            started_at: Instant::now(),
            env: None,
            checking_env: false,
            last_env_check: Instant::now(),
            statuses: Vec::new(),
            status_loaded: false,
            refreshing_status: false,
            last_status_refresh: Instant::now(),
            status_collapsed: true,
            cosmos_ready: false,
            installed_cosmos: None,
            operator: None,
            operator_busy: false,
            operator_restart_pending: None,
            bridge_operator_enabled: false,
            bridge_retry: Arc::new(AtomicBool::new(false)),
            bridge_pairing: false,
            microservices: Vec::new(),
            bridge_status: BridgeConnectionStatus::default(),
            log_records: Vec::new(),
            log_generation: u64::MAX,
            log_level: LogLevelFilter::Info,
            log_search: String::new(),
            log_paused: false,
            task: None,
            settings_open: false,
            dev_context_on_open: (None, None),
            shutdown_confirm: false,
            quit_confirm: false,
            cleanup_confirm: false,
            restart_pending: false,
            dialog: None,
            quitting: false,
            update_latest,
            update_available: None,
            checking_update: false,
            cosmos_latest,
            cosmos_update: None,
            cosmos_dev_mode,
            show_requested,
            saver,
            main_window: None,
            logs_window: None,
        };
        crate::logging::info(APP, "OpenC3 COSMOS control panel ready.");
        state.refresh_env(cx);
        // Start the host microservice operator (it reads the COSMOS .env to
        // decide whether the bridge is enabled, so it starts on a worker).
        state.restart_operator(false, cx);
        state
    }

    pub fn busy(&self) -> bool {
        self.task.is_some()
    }

    /// Whether the bridge should run, from the settings and the cached
    /// installed COSMOS version (no file I/O).
    pub fn bridge_enabled(&self) -> bool {
        self.settings.bridge_enabled_for(self.installed_cosmos.as_deref())
    }

    /// Whether the setup page should be shown. Running COSMOS locally needs
    /// Docker + Python + the COSMOS environment (plus, on Windows, the WSL2/
    /// virtualization features Docker depends on); running against a remote
    /// COSMOS only needs the host Python runtime (for the bridge microservices).
    pub fn needs_setup(&self) -> bool {
        let Some(env) = &self.env else {
            return false;
        };
        if self.settings.run_locally {
            !(env.windows_features_ok && env.container_ok && env.python_ok && env.cosmos_ok)
        } else {
            !env.python_ok
        }
    }

    // ---------------------------------------------------------------------
    // Periodic housekeeping
    // ---------------------------------------------------------------------

    /// Runs every [`HEARTBEAT`] on the UI thread. Must stay cheap: it only
    /// reads atomics and briefly-held mutexes, and kicks off worker jobs.
    fn heartbeat(&mut self, cx: &mut Cx<Self>) {
        let mut changed = false;

        // SIGTERM/SIGINT/SIGHUP or a Windows console close: shut down the same
        // way Quit does, so host microservices are stopped, not orphaned.
        if crate::lifecycle::requested() && !self.quitting {
            self.quit(cx);
            return;
        }

        // Log table: re-snapshot only when the sink actually changed.
        let generation = crate::logging::generation();
        if !self.log_paused && generation != self.log_generation {
            self.log_generation = generation;
            self.log_records = crate::logging::snapshot();
            changed = true;
        }

        // Operator-published status.
        if let Some(op) = &self.operator {
            if let Ok(list) = op.status.lock() {
                if *list != self.microservices {
                    self.microservices = list.clone();
                    changed = true;
                }
            }
            if let Ok(status) = op.bridge_status.lock() {
                if *status != self.bridge_status {
                    self.bridge_status = status.clone();
                    changed = true;
                }
            }
        }

        // A COSMOS install or upgrade can change the version-derived bridge
        // default while the app is open. Start/stop the bridge once any
        // background task has finished, unless Settings is being edited.
        if !self.busy()
            && !self.settings_open
            && !self.operator_busy
            && !self.quitting
            && self.bridge_enabled() != self.bridge_operator_enabled
        {
            self.restart_operator(false, cx);
        }

        match self.page {
            Page::Splash => {
                // Route once the splash has shown for a moment *and* the first
                // environment check is in (it may take longer than the splash
                // on Windows, where it shells out to PowerShell).
                if self.env.is_some() && self.started_at.elapsed() >= SPLASH_DURATION {
                    if self.needs_setup() {
                        self.page = Page::Install;
                    } else {
                        self.go_main(cx);
                    }
                    changed = true;
                }
            }
            Page::Install => {
                // Keep the setup page in sync as components are installed; the
                // check's completion advances to Main once nothing is missing.
                if !self.busy() && self.last_env_check.elapsed() >= ENV_INTERVAL {
                    self.refresh_env(cx);
                }
            }
            Page::Main => {
                if !self.busy() && self.last_status_refresh.elapsed() >= STATUS_INTERVAL {
                    self.refresh_status(cx);
                }
            }
        }

        // Surface a pending app update on the main page (don't interrupt the
        // splash/setup flow); one modal at a time.
        if self.page == Page::Main && self.update_available.is_none() && self.dialog.is_none() {
            if let Ok(mut slot) = self.update_latest.lock() {
                let show = slot.as_ref().is_some_and(|rel| {
                    crate::update::is_newer(&rel.version, crate::update::current_version())
                        && rel.version != self.settings.skipped_version
                });
                if show {
                    self.update_available = slot.take();
                    changed = true;
                }
            }
        }
        // Likewise a pending COSMOS update, but only when the app update prompt
        // isn't already showing. The checker only stores newer-than-installed
        // releases; honor the skip.
        if self.page == Page::Main
            && !self.settings_open
            && self.update_available.is_none()
            && self.cosmos_update.is_none()
            && self.dialog.is_none()
        {
            if let Ok(mut slot) = self.cosmos_latest.lock() {
                let show = slot
                    .as_ref()
                    .is_some_and(|rel| rel.version != self.settings.cosmos_skipped_version);
                if show {
                    self.cosmos_update = slot.take();
                    changed = true;
                }
            }
        }

        // A second launch asked us to surface our window, or a tray click.
        if self.show_requested.swap(false, Ordering::Relaxed) {
            self.show_main_window(cx);
        }
        match crate::tray::poll() {
            Some(crate::tray::TrayAction::Show) => self.show_main_window(cx),
            Some(crate::tray::TrayAction::Quit) => self.quit(cx),
            None => {}
        }

        if changed {
            cx.notify();
        }
    }

    // ---------------------------------------------------------------------
    // Background refreshes
    // ---------------------------------------------------------------------

    /// Re-detect the environment on a worker thread (`docker info`, PowerShell
    /// WMI queries, runtime detection). At most one check runs at a time.
    pub fn refresh_env(&mut self, cx: &mut Cx<Self>) {
        if self.checking_env {
            return;
        }
        self.checking_env = true;
        let paths = self.ctx.paths.clone();
        cx.spawn(async move |this, cx| {
            let env = run_blocking(move || EnvCheck::detect(&paths)).await;
            let _ = this.update(cx, |state, cx| {
                state.checking_env = false;
                state.last_env_check = Instant::now();
                if let Some(env) = env {
                    state.apply_env(env, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn apply_env(&mut self, env: EnvCheck, cx: &mut Cx<Self>) {
        self.ctx.runtime = env.runtime.clone();
        self.installed_cosmos = env.installed_cosmos.clone();
        self.env = Some(env);
        if self.page == Page::Install && !self.busy() && !self.needs_setup() {
            self.go_main(cx);
        }
    }

    /// Poll container status + the web UI on a worker thread (`docker ps`/
    /// `stats` via the socket or CLI, and a `curl` probe). At most one in flight.
    pub fn refresh_status(&mut self, cx: &mut Cx<Self>) {
        if self.refreshing_status {
            return;
        }
        self.refreshing_status = true;
        let ctx = self.ctx.clone();
        let url = self.settings.effective_cosmos_url().to_string();
        let run_locally = self.settings.run_locally;
        // Per-container CPU/mem stats are expensive; only collect them when the
        // table is expanded to show them.
        let with_stats = !self.status_collapsed;
        cx.spawn(async move |this, cx| {
            let result = run_blocking(move || {
                // Only poll local containers when we're managing COSMOS locally.
                let statuses = if run_locally {
                    monitor::snapshot(&ctx, with_stats).unwrap_or_default()
                } else {
                    Vec::new()
                };
                let ready = probe_cosmos(&url);
                let installed = crate::update::installed_cosmos_version(&ctx.paths.env_file());
                (statuses, ready, installed)
            })
            .await;
            let _ = this.update(cx, |state, cx| {
                state.refreshing_status = false;
                state.last_status_refresh = Instant::now();
                if let Some((statuses, ready, installed)) = result {
                    state.statuses = statuses;
                    state.status_loaded = true;
                    state.cosmos_ready = ready;
                    state.installed_cosmos = installed;
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn go_main(&mut self, cx: &mut Cx<Self>) {
        self.page = Page::Main;
        self.refresh_status(cx);
    }

    // ---------------------------------------------------------------------
    // Long-running tasks
    // ---------------------------------------------------------------------

    /// Run `work` on a worker thread as the app's single long-running task,
    /// streaming its progress lines (via the `install` notifier) into the
    /// activity indicator and its dialog messages into a popup. `on_ok` runs on
    /// the UI thread with the result if it succeeds.
    fn run_task<T, W, D>(&mut self, label: impl Into<String>, cx: &mut Cx<Self>, work: W, on_ok: D)
    where
        T: Send + 'static,
        W: FnOnce() -> anyhow::Result<T> + Send + 'static,
        D: FnOnce(&mut Self, T, &mut Cx<Self>) + 'static,
    {
        if self.busy() {
            crate::logging::warn(APP, "A task is already running; please wait.");
            return;
        }
        let label = label.into();
        crate::logging::info(APP, &format!("{label}..."));
        self.task = Some(RunningTask { label: label.clone().into(), activity: None });
        cx.notify();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<TaskEvent<T>>();
        std::thread::spawn(move || {
            let activity_tx = tx.clone();
            install::set_notifier(Box::new(move |m| {
                crate::logging::info(APP, &m);
                let _ = activity_tx.send(TaskEvent::Activity(m));
            }));
            let dialog_tx = tx.clone();
            install::set_dialog_notifier(Box::new(move |m| {
                let _ = dialog_tx.send(TaskEvent::Dialog(m));
            }));
            let result = work();
            install::clear_notifier();
            install::clear_dialog_notifier();
            match &result {
                Ok(_) => crate::logging::info(APP, &format!("{label}: done.")),
                Err(e) => crate::logging::error(APP, &format!("{label}: ERROR: {e}")),
            }
            let _ = tx.send(TaskEvent::Done(result));
        });

        cx.spawn(async move |this, cx| {
            let mut on_ok = Some(on_ok);
            let mut finished = false;
            while let Some(first) = rx.recv().await {
                // Coalesce bursts (e.g. image-pull progress) into one UI update.
                let mut batch = vec![first];
                while let Ok(more) = rx.try_recv() {
                    batch.push(more);
                }
                let updated = this.update(cx, |state, cx| {
                    for event in batch {
                        match event {
                            TaskEvent::Activity(line) => {
                                if let Some(task) = state.task.as_mut() {
                                    task.activity = Some(line);
                                }
                            }
                            TaskEvent::Dialog(msg) => state.dialog = Some(msg),
                            TaskEvent::Done(result) => {
                                state.task = None;
                                finished = true;
                                if let (Ok(value), Some(f)) = (result, on_ok.take()) {
                                    f(state, value, cx);
                                }
                            }
                        }
                    }
                    cx.notify();
                });
                if updated.is_err() || finished {
                    return;
                }
            }
            // The worker died without reporting (panicked): don't stay busy.
            let _ = this.update(cx, |state, cx| {
                state.task = None;
                cx.notify();
            });
        })
        .detach();
    }

    pub fn install_docker(&mut self, cx: &mut Cx<Self>) {
        if cfg!(target_os = "macos") {
            self.run_task("Installing Docker Desktop", cx, install::install_docker_macos, |_, _, _| {});
        } else if cfg!(target_os = "windows") {
            self.run_task("Installing Docker Desktop", cx, install::install_docker_windows, |_, _, _| {});
        } else {
            let ctx = self.ctx.clone();
            self.run_task("Installing Docker", cx, move || install::docker(&ctx), |_, _, _| {});
        }
    }

    pub fn start_docker(&mut self, cx: &mut Cx<Self>) {
        self.run_task("Starting Docker", cx, install::start_docker, |_, _, _| {});
    }

    /// Windows: enable the WSL2/virtualization optional features Docker needs;
    /// on success prompt for the reboot they require.
    pub fn enable_windows_features(&mut self, cx: &mut Cx<Self>) {
        self.run_task(
            "Enabling Windows features",
            cx,
            install::enable_windows_features,
            |state, _, _| state.restart_pending = true,
        );
    }

    pub fn restart_windows(&mut self, cx: &mut Cx<Self>) {
        self.restart_pending = false;
        self.run_task("Restarting Windows", cx, install::restart_windows, |_, _, _| {});
    }

    pub fn dismiss_restart(&mut self, cx: &mut Cx<Self>) {
        self.restart_pending = false;
        cx.notify();
    }

    pub fn dismiss_dialog(&mut self, cx: &mut Cx<Self>) {
        self.dialog = None;
        cx.notify();
    }

    pub fn install_python(&mut self, cx: &mut Cx<Self>) {
        let ctx = self.ctx.clone();
        self.run_task("Installing Python", cx, move || install::python(&ctx), |_, _, _| {});
    }

    pub fn install_cosmos(&mut self, cx: &mut Cx<Self>) {
        let ctx = self.ctx.clone();
        let enterprise = self.settings.edition.is_enterprise();
        let token = self.settings.enterprise_token.clone();
        let label = if enterprise { "Installing COSMOS Enterprise" } else { "Installing COSMOS Core" };
        self.run_task(label, cx, move || install::cosmos(&ctx, "latest", enterprise, &token), |_, _, _| {});
    }

    /// Leave the setup page without installing everything.
    pub fn skip_setup(&mut self, cx: &mut Cx<Self>) {
        self.go_main(cx);
        self.refresh_env(cx);
        cx.notify();
    }

    pub fn start_cosmos(&mut self, cx: &mut Cx<Self>) {
        let ctx = self.ctx.clone();
        self.run_task("Starting COSMOS", cx, move || commands::run(&ctx), |_, _, _| {});
    }

    /// Ask to shut COSMOS down — opens the confirmation modal.
    pub fn request_shutdown(&mut self, cx: &mut Cx<Self>) {
        self.shutdown_confirm = true;
        cx.notify();
    }

    pub fn cancel_shutdown(&mut self, cx: &mut Cx<Self>) {
        self.shutdown_confirm = false;
        cx.notify();
    }

    pub fn confirm_shutdown(&mut self, cx: &mut Cx<Self>) {
        self.shutdown_confirm = false;
        let ctx = self.ctx.clone();
        self.run_task("Stopping COSMOS", cx, move || commands::stop(&ctx), |_, _, _| {});
    }

    /// Open COSMOS in the default browser. Uses the platform's native,
    /// non-blocking URL opener rather than waiting on an `open`/`xdg-open`
    /// subprocess.
    pub fn open_browser(&mut self, cx: &mut Cx<Self>) {
        let url = self.settings.effective_cosmos_url().to_string();
        cx.open_url(&url);
    }

    // ---------------------------------------------------------------------
    // Updates
    // ---------------------------------------------------------------------

    /// Download + launch the available update's installer, then quit so the
    /// installer can replace the app (unless it fell back to the release page).
    pub fn install_update(&mut self, cx: &mut Cx<Self>) {
        let Some(rel) = self.update_available.take() else {
            return;
        };
        let label = format!("Installing update {}", rel.version);
        self.run_task(
            label,
            cx,
            move || crate::update::install_release(&rel),
            |state, outcome, cx| {
                if outcome == crate::update::InstallReleaseOutcome::InstallerLaunched {
                    state.quit(cx);
                }
            },
        );
    }

    pub fn skip_update(&mut self, cx: &mut Cx<Self>) {
        if let Some(rel) = self.update_available.take() {
            self.settings.skipped_version = rel.version;
            self.save_settings();
        }
        cx.notify();
    }

    pub fn dismiss_update(&mut self, cx: &mut Cx<Self>) {
        self.update_available = None;
        cx.notify();
    }

    /// Shut down COSMOS, replace it with the available new version, restart.
    pub fn install_cosmos_update(&mut self, cx: &mut Cx<Self>) {
        let Some(rel) = self.cosmos_update.take() else {
            return;
        };
        let ctx = self.ctx.clone();
        let enterprise = self.settings.edition.is_enterprise();
        let token = self.settings.enterprise_token.clone();
        let label = format!("Upgrading COSMOS to {}", rel.version);
        self.run_task(
            label,
            cx,
            move || commands::upgrade_cosmos(&ctx, &rel.tag, enterprise, &token),
            |_, _, _| {},
        );
    }

    pub fn skip_cosmos_update(&mut self, cx: &mut Cx<Self>) {
        if let Some(rel) = self.cosmos_update.take() {
            self.settings.cosmos_skipped_version = rel.version;
            self.save_settings();
        }
        cx.notify();
    }

    pub fn dismiss_cosmos_update(&mut self, cx: &mut Cx<Self>) {
        self.cosmos_update = None;
        cx.notify();
    }

    /// The Settings "Check for updates now": checks the app, then COSMOS, and
    /// always reports (modal or popup), ignoring skipped versions.
    pub fn check_for_updates(&mut self, cx: &mut Cx<Self>) {
        if self.checking_update {
            return;
        }
        self.checking_update = true;
        crate::logging::info("update", "Checking for updates…");
        let env_path = self.ctx.paths.env_file();
        let enterprise = self.settings.edition.is_enterprise();
        let token = self.settings.enterprise_token.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let outcome =
                run_blocking(move || crate::update::check_now(&env_path, enterprise, &token)).await;
            let _ = this.update(cx, |state, cx| {
                state.checking_update = false;
                match outcome {
                    Some(CheckOutcome::AppUpdate(rel)) => state.update_available = Some(rel),
                    Some(CheckOutcome::CosmosUpdate(rel)) => state.cosmos_update = Some(rel),
                    Some(CheckOutcome::UpToDate) => {
                        state.dialog = Some(format!(
                            "You're on the latest version (app v{}) and COSMOS is up to date.",
                            crate::update::current_version()
                        ));
                    }
                    Some(CheckOutcome::Failed(msg)) => {
                        state.dialog = Some(format!("Update check failed:\n\n{msg}"));
                    }
                    None => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    // ---------------------------------------------------------------------
    // Settings
    // ---------------------------------------------------------------------

    fn save_settings(&self) {
        self.saver.save(&self.settings);
    }

    /// Re-apply development-mode settings: the process-env tag overrides and the
    /// context's dev source folder.
    fn apply_dev_settings(&mut self) {
        self.ctx.dev_folder = dev_run_folder(&self.settings);
        apply_dev_env(self.settings.dev_mode, dev_core_folder(&self.settings).as_deref());
        // Keep the COSMOS checker's live dev-mode flag in sync.
        self.cosmos_dev_mode.store(self.settings.dev_mode, Ordering::Relaxed);
    }

    pub fn open_settings(&mut self, cx: &mut Cx<Self>) {
        self.settings_open = true;
        // Remember the effective dev context so we can tell on close whether it
        // changed (and thus whether to re-enroll the bridge / rebuild venvs).
        self.dev_context_on_open = (self.ctx.dev_folder.clone(), dev_core_folder(&self.settings));
        cx.notify();
    }

    pub fn close_settings(&mut self, cx: &mut Cx<Self>) {
        self.settings_open = false;
        // Persist any text-field edits made while the dialog was open.
        self.save_settings();
        let dev_context = (self.ctx.dev_folder.clone(), dev_core_folder(&self.settings));
        if dev_context != self.dev_context_on_open {
            crate::logging::info("bridge", "Development context changed; re-running bridge enrollment");
            self.dev_context_on_open = dev_context;
            self.restart_operator(true, cx);
        }
        cx.notify();
    }

    /// Text-field edits are persisted when Settings closes, so typing doesn't
    /// queue a write per keystroke.
    pub fn set_cosmos_url(&mut self, value: String) {
        self.settings.cosmos_url = value;
    }

    pub fn set_enterprise_token(&mut self, value: String, cx: &mut Cx<Self>) {
        self.settings.enterprise_token = value;
        // The setup page's Install button enables once a token is present.
        cx.notify();
    }

    pub fn set_dev_folder(&mut self, value: String) {
        self.settings.dev_folder = value;
        self.apply_dev_settings();
    }

    pub fn set_dev_folder_enterprise(&mut self, value: String) {
        self.settings.dev_folder_enterprise = value;
        self.apply_dev_settings();
    }

    /// A folder chosen with the Browse… picker is saved right away.
    pub fn pick_dev_folder(&mut self, enterprise: bool, path: String, cx: &mut Cx<Self>) {
        if enterprise {
            self.settings.dev_folder_enterprise = path;
        } else {
            self.settings.dev_folder = path;
        }
        self.apply_dev_settings();
        self.save_settings();
        cx.notify();
    }

    pub fn set_run_locally(&mut self, value: bool, cx: &mut Cx<Self>) {
        self.settings.run_locally = value;
        self.save_settings();
        cx.notify();
    }

    pub fn set_bridge_disabled(&mut self, disabled: bool, cx: &mut Cx<Self>) {
        self.settings.disable_bridge = Some(disabled);
        self.save_settings();
        crate::logging::info(
            "bridge",
            if disabled { "Bridge functionality disabled" } else { "Bridge functionality enabled" },
        );
        self.restart_operator(false, cx);
        cx.notify();
    }

    pub fn set_edition(&mut self, edition: Edition, cx: &mut Cx<Self>) {
        self.settings.edition = edition;
        // The rest of the app (compose profiles, upgrade source) keys off
        // ctx.enterprise; the dev run folder depends on the edition too.
        self.ctx.enterprise = edition.is_enterprise();
        self.apply_dev_settings();
        self.save_settings();
        cx.notify();
    }

    pub fn set_dev_mode(&mut self, value: bool, cx: &mut Cx<Self>) {
        self.settings.dev_mode = value;
        self.apply_dev_settings();
        self.save_settings();
        if value {
            // Entering dev mode: drop any pending COSMOS update prompt — dev mode
            // runs "latest" from source, not a tagged install.
            self.cosmos_update = None;
            if let Ok(mut slot) = self.cosmos_latest.lock() {
                *slot = None;
            }
        } else {
            // Leaving dev mode: recheck now instead of waiting up to 8h.
            crate::update::spawn_cosmos_check_once(
                self.cosmos_latest.clone(),
                self.ctx.paths.env_file(),
                self.settings.edition.is_enterprise(),
                self.settings.enterprise_token.clone(),
            );
        }
        cx.notify();
    }

    // ---------------------------------------------------------------------
    // Log table
    // ---------------------------------------------------------------------

    pub fn set_log_level(&mut self, level: LogLevelFilter, cx: &mut Cx<Self>) {
        self.log_level = level;
        cx.notify();
    }

    pub fn set_log_search(&mut self, value: String, cx: &mut Cx<Self>) {
        self.log_search = value;
        cx.notify();
    }

    pub fn toggle_log_pause(&mut self, cx: &mut Cx<Self>) {
        self.log_paused = !self.log_paused;
        // Resuming: the next heartbeat re-snapshots (generation mismatch).
        if !self.log_paused {
            self.log_generation = u64::MAX;
        }
        cx.notify();
    }

    pub fn clear_log(&mut self, cx: &mut Cx<Self>) {
        crate::logging::clear();
        self.log_records.clear();
        self.log_generation = crate::logging::generation();
        cx.notify();
    }

    // ---------------------------------------------------------------------
    // Cleanup
    // ---------------------------------------------------------------------

    pub fn show_cleanup(&mut self, cx: &mut Cx<Self>) {
        self.cleanup_confirm = true;
        // Close Settings behind it so cancelling returns to the main page.
        self.settings_open = false;
        self.save_settings();
        cx.notify();
    }

    pub fn cancel_cleanup(&mut self, cx: &mut Cx<Self>) {
        self.cleanup_confirm = false;
        cx.notify();
    }

    /// Remove all COSMOS volumes and data. The view only enables this once the
    /// user has typed the confirmation word.
    pub fn confirm_cleanup(&mut self, cx: &mut Cx<Self>) {
        self.cleanup_confirm = false;
        let ctx = self.ctx.clone();
        // force=true (no prompt, we already confirmed), local=false.
        self.run_task(
            "Cleaning up (removing all data)",
            cx,
            move || commands::cleanup(&ctx, false, true),
            |_, _, _| {},
        );
    }

    pub fn toggle_status(&mut self, cx: &mut Cx<Self>) {
        self.status_collapsed = !self.status_collapsed;
        // Expanding shows CPU/mem, which the poll skips while collapsed —
        // refresh right away so they populate promptly.
        if !self.status_collapsed && !self.busy() {
            self.refresh_status(cx);
        }
        cx.notify();
    }

    // ---------------------------------------------------------------------
    // Bridge operator
    // ---------------------------------------------------------------------

    /// (Re)start the host-microservice operator on a worker thread so it picks
    /// up the current context (e.g. after a Development-mode compose-context
    /// switch). The old operator is stopped and joined first so two never run
    /// against the same bridge. When `reenroll`, an auto-enrolled ticket is
    /// dropped first so enrollment re-runs against the new context (a manual
    /// token is preserved; see `forget_cached_ticket`). Requests made while one
    /// is in flight are queued and run afterwards.
    pub fn restart_operator(&mut self, reenroll: bool, cx: &mut Cx<Self>) {
        if self.quitting {
            return;
        }
        if self.operator_busy {
            let pending = self.operator_restart_pending.unwrap_or(false);
            self.operator_restart_pending = Some(pending || reenroll);
            return;
        }
        self.operator_busy = true;
        let ctx = self.ctx.clone();
        let settings = self.settings.clone();
        let retry = self.bridge_retry.clone();
        let old = self.operator.take();
        cx.spawn(async move |this, cx| {
            let started = run_blocking(move || {
                if reenroll {
                    crate::enroll::forget_cached_ticket(&ctx.paths.root);
                }
                if let Some(old) = old {
                    old.stop();
                }
                let enabled = settings.bridge_enabled(&ctx.paths.env_file());
                (Operator::start(&ctx, retry, enabled), enabled)
            })
            .await;
            let _ = this.update(cx, |state, cx| {
                if let Some((operator, enabled)) = started {
                    state.operator = Some(operator);
                    state.bridge_operator_enabled = enabled;
                }
                state.operator_settled(cx);
            });
        })
        .detach();
    }

    /// An operator job finished: finish a pending quit, or run a queued restart.
    fn operator_settled(&mut self, cx: &mut Cx<Self>) {
        self.operator_busy = false;
        if self.quitting {
            self.finish_quit(cx);
        } else if let Some(reenroll) = self.operator_restart_pending.take() {
            self.restart_operator(reenroll, cx);
        }
        cx.notify();
    }

    /// Redeem an enrollment token from a remote COSMOS (Admin → Bridges) and
    /// restart the operator so it uses the new pairing. Both steps block
    /// (network + thread join), so they run on a worker.
    pub fn submit_bridge_token(&mut self, token: String, cx: &mut Cx<Self>) -> bool {
        let token = token.trim().to_string();
        if token.is_empty() || self.bridge_pairing || self.operator_busy || self.quitting {
            return false;
        }
        self.bridge_pairing = true;
        self.operator_busy = true;
        self.settings_open = false;
        self.save_settings();
        crate::logging::info("bridge", "Pairing with COSMOS…");
        let ctx = self.ctx.clone();
        let settings = self.settings.clone();
        let retry = self.bridge_retry.clone();
        let old = self.operator.take();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = run_blocking(move || match crate::enroll::enroll_with_token(&ctx, &token) {
                Ok(bridge) => {
                    crate::logging::info("bridge", &format!("Paired with bridge '{bridge}'. Reconnecting…"));
                    if let Some(old) = old {
                        old.stop();
                    }
                    let enabled = settings.bridge_enabled(&ctx.paths.env_file());
                    (Operator::start(&ctx, retry, enabled), Some(enabled))
                }
                Err(e) => {
                    crate::logging::error("bridge", &format!("Enrollment failed: {e:#}"));
                    // Old operator untouched; hand it back as-is.
                    let old = old.unwrap_or_else(|| Operator::start(&ctx, retry, false));
                    (old, None)
                }
            })
            .await;
            let _ = this.update(cx, |state, cx| {
                state.bridge_pairing = false;
                if let Some((operator, enabled)) = result {
                    state.operator = Some(operator);
                    if let Some(enabled) = enabled {
                        state.bridge_operator_enabled = enabled;
                    }
                }
                state.operator_settled(cx);
            });
        })
        .detach();
        true
    }

    /// Re-arm the operator's bridge enrollment (the "Retry" button); it picks
    /// this up on its next cycle. Optimistically reflected in the UI.
    pub fn retry_bridge(&mut self, cx: &mut Cx<Self>) {
        self.bridge_retry.store(true, Ordering::Relaxed);
        self.bridge_status.message = "Retrying…".to_string();
        crate::logging::info("bridge", "Retrying bridge connection…");
        cx.notify();
    }

    // ---------------------------------------------------------------------
    // Windows and lifecycle
    // ---------------------------------------------------------------------

    /// Surface the main window: restore the Dock icon (macOS) and focus the
    /// window, reopening it if it was closed to the tray.
    pub fn show_main_window(&mut self, cx: &mut Cx<Self>) {
        crate::tray::set_dock_visible(true);
        cx.activate(true);
        if let Some(handle) = self.main_window {
            if handle.update(cx, |_, window, _| window.activate_window()).is_ok() {
                return;
            }
            self.main_window = None;
        }
        let state = cx.entity();
        // Opening a window builds its view, which reads this entity — defer
        // until this update has released it.
        cx.defer(move |cx| super::open_main_window(state, cx));
    }

    /// The main window's close button. With a tray (macOS/Windows) the window
    /// closes but the app keeps running in the tray (and drops out of the Dock
    /// on macOS). Without one (Linux) closing would quit, so confirm first.
    /// Returns whether the window may close.
    pub fn main_window_should_close(&mut self, cx: &mut Cx<Self>) -> bool {
        if self.quitting {
            return true;
        }
        if crate::tray::ENABLED {
            crate::tray::set_dock_visible(false);
            self.main_window = None;
            true
        } else {
            self.quit_confirm = true;
            cx.notify();
            false
        }
    }

    pub fn cancel_quit(&mut self, cx: &mut Cx<Self>) {
        self.quit_confirm = false;
        cx.notify();
    }

    /// Open (or retarget) the container logs window for `service`.
    pub fn view_logs(&mut self, service: String, cx: &mut Cx<Self>) {
        if let Some((handle, view)) = self.logs_window.clone() {
            let ok = handle
                .update(cx, |_, window, cx| {
                    view.update(cx, |view, cx| view.load(service.clone(), window, cx));
                    window.activate_window();
                })
                .is_ok();
            if ok {
                return;
            }
            self.logs_window = None;
        }
        let state = cx.entity();
        let ctx = self.ctx.clone();
        cx.defer(move |cx| super::open_logs_window(state, ctx, service, cx));
    }

    /// Stop the supervised microservices cleanly, flush settings, then exit —
    /// so we don't orphan spawned child processes. The stop/join runs on a
    /// worker; the UI stays live (showing "Shutting down…") until it finishes.
    pub fn quit(&mut self, cx: &mut Cx<Self>) {
        if self.quitting {
            return;
        }
        self.quitting = true;
        self.quit_confirm = false;
        crate::logging::info(APP, "Shutting down…");
        cx.notify();
        // An operator start/restart in flight finishes first; operator_settled
        // then completes the quit with the operator it produced.
        if !self.operator_busy {
            self.finish_quit(cx);
        }
    }

    fn finish_quit(&mut self, cx: &mut Cx<Self>) {
        let operator = self.operator.take();
        let flush = self.saver.flusher();
        cx.spawn(async move |_, cx| {
            run_blocking(move || {
                if let Some(operator) = operator {
                    operator.stop();
                }
                flush();
                crate::lifecycle::cleanup_finished();
            })
            .await;
            cx.update(|cx| cx.quit());
        })
        .detach();
    }

    /// For an exit we didn't initiate (macOS: Dock "Quit", logout/shutdown —
    /// AppKit terminates the app without going through [`Self::quit`]): hand
    /// back the blocking work that stops the host microservices and flushes
    /// settings, or `None` if our own quit already did it.
    pub fn stop_for_exit(&mut self) -> Option<impl FnOnce() + Send + 'static> {
        self.quitting = true;
        let operator = self.operator.take()?;
        crate::logging::info(APP, "Exiting at the system's request; stopping host microservices…");
        let flush = self.saver.flusher();
        Some(move || {
            operator.stop();
            flush();
        })
    }

    /// The installed COSMOS version for display ("latest (dev)" in dev mode,
    /// `None` when COSMOS is remote).
    pub fn cosmos_version_label(&self) -> Option<String> {
        if !self.settings.run_locally {
            None
        } else if self.settings.dev_mode {
            Some("latest (dev)".to_string())
        } else {
            Some(
                self.installed_cosmos
                    .as_ref()
                    .map(|v| format!("v{v}"))
                    .unwrap_or_else(|| "not installed".to_string()),
            )
        }
    }
}

/// Apply the development-mode process-environment overrides so child processes
/// (docker compose / openc3.sh) and host-microservice venvs pick them up:
///   * OPENC3_TAG / OPENC3_ENTERPRISE_TAG = "latest" (override the `.env`), and
///   * OPENC3_DEVEL = the local openc3 gem in the **core** dev folder, so host
///     venvs install openc3 editable from the working-tree source (see
///     `operator::openc3_devel_source`) instead of the published package. The
///     Python library always comes from core, even for an Enterprise dev run.
fn apply_dev_env(dev_mode: bool, dev_folder: Option<&std::path::Path>) {
    if dev_mode {
        std::env::set_var("OPENC3_TAG", "latest");
        std::env::set_var("OPENC3_ENTERPRISE_TAG", "latest");
        match dev_folder.and_then(openc3_gem_dir) {
            Some(gem) => std::env::set_var("OPENC3_DEVEL", gem),
            None => std::env::remove_var("OPENC3_DEVEL"),
        }
    } else {
        std::env::remove_var("OPENC3_TAG");
        std::env::remove_var("OPENC3_ENTERPRISE_TAG");
        std::env::remove_var("OPENC3_DEVEL");
    }
}

/// The local openc3 gem directory (containing a Python package) inside a dev
/// checkout, for OPENC3_DEVEL. Handles the monorepo layout (`<dev>/openc3/python`)
/// and the case where the dev folder itself is the openc3 gem (`<dev>/python`).
fn openc3_gem_dir(dev: &std::path::Path) -> Option<std::path::PathBuf> {
    let monorepo = dev.join("openc3");
    if monorepo.join("python").join("pyproject.toml").is_file() {
        return Some(monorepo);
    }
    if dev.join("python").join("pyproject.toml").is_file() {
        return Some(dev.to_path_buf());
    }
    None
}

/// The **core** development checkout (`settings.dev_folder`) — the source of the
/// openc3 Python library (`OPENC3_DEVEL`). None when dev mode is off or unset.
fn dev_core_folder(settings: &Settings) -> Option<std::path::PathBuf> {
    if settings.dev_mode && !settings.dev_folder.trim().is_empty() {
        Some(std::path::PathBuf::from(settings.dev_folder.trim()))
    } else {
        None
    }
}

/// The development checkout COSMOS is actually run from (its `openc3.sh` /
/// compose). For Enterprise that's the enterprise checkout
/// (`settings.dev_folder_enterprise`); for Core it's the core checkout
/// (`settings.dev_folder`). None when dev mode is off or the relevant folder is
/// unset. The Python library always comes from the core folder regardless.
fn dev_run_folder(settings: &Settings) -> Option<std::path::PathBuf> {
    if !settings.dev_mode {
        return None;
    }
    let folder = if settings.edition.is_enterprise() {
        settings.dev_folder_enterprise.trim()
    } else {
        settings.dev_folder.trim()
    };
    (!folder.is_empty()).then(|| std::path::PathBuf::from(folder))
}

/// Probe the COSMOS web UI: ready when `url` returns success and serves the
/// index HTML. Uses curl with a short timeout; returns false on any failure.
/// **Blocking** — worker threads only.
fn probe_cosmos(url: &str) -> bool {
    // Route through process::capture so it gets CREATE_NO_WINDOW on Windows —
    // this is polled continuously, and a bare Command would flash a curl console
    // window every time (the GUI has no console of its own).
    let mut cmd = std::process::Command::new("curl");
    cmd.args(["-fsSL", "--max-time", "3", url]);
    match crate::process::capture(&mut cmd) {
        Ok(o) if o.status.success() => {
            let body = String::from_utf8_lossy(&o.stdout).to_lowercase();
            body.contains("<html") || body.contains("<!doctype html")
        }
        _ => false,
    }
}
