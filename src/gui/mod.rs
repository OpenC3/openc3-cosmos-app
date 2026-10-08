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

//! GPUI-based graphical control panel. This is the default front-end (the app
//! launches into it when run with no subcommand). It shows live container and
//! bridge health and offers buttons for the common lifecycle actions.
//!
//! Layout of the module:
//! * [`app_state`] — the window-independent model and every action.
//! * [`main_view`] / [`logs_view`] — the windows, rendered from the model.
//! * [`ui`] — palette, theme and shared visual pieces.
//! * [`worker`] — threading helpers. **No blocking work runs on the UI
//!   thread**: subprocesses, network, disk writes and thread joins all run on
//!   worker threads and report back through awaited futures (see `worker`).

mod app_state;
mod logs_view;
mod main_view;
mod ui;
mod worker;

use crate::context::{Context, Paths};
use app_state::AppState;
use gpui_kit::component::TitleBar;
use gpui_kit::{
    actions, px, size, App, AppContext, AssetSource, Bounds, Entity, Global, KeyBinding, Menu,
    MenuItem, QuitMode, SharedString, TitlebarOptions, WindowBounds, WindowOptions,
};
use std::borrow::Cow;
use std::path::PathBuf;

actions!(openc3, [Quit]);

// Lucide icons used beyond gpui-component's default set; only these SVGs are
// embedded in the binary.
gpui_kit::assets::icon_assets!(
    ExtraIcons,
    [
        Activity, Boxes, Cable, CircleCheckBig, Cloud, Container, Download, FolderSearch, Gauge,
        KeyRound, Layers, Package, PlugZap, Power, RotateCcw, ScrollText, Server, ShieldCheck,
        Sparkles, SquareArrowOutUpRight, Terminal, Unplug, Wrench, Zap,
    ]
);

/// Default component icons plus our extras.
struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> gpui_kit::Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

/// The app model, reachable from app-level callbacks (Dock reopen, quit).
struct AppGlobal(Entity<AppState>);
impl Global for AppGlobal {}

/// Launch the GUI. Returns when the user quits.
pub fn launch(root_override: Option<PathBuf>, enterprise: bool) -> anyhow::Result<()> {
    // Only one GUI instance may run — a second launch signals the first to show
    // its window (it may be hidden in the tray) and exits, rather than spawning a
    // duplicate (another tray icon / operator). See `single_instance`. Resolve
    // the root the same way Context does so the lock lives in the app's data
    // dir; fall back to the temp dir if that fails.
    let paths = Paths::resolve(root_override)?;
    let guard = match crate::single_instance::acquire(&paths.root) {
        crate::single_instance::Acquire::Secondary => {
            crate::logging::info(
                "openc3-cosmos-app",
                "OpenC3 COSMOS is already running; brought its window to the front.",
            );
            return Ok(());
        }
        crate::single_instance::Acquire::Primary(guard) => guard,
    };

    // Build the context *without* container-runtime detection: that runs
    // `docker info` / `docker compose version`, which can stall for seconds
    // (or hang on a wedged daemon). The first environment check — on a worker
    // thread — fills it in before the splash screen hands off.
    let ctx = Context { paths, runtime: None, enterprise, dev_folder: None };
    // Tiny local file read, done before the event loop starts.
    let settings = crate::settings::Settings::load(&ctx);

    let app = gpui_kit::application()
        .with_assets(AppAssets)
        // Closing the main window hides to the tray; only an explicit Quit exits.
        .with_quit_mode(QuitMode::Explicit);
    // macOS: clicking the Dock icon with no window open shows the main window.
    app.on_reopen(|cx| {
        if let Some(global) = cx.try_global::<AppGlobal>() {
            let state = global.0.clone();
            state.update(cx, |state, cx| state.show_main_window(cx));
        }
    });
    app.run(move |cx| {
        gpui_kit::init(cx);
        ui::apply_theme(cx);
        // The tray must be created on the main thread once the platform app is
        // initialized (macOS requires an initialized NSApplication).
        crate::tray::init();

        let state = cx.new(|cx| AppState::new(ctx, settings, guard, cx));
        cx.set_global(AppGlobal(state.clone()));

        // Quit (⌘Q / the app menu) goes through the graceful shutdown.
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.on_action({
            let state = state.clone();
            move |_: &Quit, cx: &mut App| state.update(cx, |state, cx| state.quit(cx))
        });
        cx.set_menus(vec![Menu {
            name: "OpenC3 COSMOS".into(),
            items: vec![MenuItem::action("Quit OpenC3 COSMOS", Quit)],
            disabled: false,
        }]);

        // Keep the model's window handles in sync however a window goes away.
        cx.on_window_closed({
            let state = state.clone();
            move |cx, id| {
                state.update(cx, |state, _| {
                    if state.main_window.is_some_and(|w| w.window_id() == id) {
                        state.main_window = None;
                    }
                    if state.logs_window.as_ref().is_some_and(|(w, _)| w.window_id() == id) {
                        state.logs_window = None;
                    }
                });
            }
        })
        .detach();

        open_main_window(state, cx);
        cx.activate(true);
    });
    Ok(())
}

/// Window options shared by our windows: the custom title bar, a native title
/// (taskbar / window switcher) and a centered initial size.
fn window_options(title: &str, width: f32, height: f32, min: (f32, f32), cx: &App) -> WindowOptions {
    WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some(SharedString::from(title.to_string())),
            ..TitleBar::title_bar_options()
        }),
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(width), px(height)),
            cx,
        ))),
        window_min_size: Some(size(px(min.0), px(min.1))),
        app_id: Some("com.openc3.cosmos".to_string()),
        ..TitleBar::window_options()
    }
}

/// Open the main control-panel window.
fn open_main_window(state: Entity<AppState>, cx: &mut App) {
    let options = window_options("OpenC3 COSMOS", 1120., 840., (760., 560.), cx);
    let view_state = state.clone();
    match gpui_kit::open_window(options, cx, move |window, cx| {
        cx.new(|cx| main_view::MainView::new(view_state, window, cx))
    }) {
        Ok((handle, _)) => state.update(cx, |state, _| state.main_window = Some(handle)),
        Err(e) => crate::logging::error("openc3-cosmos-app", &format!("Failed to open window: {e:#}")),
    }
}

/// Open the container-logs window for `service`.
fn open_logs_window(state: Entity<AppState>, ctx: Context, service: String, cx: &mut App) {
    let options = window_options("OpenC3 COSMOS — Logs", 900., 620., (480., 320.), cx);
    match gpui_kit::open_window(options, cx, move |window, cx| {
        cx.new(|cx| logs_view::LogsView::new(ctx, service, window, cx))
    }) {
        Ok((handle, view)) => state.update(cx, |state, _| state.logs_window = Some((handle, view))),
        Err(e) => crate::logging::error("openc3-cosmos-app", &format!("Failed to open logs window: {e:#}")),
    }
}
