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

//! The main window: splash → setup → dashboard, the Settings sheet, and every
//! modal prompt. Pure presentation — all state and actions live in
//! [`AppState`]; this view owns only the text-input widgets (which GPUI ties to
//! a window).

use super::app_state::{level_rank, AppState, LogLevelFilter, Page};
use super::ui::{self, color};
use crate::monitor::{self, ContainerStatus, RunState};
use crate::settings::{Edition, DEFAULT_COSMOS_URL};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{h_flex, v_flex, Disableable, Icon, Sizable, StyledExt, TitleBar};
use gpui_kit::prelude::*;
use gpui_kit::{
    div, img, px, relative, Animation, AnimationExt, AnyElement, App, AppContext, ClickEvent,
    Context as Cx, Div, Entity, Hsla, Image, ImageFormat, IntoElement, PathPromptOptions, Render,
    SharedString, Subscription, Window,
};
use std::sync::Arc;
use std::time::Duration;

/// The app-tile logo shown on the splash and setup pages (embedded PNG).
const LOGO_PNG: &[u8] = include_bytes!("../../assets/icons/512x512.png");

/// Maximum rows rendered in the log table (newest first).
const MAX_LOG_ROWS: usize = 300;

type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

pub struct MainView {
    state: Entity<AppState>,
    logo: Arc<Image>,
    log_search: Entity<InputState>,
    cosmos_url: Entity<InputState>,
    enterprise_token: Entity<InputState>,
    dev_folder: Entity<InputState>,
    dev_folder_enterprise: Entity<InputState>,
    bridge_token: Entity<TextareaState>,
    cleanup_input: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl MainView {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Cx<Self>) -> Self {
        let settings = state.read(cx).settings.clone();
        let log_search = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Search")
                .default_value(state.read(cx).log_search.clone())
        });
        let cosmos_url = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(DEFAULT_COSMOS_URL)
                .default_value(settings.cosmos_url.clone())
        });
        let enterprise_token = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Enterprise access token")
                .masked(true)
                .default_value(settings.enterprise_token.clone())
        });
        let dev_folder = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Folder containing openc3.sh")
                .default_value(settings.dev_folder.clone())
        });
        let dev_folder_enterprise = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Enterprise folder containing openc3.sh")
                .default_value(settings.dev_folder_enterprise.clone())
        });
        let bridge_token = cx.new(|cx| {
            TextareaState::new(window, cx).placeholder("Paste an enrollment token").rows(4)
        });
        let cleanup_input = cx.new(|cx| InputState::new(window, cx).placeholder("cleanup"));

        let mut subscriptions = vec![cx.observe(&state, |_, _, cx| cx.notify())];
        // Mirror text-field edits into the model.
        let mut on_change = |input: &Entity<InputState>, apply: fn(&mut AppState, String, &mut Cx<AppState>)| {
            let state = state.clone();
            subscriptions.push(cx.subscribe(input, move |_, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let value = input.read(cx).value().to_string();
                    state.update(cx, |state, cx| apply(state, value, cx));
                }
            }));
        };
        on_change(&log_search, |s, v, cx| s.set_log_search(v, cx));
        on_change(&cosmos_url, |s, v, _| s.set_cosmos_url(v));
        on_change(&enterprise_token, |s, v, cx| s.set_enterprise_token(v, cx));
        on_change(&dev_folder, |s, v, _| s.set_dev_folder(v));
        on_change(&dev_folder_enterprise, |s, v, _| s.set_dev_folder_enterprise(v));
        // The cleanup button enables once "cleanup" is typed; Enter confirms.
        subscriptions.push(cx.subscribe(&cleanup_input, |view, input, event: &InputEvent, cx| {
            match event {
                InputEvent::Change => cx.notify(),
                InputEvent::PressEnter { .. } if input.read(cx).value().trim() == "cleanup" => {
                    view.state.update(cx, |s, cx| s.confirm_cleanup(cx));
                }
                _ => {}
            }
        }));
        subscriptions.push(cx.observe(&bridge_token, |_, _, cx| cx.notify()));

        // The window's close button: hide to the tray, or confirm quitting
        // where there is no tray (Linux).
        let close_state = state.clone();
        window.on_window_should_close(cx, move |_, cx| {
            close_state.update(cx, |state, cx| state.main_window_should_close(cx))
        });

        Self {
            state,
            logo: Arc::new(Image::from_bytes(ImageFormat::Png, LOGO_PNG.to_vec())),
            log_search,
            cosmos_url,
            enterprise_token,
            dev_folder,
            dev_folder_enterprise,
            bridge_token,
            cleanup_input,
            _subscriptions: subscriptions,
        }
    }

    /// A click handler that runs `f` on the model.
    fn act(&self, f: impl Fn(&mut AppState, &mut Cx<AppState>) + 'static) -> ClickHandler {
        let state = self.state.clone();
        Box::new(move |_, _, cx| state.update(cx, |state, cx| f(state, cx)))
    }

    /// Choose a development folder with the native picker (asynchronous — the
    /// dialog never blocks the event loop).
    fn browse_dev_folder(&mut self, enterprise: bool, window: &mut Window, cx: &mut Cx<Self>) {
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(if enterprise {
                "Choose the COSMOS Enterprise development folder".into()
            } else {
                "Choose the COSMOS development folder".into()
            }),
        });
        let input = if enterprise { self.dev_folder_enterprise.clone() } else { self.dev_folder.clone() };
        let state = self.state.clone();
        cx.spawn_in(window, async move |_, cx| {
            let Ok(Ok(Some(paths))) = prompt.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let path = path.to_string_lossy().into_owned();
            let _ = cx.update(|window, cx| {
                input.update(cx, |input, cx| input.set_value(path.clone(), window, cx));
                state.update(cx, |state, cx| state.pick_dev_folder(enterprise, path, cx));
            });
        })
        .detach();
    }

    fn submit_bridge_token(&mut self, window: &mut Window, cx: &mut Cx<Self>) {
        let token = self.bridge_token.read(cx).value().to_string();
        let submitted = self.state.update(cx, |state, cx| state.submit_bridge_token(token, cx));
        if submitted {
            self.bridge_token.update(cx, |input, cx| input.set_value("", window, cx));
        }
    }

    fn open_cleanup(&mut self, window: &mut Window, cx: &mut Cx<Self>) {
        self.cleanup_input.update(cx, |input, cx| input.set_value("", window, cx));
        self.state.update(cx, |state, cx| state.show_cleanup(cx));
        self.cleanup_input.update(cx, |input, cx| input.focus(window, cx));
    }

    // ---------------------------------------------------------------------
    // Chrome
    // ---------------------------------------------------------------------

    fn title_bar(&self, s: &AppState) -> TitleBar {
        let mut right = h_flex().gap_2().items_center().pr_2();
        if s.page == Page::Main {
            if s.busy() {
                right = right.child(ui::pill("Working", color::info()));
            } else if s.settings.run_locally {
                let (label, tint) = cosmos_state(s);
                right = right.child(ui::pill(label, tint));
            }
            right = right.child(
                Button::new("open-settings")
                    .ghost()
                    .small()
                    .icon(IconName::Settings)
                    .tooltip("Settings")
                    .on_click(self.act(|s, cx| s.open_settings(cx))),
            );
        }
        let close_state = self.state.clone();
        TitleBar::new()
            // Linux client-side decorations: the X asks before quitting.
            .on_close_window(move |_, _, cx| {
                close_state.update(cx, |state, cx| {
                    state.main_window_should_close(cx);
                })
            })
            .child(
                h_flex()
                    .w_full()
                    .justify_between()
                    .items_center()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(img(self.logo.clone()).size(px(18.)))
                            .child(div().text_sm().font_semibold().child("OpenC3 COSMOS"))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(color::faint())
                                    .child(format!("v{}", crate::update::current_version())),
                            ),
                    )
                    .child(right),
            )
    }

    // ---------------------------------------------------------------------
    // Pages
    // ---------------------------------------------------------------------

    fn splash(&self) -> AnyElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_6()
            .child(
                img(self.logo.clone())
                    .size(px(168.))
                    .with_animation(
                        "splash-logo",
                        Animation::new(Duration::from_millis(700)).with_easing(gpui_kit::ease_in_out),
                        |logo, delta| logo.opacity(delta),
                    ),
            )
            .child(
                v_flex()
                    .items_center()
                    .gap_1()
                    .child(div().text_2xl().font_semibold().child("OpenC3 COSMOS"))
                    .child(ui::help("Checking your environment…")),
            )
            .child(
                div()
                    .w(px(280.))
                    .h(px(4.))
                    .rounded_full()
                    .bg(color::surface_2())
                    .overflow_hidden()
                    .child(
                        div().h_full().rounded_full().bg(color::accent()).with_animation(
                            "splash-bar",
                            Animation::new(Duration::from_millis(1400))
                                .with_easing(gpui_kit::ease_in_out),
                            |bar, delta| bar.w(relative(delta)),
                        ),
                    ),
            )
            .into_any_element()
    }

    /// Setup page: a checklist of the components COSMOS needs, with an install
    /// action on each missing one.
    fn setup(&self, s: &AppState, cx: &App) -> AnyElement {
        let env = s.env.clone().expect("setup is shown after the first environment check");
        let busy = s.busy();
        let local = s.settings.run_locally;
        let mut items = v_flex().gap_3();

        // Which edition to install (only relevant to a local install).
        if local {
            items = items.child(setup_item(
                IconName::Layers,
                "Edition",
                ui::help("Choose which COSMOS edition to install."),
                self.edition_picker(s, "setup").into_any_element(),
            ));
        }
        // Hardware virtualization must be enabled in BIOS/UEFI for Docker's
        // backend; we can only detect and inform. Only warn when sure it's off.
        if local && env.virtualization == Some(false) {
            items = items.child(setup_item(
                IconName::TriangleAlert,
                "Virtualization disabled in BIOS",
                v_flex()
                    .gap_1()
                    .child(ui::help(
                        "Hardware virtualization (Intel VT-x / AMD-V) is turned off in your computer's \
                         firmware. Docker's WSL2 backend can't run until it's on — this is a BIOS/UEFI \
                         setting the app can't change for you.",
                    ))
                    .child(ui::help(
                        "Restart, enter BIOS/UEFI setup (often F2/F10/Del at boot), enable \
                         Virtualization / VT-x / SVM (usually under CPU or Advanced), save, and reboot.",
                    )),
                ui::pill("BIOS setting", color::warning()).into_any_element(),
            ));
        }
        // Windows needs the WSL2 / virtualization optional features before
        // Docker's backend will run. A prerequisite, so shown before Docker.
        if local && cfg!(target_os = "windows") {
            items = items.child(setup_item(
                IconName::Wrench,
                "Windows features",
                v_flex()
                    .gap_1()
                    .child(ui::help(
                        "Docker's WSL2 backend needs Virtual Machine Platform, Windows Hypervisor \
                         Platform and Windows Subsystem for Linux.",
                    ))
                    .child(ui::help("Enabling requires administrator approval and a Windows restart.")),
                if env.windows_features_ok {
                    ready_pill()
                } else {
                    Button::new("enable-features")
                        .primary()
                        .label("Enable")
                        .disabled(busy)
                        .on_click(self.act(|s, cx| s.enable_windows_features(cx)))
                        .into_any_element()
                },
            ));
        }
        // Docker and the COSMOS environment are only needed for a local COSMOS.
        if local {
            let mut desc = v_flex().gap_1().child(ui::help("The container engine COSMOS runs in."));
            if !env.container_ok
                && !env.container_installed
                && cfg!(any(target_os = "macos", target_os = "windows"))
            {
                desc = desc.child(
                    div().text_xs().text_color(color::faint()).child(crate::install::DOCKER_DESKTOP_LICENSE),
                );
            }
            let action = if env.container_ok {
                ready_pill()
            } else if env.container_installed {
                // Installed but its daemon isn't running: start it, don't reinstall.
                Button::new("start-docker")
                    .primary()
                    .label("Start Docker")
                    .disabled(busy)
                    .on_click(self.act(|s, cx| s.start_docker(cx)))
                    .into_any_element()
            } else {
                Button::new("install-docker")
                    .primary()
                    .label("Install Docker")
                    .disabled(busy)
                    .on_click(self.act(|s, cx| s.install_docker(cx)))
                    .into_any_element()
            };
            let title = if env.container_installed && !env.container_ok {
                "Docker (installed, not running)"
            } else {
                "Docker"
            };
            items = items.child(setup_item(IconName::Container, title, desc, action));
        }
        items = items.child(setup_item(
            IconName::Terminal,
            "Python runtime",
            ui::help("An isolated interpreter for host interfaces and scripts."),
            if env.python_ok {
                ready_pill()
            } else {
                Button::new("install-python")
                    .primary()
                    .label("Install Python")
                    .disabled(busy)
                    .on_click(self.act(|s, cx| s.install_python(cx)))
                    .into_any_element()
            },
        ));
        if local {
            let enterprise = s.settings.edition.is_enterprise();
            let mut desc = v_flex().gap_2();
            if enterprise {
                desc = desc
                    .child(ui::help(
                        "COSMOS Enterprise from repos.openc3.com (requires an access token).",
                    ))
                    .when(!env.cosmos_ok, |d| {
                        d.child(Input::new(&self.enterprise_token).small().mask_toggle())
                    });
            } else {
                desc = desc.child(ui::help("The COSMOS containers and configuration."));
            }
            let token_missing = enterprise && s.settings.enterprise_token.trim().is_empty();
            items = items.child(setup_item(
                IconName::Package,
                "COSMOS environment",
                desc,
                if env.cosmos_ok {
                    ready_pill()
                } else {
                    Button::new("install-cosmos")
                        .primary()
                        .label("Install COSMOS")
                        .disabled(busy || token_missing)
                        .on_click(self.act(|s, cx| s.install_cosmos(cx)))
                        .into_any_element()
                },
            ));
        }

        // Recent log lines so install progress is visible here. INFO and above:
        // DEBUG lines (e.g. the repeated Docker-API fallback notice expected
        // while Docker isn't up yet) are noise during setup.
        let mono = ui::mono(cx);
        let recent: Vec<_> = s
            .log_records
            .iter()
            .filter(|r| level_rank(&r.level) >= level_rank("INFO"))
            .collect();
        let start = recent.len().saturating_sub(10);
        let log_panel = v_flex()
            .gap_1()
            .p_3()
            .rounded(px(10.))
            .bg(color::surface())
            .border_1()
            .border_color(color::border())
            .font_family(mono)
            .text_xs()
            .children(recent[start..].iter().map(|rec| {
                h_flex()
                    .gap_2()
                    .child(div().w(px(44.)).flex_shrink_0().text_color(level_color(&rec.level)).child(rec.level.clone()))
                    .child(div().flex_1().min_w_0().text_color(color::muted()).child(rec.message.clone()))
            }))
            .when(start == recent.len(), |p| p.child(div().text_color(color::faint()).child("No activity yet.")));

        let content = v_flex()
            .w_full()
            .max_w(px(680.))
            .gap_5()
            .py_8()
            .px_6()
            .child(
                h_flex()
                    .gap_4()
                    .items_center()
                    .child(img(self.logo.clone()).size(px(64.)))
                    .child(
                        v_flex()
                            .gap_1()
                            .child(div().text_2xl().font_semibold().child("Set up OpenC3 COSMOS"))
                            .child(ui::help(if local {
                                "Install the components COSMOS needs, then continue."
                            } else {
                                "Using a remote COSMOS — only the host Python runtime is needed."
                            })),
                    ),
            )
            .child(items)
            .when_some(s.task.as_ref(), |c, task| c.child(task_banner(task)))
            .child(
                v_flex()
                    .gap_2()
                    .child(div().text_xs().font_medium().text_color(color::muted()).child("ACTIVITY"))
                    .child(log_panel),
            )
            .child(
                h_flex().justify_center().child(
                    Button::new("skip-setup")
                        .ghost()
                        .label("Skip for now")
                        .on_click(self.act(|s, cx| s.skip_setup(cx))),
                ),
            );

        div()
            .id("setup-scroll")
            .size_full()
            .overflow_y_scrollbar()
            .child(h_flex().w_full().justify_center().child(content))
            .into_any_element()
    }

    /// The main dashboard.
    fn dashboard(&self, s: &AppState, cx: &App) -> AnyElement {
        let mut body = v_flex().gap_4().p_5().child(self.hero(s));
        body = body.child(self.stats(s));
        if s.settings.run_locally {
            body = body.child(self.containers(s, cx));
        }
        body = body.child(self.bridge(s, cx)).child(self.activity_log(s, cx));

        div()
            .id("dashboard-scroll")
            .size_full()
            .overflow_y_scrollbar()
            .child(body)
            .into_any_element()
    }

    /// The headline card: what COSMOS is doing and the main action.
    fn hero(&self, s: &AppState) -> Div {
        let url = s.settings.effective_cosmos_url().to_string();
        let local = s.settings.run_locally;
        let started = local && s.statuses.iter().any(|c| c.is_running());
        let busy = s.busy();

        let (icon, tint, title, subtitle): (IconName, Hsla, &str, String) = if !local {
            (IconName::Cloud, color::accent(), "Remote COSMOS", url.clone())
        } else if !s.status_loaded {
            (IconName::Activity, color::muted(), "Checking COSMOS…", "Reading container status.".into())
        } else if !started {
            (IconName::Power, color::muted(), "COSMOS is stopped", "Start the containers to launch COSMOS.".into())
        } else if s.cosmos_ready {
            (IconName::CircleCheckBig, color::success(), "COSMOS is running", url.clone())
        } else {
            (IconName::Rocket, color::warning(), "COSMOS is starting…", format!("Waiting for the web interface at {url}"))
        };

        let primary = if !local || s.cosmos_ready && started {
            Button::new("open-cosmos")
                .primary()
                .large()
                .icon(IconName::SquareArrowOutUpRight)
                .label("Open COSMOS")
                .on_click(self.act(|s, cx| s.open_browser(cx)))
        } else if !started {
            Button::new("start-cosmos")
                .primary()
                .large()
                .icon(IconName::Play)
                .label("Start COSMOS")
                .disabled(busy || !s.status_loaded)
                .on_click(self.act(|s, cx| s.start_cosmos(cx)))
        } else {
            Button::new("waiting-cosmos")
                .secondary()
                .large()
                .loading(true)
                .label("Waiting for COSMOS…")
                .disabled(true)
        };

        let mut actions = h_flex().gap_2().items_center().flex_shrink_0();
        if started {
            actions = actions.child(
                Button::new("stop-cosmos")
                    .ghost()
                    .large()
                    .icon(IconName::CircleStop)
                    .label("Shut down")
                    .disabled(busy)
                    .on_click(self.act(|s, cx| s.request_shutdown(cx))),
            );
        }
        actions = actions.child(primary);

        let hero_tint = tint;
        ui::card()
            .p_5()
            .gap_4()
            .child(
                h_flex()
                    .gap_4()
                    .items_center()
                    .child(
                        div()
                            .flex()
                            .flex_shrink_0()
                            .items_center()
                            .justify_center()
                            .size(px(52.))
                            .rounded(px(14.))
                            .bg(hero_tint.opacity(0.14))
                            .text_color(hero_tint)
                            .child(Icon::new(icon).large()),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(div().text_xl().font_semibold().child(title))
                            .child(div().text_sm().text_color(color::muted()).truncate().child(subtitle)),
                    )
                    .child(actions),
            )
            .when_some(s.task.as_ref(), |c, task| c.child(task_banner(task)))
    }

    fn stats(&self, s: &AppState) -> Div {
        let mut row = h_flex().gap_4().flex_wrap();
        if s.settings.run_locally {
            let visible = visible_containers(&s.statuses);
            let running = visible.iter().filter(|c| c.is_running()).count();
            let healthy = visible.iter().filter(|c| c.is_running() && c.is_healthy()).count();
            let value = if s.status_loaded { format!("{running} / {}", visible.len()) } else { "—".into() };
            let tint = if !s.status_loaded || visible.is_empty() {
                color::muted()
            } else if running == visible.len() {
                color::success()
            } else {
                color::warning()
            };
            row = row.child(ui::stat_tile(
                IconName::Boxes,
                tint,
                "Containers",
                value,
                if running > 0 { format!("{healthy} healthy") } else { "none running".into() },
            ));
        }
        let running_ms = s.microservices.iter().filter(|m| m.running).count();
        row = row.child(ui::stat_tile(
            IconName::Server,
            if running_ms > 0 { color::success() } else { color::muted() },
            "Host interfaces",
            format!("{running_ms}"),
            format!("{} configured", s.microservices.len()),
        ));
        let (bridge_label, bridge_tint) = bridge_state(s);
        row = row.child(ui::stat_tile(
            IconName::Cable,
            bridge_tint,
            "Bridge",
            bridge_label,
            if s.bridge_status.connected { "linked to COSMOS".to_string() } else { "host ↔ COSMOS link".to_string() },
        ));
        row.child(ui::stat_tile(
            IconName::Package,
            color::accent(),
            "COSMOS version",
            s.cosmos_version_label().unwrap_or_else(|| "remote".into()),
            format!("app v{}", crate::update::current_version()),
        ))
    }

    /// Collapsible container status table.
    fn containers(&self, s: &AppState, cx: &App) -> Div {
        let visible = visible_containers(&s.statuses);
        let running = visible.iter().filter(|c| c.is_running()).count();
        let collapsed = s.status_collapsed;
        let summary = if s.status_loaded {
            format!("{running} of {} running", visible.len())
        } else {
            "loading…".to_string()
        };

        let header = h_flex()
            .id("containers-header")
            .gap_2p5()
            .items_center()
            .cursor_pointer()
            .rounded(px(8.))
            .on_click(self.act(|s, cx| s.toggle_status(cx)))
            .child(ui::icon_badge(IconName::Container, color::accent()))
            .child(div().text_base().font_semibold().child("Containers"))
            .child(ui::tag(summary))
            .child(div().flex_1())
            .child(
                div().text_color(color::muted()).child(
                    Icon::new(if collapsed { IconName::ChevronRight } else { IconName::ChevronDown }).small(),
                ),
            );

        let mut card = ui::card().child(header);
        if collapsed {
            return card;
        }
        if !s.status_loaded {
            return card.child(
                h_flex()
                    .gap_2()
                    .py_4()
                    .justify_center()
                    .text_sm()
                    .text_color(color::muted())
                    .child(Spinner::new().small())
                    .child("Loading container status…"),
            );
        }
        if visible.is_empty() {
            return card.child(empty_note("No containers found."));
        }

        let mono = ui::mono(cx);
        let mut table = v_flex()
            .rounded(px(10.))
            .border_1()
            .border_color(color::border())
            .overflow_hidden()
            .child(
                table_header()
                    .child(th("Container").flex_1().min_w_0())
                    .child(th("Tag").w(px(110.)))
                    .child(th("Status").w(px(200.)))
                    .child(th("CPU").w(px(70.)))
                    .child(th("Memory").w(px(110.)))
                    .child(th("").w(px(64.))),
            );
        for (i, c) in visible.iter().enumerate() {
            let (label, tint) = state_style(c);
            let service = c.service.clone();
            let state = self.state.clone();
            table = table.child(
                table_row(i)
                    .child(td_mono(c.service.clone(), &mono).flex_1().min_w_0())
                    .child(td_mono(c.tag_display().to_string(), &mono).w(px(110.)))
                    .child(div().w(px(200.)).child(ui::pill(label, tint)))
                    .child(td_mono(c.cpu_display().to_string(), &mono).w(px(70.)))
                    .child(td_mono(c.mem_display().to_string(), &mono).w(px(110.)))
                    .child(
                        div().w(px(64.)).child(
                            Button::new(SharedString::from(format!("logs-{}", c.service)))
                                .xsmall()
                                .ghost()
                                .icon(IconName::ScrollText)
                                .label("Logs")
                                .on_click(move |_, _, cx| {
                                    state.update(cx, |s, cx| s.view_logs(service.clone(), cx))
                                }),
                        ),
                    ),
            );
        }
        let (cpu_total, mem_total) = monitor::totals(visible.iter().copied());
        table = table.child(
            h_flex()
                .px_3()
                .py_2()
                .gap_3()
                .bg(color::surface_2())
                .text_xs()
                .child(div().flex_1().font_semibold().text_color(color::muted()).child("Total"))
                .child(div().w(px(110.)))
                .child(div().w(px(200.)))
                .child(td_mono(cpu_total, &mono).w(px(70.)))
                .child(td_mono(mem_total, &mono).w(px(110.)))
                .child(div().w(px(64.))),
        );
        card = card.child(table);
        card
    }

    /// Bridge connection + host microservices.
    fn bridge(&self, s: &AppState, cx: &App) -> Div {
        let (label, tint) = bridge_state(s);
        let message = if s.bridge_status.message.is_empty() {
            "Not paired with COSMOS".to_string()
        } else {
            s.bridge_status.message.clone()
        };
        let mut header = ui::card_header(IconName::Cable, "Bridge").child(ui::pill(label, tint)).child(div().flex_1());
        // Offer a manual retry whenever the bridge isn't connected (auto-enroll
        // gives up after a few tries per COSMOS up-session).
        if s.bridge_enabled() && !s.bridge_status.connected {
            header = header.child(
                Button::new("retry-bridge")
                    .small()
                    .ghost()
                    .icon(IconName::RotateCcw)
                    .label("Retry")
                    .on_click(self.act(|s, cx| s.retry_bridge(cx))),
            );
        }
        let mut card = ui::card().child(header).child(
            h_flex()
                .gap_2()
                .items_start()
                .px_3()
                .py_2()
                .rounded(px(8.))
                .bg(color::surface_2())
                .text_sm()
                .child(div().pt(px(6.)).child(ui::dot(tint)))
                .child(div().flex_1().min_w_0().text_color(color::muted()).child(message)),
        );

        if s.microservices.is_empty() {
            return card.child(empty_note("No host microservices running."));
        }
        let mono = ui::mono(cx);
        let mut table = v_flex()
            .rounded(px(10.))
            .border_1()
            .border_color(color::border())
            .overflow_hidden()
            .child(
                table_header()
                    .child(th("Microservice").flex_1().min_w_0())
                    .child(th("State").w(px(110.)))
                    .child(th("PID").w(px(70.)))
                    .child(th("Connection").w(px(140.)))
                    .child(th("Rx").w(px(90.)))
                    .child(th("Tx").w(px(90.))),
            );
        for (i, m) in s.microservices.iter().enumerate() {
            let (state_label, state_tint) =
                if m.running { ("running", color::success()) } else { ("stopped", color::danger()) };
            let connection: AnyElement = match m.interface_state.as_deref() {
                Some("CONNECTED") => ui::pill("CONNECTED", color::success()).into_any_element(),
                Some(other) if !other.is_empty() => ui::pill(other.to_string(), color::warning()).into_any_element(),
                _ => div().text_color(color::faint()).child("—").into_any_element(),
            };
            let bytes = |v: Option<u64>| v.map(crate::monitor::human_bytes).unwrap_or_else(|| "—".to_string());
            table = table.child(
                table_row(i)
                    .child(td_mono(m.name.clone(), &mono).flex_1().min_w_0())
                    .child(div().w(px(110.)).child(ui::pill(state_label, state_tint)))
                    .child(td_mono(m.pid.map(|p| p.to_string()).unwrap_or_else(|| "—".into()), &mono).w(px(70.)))
                    .child(div().w(px(140.)).child(connection))
                    .child(td_mono(bytes(m.rxbytes), &mono).w(px(90.)))
                    .child(td_mono(bytes(m.txbytes), &mono).w(px(90.))),
            );
        }
        card = card.child(table);
        card
    }

    /// Everything openc3-cosmos-app logs, newest first, filterable by level and
    /// search, with pause/clear. Modeled on COSMOS' LogMessages.vue.
    fn activity_log(&self, s: &AppState, cx: &App) -> Div {
        let mut levels = h_flex().gap_0p5().p_0p5().rounded(px(8.)).bg(color::surface_2());
        for level in LogLevelFilter::ALL {
            let selected = s.log_level == level;
            let button = Button::new(SharedString::from(format!("level-{}", level.label())))
                .xsmall()
                .label(level.label())
                .on_click(self.act(move |s, cx| s.set_log_level(level, cx)));
            levels = levels.child(if selected { button.primary() } else { button.ghost() });
        }
        let header = ui::card_header(IconName::ScrollText, "Activity")
            .child(div().flex_1())
            .child(levels)
            .child(
                div().w(px(200.)).child(
                    Input::new(&self.log_search)
                        .small()
                        .cleanable(true)
                        .prefix(Icon::new(IconName::Search).xsmall().text_color(color::muted())),
                ),
            )
            .child(
                Button::new("pause-log")
                    .small()
                    .ghost()
                    .icon(if s.log_paused { IconName::Play } else { IconName::Pause })
                    .tooltip(if s.log_paused { "Resume" } else { "Pause" })
                    .on_click(self.act(|s, cx| s.toggle_log_pause(cx))),
            )
            .child(
                Button::new("clear-log")
                    .small()
                    .ghost()
                    .icon(IconName::Trash)
                    .tooltip("Clear")
                    .on_click(self.act(|s, cx| s.clear_log(cx))),
            );

        // Filter to level >= the selected minimum and matching the search, then
        // show newest first (capped so a huge backlog doesn't bloat a frame).
        let min_rank = s.log_level.rank();
        let needle = s.log_search.to_lowercase();
        let mono = ui::mono(cx);
        let rows: Vec<AnyElement> = s
            .log_records
            .iter()
            .rev()
            .filter(|rec| level_rank(&rec.level) >= min_rank)
            .filter(|rec| {
                needle.is_empty()
                    || rec.message.to_lowercase().contains(&needle)
                    || rec.source.to_lowercase().contains(&needle)
            })
            .take(MAX_LOG_ROWS)
            .map(|rec| {
                // "2026-07-19T20:37:33.129001Z" -> "20:37:33.129"
                let time = rec.timestamp.get(11..23).unwrap_or(rec.timestamp.as_str()).to_string();
                h_flex()
                    .gap_3()
                    .items_start()
                    .px_3()
                    .py(px(3.))
                    .hover(|row| row.bg(color::surface_2()))
                    .child(div().w(px(92.)).flex_shrink_0().text_color(color::faint()).child(time))
                    .child(
                        div()
                            .w(px(52.))
                            .flex_shrink_0()
                            .font_semibold()
                            .text_color(level_color(&rec.level))
                            .child(rec.level.clone()),
                    )
                    .child(
                        div()
                            .w(px(170.))
                            .flex_shrink_0()
                            .truncate()
                            .text_color(color::muted())
                            .child(rec.source.clone()),
                    )
                    .child(div().flex_1().min_w_0().child(rec.message.clone()))
                    .into_any_element()
            })
            .collect();
        let empty = rows.is_empty();

        ui::card().child(header).child(
            div()
                .h(px(320.))
                .rounded(px(10.))
                .border_1()
                .border_color(color::border())
                .bg(color::bg())
                .overflow_hidden()
                .child(
                    v_flex()
                        .id("log-rows")
                        .size_full()
                        .py_1()
                        .font_family(mono)
                        .text_xs()
                        .overflow_y_scrollbar()
                        .children(rows)
                        .when(empty, |c| c.child(empty_note("No log messages."))),
                ),
        )
    }

    // ---------------------------------------------------------------------
    // Settings
    // ---------------------------------------------------------------------

    fn edition_picker(&self, s: &AppState, id: &str) -> Div {
        let mut picker = h_flex().gap_0p5().p_0p5().rounded(px(8.)).bg(color::surface_2());
        for edition in Edition::ALL {
            let label = match edition {
                Edition::Core => "Core",
                Edition::Enterprise => "Enterprise",
            };
            let button = Button::new(SharedString::from(format!("{id}-edition-{label}")))
                .small()
                .label(label)
                .on_click(self.act(move |s, cx| s.set_edition(edition, cx)));
            picker = picker.child(if s.settings.edition == edition { button.primary() } else { button.ghost() });
        }
        picker
    }

    fn settings_modal(&self, s: &AppState, cx: &Cx<Self>) -> AnyElement {
        let enterprise = s.settings.edition.is_enterprise();

        let mut cosmos = settings_section(IconName::Layers, "COSMOS")
            .child(field_row("Edition", self.edition_picker(s, "settings")))
            .child(
                v_flex()
                    .gap_1p5()
                    .child(label("Web address"))
                    .child(Input::new(&self.cosmos_url))
                    .child(ui::help("Opened by \u{201C}Open COSMOS\u{201D}.")),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        Switch::new("run-locally")
                            .checked(s.settings.run_locally)
                            .label("Run COSMOS locally")
                            .on_click({
                                let state = self.state.clone();
                                move |checked: &bool, _, cx| {
                                    let checked = *checked;
                                    state.update(cx, |s, cx| s.set_run_locally(checked, cx))
                                }
                            }),
                    )
                    .child(ui::help(
                        "When off, this app won't install or start/stop a local COSMOS — it just \
                         opens the address above.",
                    )),
            );
        if enterprise {
            cosmos = cosmos.child(
                v_flex()
                    .gap_1p5()
                    .child(label("Enterprise access token"))
                    .child(Input::new(&self.enterprise_token).mask_toggle())
                    .child(ui::help("Access token for repos.openc3.com (COSMOS Enterprise).")),
            );
        }

        let mut dev = settings_section(IconName::Code, "Development").child(
            v_flex()
                .gap_1()
                .child(
                    Switch::new("dev-mode")
                        .checked(s.settings.dev_mode)
                        .label("Development Mode")
                        .on_click({
                            let state = self.state.clone();
                            move |checked: &bool, _, cx| {
                                let checked = *checked;
                                state.update(cx, |s, cx| s.set_dev_mode(checked, cx))
                            }
                        }),
                )
                .child(ui::help(
                    "Forces OPENC3_TAG and OPENC3_ENTERPRISE_TAG to \"latest\" (overriding the .env) \
                     and runs COSMOS from a local source checkout's openc3.sh.",
                )),
        );
        if s.settings.dev_mode {
            // Core checkout: always the source of the openc3 Python library;
            // for Core edition also what COSMOS is run from.
            let core_label = if enterprise {
                "Core development folder (openc3 Python library)"
            } else {
                "Development folder (contains openc3.sh)"
            };
            dev = dev.child(folder_row(
                core_label,
                &self.dev_folder,
                Button::new("browse-core")
                    .icon(IconName::FolderOpen)
                    .label("Browse…")
                    .on_click(cx.listener(|view, _, window, cx| view.browse_dev_folder(false, window, cx))),
            ));
            if enterprise {
                dev = dev.child(folder_row(
                    "Enterprise development folder (contains openc3.sh)",
                    &self.dev_folder_enterprise,
                    Button::new("browse-enterprise")
                        .icon(IconName::FolderOpen)
                        .label("Browse…")
                        .on_click(cx.listener(|view, _, window, cx| view.browse_dev_folder(true, window, cx))),
                ));
            }
        }

        let versions = match &s.installed_cosmos {
            Some(v) => format!("App v{}  ·  COSMOS v{v}", crate::update::current_version()),
            None => format!("App v{}", crate::update::current_version()),
        };
        let updates = settings_section(IconName::Download, "Updates").child(
            h_flex()
                .justify_between()
                .items_center()
                .child(div().text_sm().text_color(color::muted()).child(versions))
                .child(
                    Button::new("check-updates")
                        .icon(IconName::RefreshCw)
                        .label(if s.checking_update { "Checking…" } else { "Check for updates" })
                        .loading(s.checking_update)
                        .disabled(s.checking_update)
                        .on_click(self.act(|s, cx| s.check_for_updates(cx))),
                ),
        );

        let bridge_enabled = s.bridge_enabled();
        let mut bridge = settings_section(IconName::Cable, "Bridge").child(
            v_flex()
                .gap_1()
                .child(
                    Switch::new("bridge-enabled")
                        .checked(bridge_enabled)
                        .label("Enable bridge functionality")
                        .on_click({
                            let state = self.state.clone();
                            move |checked: &bool, _, cx| {
                                let disabled = !*checked;
                                state.update(cx, |s, cx| s.set_bridge_disabled(disabled, cx))
                            }
                        }),
                )
                .child(ui::help(
                    "By default, bridge functionality is disabled before COSMOS 7.4.0 and enabled \
                     for 7.4.0 or newer.",
                )),
        );
        if bridge_enabled {
            let has_token = !self.bridge_token.read(cx).value().trim().is_empty();
            bridge = bridge.child(
                v_flex()
                    .gap_1p5()
                    .child(label("Pair with a remote COSMOS"))
                    .child(ui::help(
                        "Paste an enrollment token from a remote COSMOS (Admin → Bridges). A \
                         co-located COSMOS pairs automatically.",
                    ))
                    .child(Textarea::new(&self.bridge_token).font_family(ui::mono(cx)).text_xs())
                    .child(
                        h_flex().justify_end().child(
                            Button::new("pair-bridge")
                                .primary()
                                .icon(IconName::Link2)
                                .label(if s.bridge_pairing { "Pairing…" } else { "Pair" })
                                .loading(s.bridge_pairing)
                                .disabled(s.bridge_pairing || !has_token)
                                .on_click(cx.listener(|view, _, window, cx| view.submit_bridge_token(window, cx))),
                        ),
                    ),
            );
        }

        let danger = v_flex()
            .gap_3()
            .p_4()
            .rounded(px(10.))
            .border_1()
            .border_color(color::danger().opacity(0.4))
            .bg(color::danger().opacity(0.06))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .text_color(color::danger())
                    .child(Icon::new(IconName::TriangleAlert).small())
                    .child(div().text_sm().font_semibold().child("Danger zone")),
            )
            .child(
                h_flex()
                    .gap_4()
                    .items_center()
                    .child(div().flex_1().child(ui::help(
                        "Remove all COSMOS Docker volumes and data (targets, logs, databases). This \
                         cannot be undone.",
                    )))
                    .child(
                        Button::new("show-cleanup")
                            .danger()
                            .icon(IconName::Trash)
                            .label("Cleanup…")
                            .on_click(cx.listener(|view, _, window, cx| view.open_cleanup(window, cx))),
                    ),
            );

        let header = h_flex()
            .justify_between()
            .items_center()
            .child(ui::modal_title(IconName::Settings, color::accent(), "Settings"))
            .child(
                Button::new("close-settings")
                    .ghost()
                    .small()
                    .icon(IconName::Close)
                    .on_click(self.act(|s, cx| s.close_settings(cx))),
            );
        let body = v_flex()
            .id("settings-body")
            .flex_1()
            .min_h_0()
            .gap_6()
            .pr_2()
            .overflow_y_scrollbar()
            .child(cosmos)
            .child(dev)
            .child(updates)
            .child(bridge)
            .child(danger);
        let footer = h_flex().justify_end().child(
            Button::new("done-settings")
                .primary()
                .label("Done")
                .on_click(self.act(|s, cx| s.close_settings(cx))),
        );
        ui::modal("settings-modal", 620., v_flex().flex_1().min_h_0().gap_4().child(header).child(body).child(footer))
            .into_any_element()
    }

    // ---------------------------------------------------------------------
    // Modals
    // ---------------------------------------------------------------------

    fn overlays(&self, s: &AppState, cx: &Cx<Self>) -> Vec<AnyElement> {
        let mut overlays = Vec::new();
        if s.page == Page::Main && s.settings_open {
            overlays.push(self.settings_modal(s, cx));
        }
        if s.shutdown_confirm {
            overlays.push(confirm_modal(
                "shutdown-modal",
                IconName::Power,
                color::danger(),
                "Shut down COSMOS?",
                "This stops all COSMOS containers running locally.",
                vec![
                    Button::new("cancel-shutdown").ghost().label("Cancel").on_click(self.act(|s, cx| s.cancel_shutdown(cx))),
                    Button::new("confirm-shutdown").danger().label("Shut down COSMOS").on_click(self.act(|s, cx| s.confirm_shutdown(cx))),
                ],
            ));
        }
        if s.cleanup_confirm {
            overlays.push(self.cleanup_modal(cx));
        }
        if s.restart_pending {
            overlays.push(confirm_modal(
                "restart-modal",
                IconName::RotateCcw,
                color::warning(),
                "Restart required",
                "Windows features were enabled. They only take effect after a restart. Restart now to finish setup?",
                vec![
                    Button::new("restart-later").ghost().label("Restart later").on_click(self.act(|s, cx| s.dismiss_restart(cx))),
                    Button::new("restart-now").danger().label("Restart now").on_click(self.act(|s, cx| s.restart_windows(cx))),
                ],
            ));
        }
        if let Some(message) = &s.dialog {
            overlays.push(confirm_modal(
                "dialog-modal",
                IconName::Info,
                color::info(),
                "Next steps",
                message.clone(),
                vec![Button::new("dismiss-dialog").primary().label("OK").on_click(self.act(|s, cx| s.dismiss_dialog(cx)))],
            ));
        }
        if let Some(rel) = &s.update_available {
            overlays.push(update_modal(
                "update-modal",
                "Update available",
                "A new version of OpenC3 COSMOS is available.",
                format!("v{}", crate::update::current_version()),
                format!("v{}", rel.version),
                None,
                vec![
                    Button::new("skip-update").ghost().label("Skip this version").on_click(self.act(|s, cx| s.skip_update(cx))),
                    Button::new("later-update").secondary().label("Later").on_click(self.act(|s, cx| s.dismiss_update(cx))),
                    Button::new("install-update").primary().icon(IconName::Download).label("Install").on_click(self.act(|s, cx| s.install_update(cx))),
                ],
            ));
        }
        if let Some(rel) = &s.cosmos_update {
            overlays.push(update_modal(
                "cosmos-update-modal",
                "COSMOS update available",
                "A new version of COSMOS is available.",
                s.installed_cosmos.as_ref().map(|v| format!("v{v}")).unwrap_or_else(|| "unknown".into()),
                format!("v{}", rel.version),
                Some(
                    "Updating shuts COSMOS down, replaces it with the new version, and restarts it. \
                     Your data (in Docker volumes) and local config (.env.local, compose.override.yaml) \
                     are preserved.",
                ),
                vec![
                    Button::new("skip-cosmos-update").ghost().label("Skip this version").on_click(self.act(|s, cx| s.skip_cosmos_update(cx))),
                    Button::new("later-cosmos-update").secondary().label("Later").on_click(self.act(|s, cx| s.dismiss_cosmos_update(cx))),
                    Button::new("install-cosmos-update").primary().icon(IconName::Download).label("Update").on_click(self.act(|s, cx| s.install_cosmos_update(cx))),
                ],
            ));
        }
        if s.quit_confirm {
            overlays.push(confirm_modal(
                "quit-modal",
                IconName::Power,
                color::danger(),
                "Quit OpenC3 COSMOS?",
                "This closes the app. Host interfaces it manages will stop.",
                vec![
                    Button::new("cancel-quit").ghost().label("Cancel").on_click(self.act(|s, cx| s.cancel_quit(cx))),
                    Button::new("confirm-quit").danger().label("Quit").on_click(self.act(|s, cx| s.quit(cx))),
                ],
            ));
        }
        if s.quitting {
            overlays.push(
                ui::modal(
                    "quitting-modal",
                    360.,
                    h_flex()
                        .gap_3()
                        .items_center()
                        .child(Spinner::new())
                        .child(
                            v_flex()
                                .gap_0p5()
                                .child(div().font_semibold().child("Shutting down…"))
                                .child(ui::help("Stopping host interfaces.")),
                        ),
                )
                .into_any_element(),
            );
        }
        overlays
    }

    fn cleanup_modal(&self, cx: &Cx<Self>) -> AnyElement {
        let confirmed = self.cleanup_input.read(cx).value().trim() == "cleanup";
        let bullets = [
            "All targets, plugins, and stored configuration",
            "All command and telemetry logs",
            "The time-series database and Redis data",
        ];
        ui::modal(
            "cleanup-modal",
            520.,
            v_flex()
                .gap_4()
                .child(ui::modal_title(IconName::TriangleAlert, color::danger(), "Destroy all COSMOS data?"))
                .child(
                    v_flex()
                        .gap_1p5()
                        .text_sm()
                        .text_color(color::muted())
                        .child("This permanently removes ALL COSMOS Docker volumes and data:")
                        .children(bullets.map(|b| {
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(ui::dot(color::danger()))
                                .child(b)
                        }))
                        .child(
                            "Running containers will be stopped and their volumes deleted. This CANNOT be undone.",
                        ),
                )
                .child(
                    v_flex()
                        .gap_1p5()
                        .child(label("Type \u{201C}cleanup\u{201D} to confirm"))
                        .child(Input::new(&self.cleanup_input)),
                )
                .child(ui::modal_actions([
                    Button::new("cancel-cleanup")
                        .ghost()
                        .label("Cancel")
                        .on_click(self.act(|s, cx| s.cancel_cleanup(cx)))
                        .into_any_element(),
                    Button::new("confirm-cleanup")
                        .danger()
                        .icon(IconName::Trash)
                        .label("Delete everything")
                        .disabled(!confirmed)
                        .on_click(self.act(|s, cx| s.confirm_cleanup(cx)))
                        .into_any_element(),
                ])),
        )
        .into_any_element()
    }
}

impl Render for MainView {
    fn render(&mut self, _window: &mut Window, cx: &mut Cx<Self>) -> impl IntoElement {
        let state = self.state.clone();
        let cx: &Cx<Self> = cx;
        let s = state.read(cx);
        let title_bar = self.title_bar(s);
        let page = match s.page {
            Page::Splash => self.splash(),
            Page::Install => self.setup(s, cx),
            Page::Main => self.dashboard(s, cx),
        };
        let overlays = self.overlays(s, cx);

        v_flex()
            .size_full()
            .relative()
            .bg(color::bg())
            .text_color(color::text())
            .text_sm()
            .child(title_bar)
            .child(div().flex_1().min_h_0().child(page))
            .children(overlays)
    }
}

// -------------------------------------------------------------------------
// Small presentational helpers
// -------------------------------------------------------------------------

/// The green "Ready" marker for an installed setup component.
fn ready_pill() -> AnyElement {
    h_flex()
        .gap_1p5()
        .items_center()
        .text_color(color::success())
        .text_sm()
        .font_medium()
        .child(Icon::new(IconName::CircleCheckBig).small())
        .child("Ready")
        .into_any_element()
}

/// One row of the setup checklist.
fn setup_item(icon: IconName, title: &str, desc: impl IntoElement, action: AnyElement) -> Div {
    ui::card()
        .p_4()
        .child(
            h_flex()
                .gap_4()
                .items_center()
                .child(ui::icon_badge(icon, color::accent()))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .child(div().text_base().font_semibold().child(title.to_string()))
                        .child(desc),
                )
                .child(div().flex_shrink_0().child(action)),
        )
}

/// The running-task strip: spinner, task label, latest progress line.
fn task_banner(task: &super::app_state::RunningTask) -> Div {
    h_flex()
        .gap_3()
        .items_center()
        .px_4()
        .py_3()
        .rounded(px(10.))
        .bg(color::accent().opacity(0.08))
        .border_1()
        .border_color(color::accent().opacity(0.3))
        .child(Spinner::new().color(color::accent()))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(div().text_sm().font_semibold().child(task.label.clone()))
                .child(
                    div()
                        .text_xs()
                        .text_color(color::muted())
                        .truncate()
                        .child(task.activity.clone().unwrap_or_else(|| {
                            "This can take several minutes. Progress is shown in the activity log.".into()
                        })),
                ),
        )
}

fn settings_section(icon: IconName, title: &str) -> Div {
    v_flex()
        .gap_3()
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .pb_2()
                .border_b_1()
                .border_color(color::border())
                .child(div().text_color(color::accent()).child(Icon::new(icon).small()))
                .child(div().text_sm().font_semibold().child(title.to_string())),
        )
}

fn label(text: &str) -> Div {
    div().text_xs().font_medium().text_color(color::muted()).child(text.to_string())
}

fn field_row(name: &str, control: impl IntoElement) -> Div {
    h_flex().justify_between().items_center().child(label(name)).child(control)
}

fn folder_row(name: &str, input: &Entity<InputState>, browse: Button) -> Div {
    v_flex()
        .gap_1p5()
        .child(label(name))
        .child(h_flex().gap_2().child(div().flex_1().child(Input::new(input))).child(browse))
}

fn empty_note(text: &str) -> Div {
    h_flex()
        .justify_center()
        .py_4()
        .text_sm()
        .text_color(color::faint())
        .child(text.to_string())
}

fn table_header() -> Div {
    h_flex()
        .gap_3()
        .px_3()
        .py_2()
        .bg(color::surface_2())
        .border_b_1()
        .border_color(color::border())
}

fn th(text: &str) -> Div {
    div()
        .text_xs()
        .font_medium()
        .text_color(color::muted())
        .child(text.to_uppercase())
}

fn table_row(index: usize) -> Div {
    h_flex()
        .gap_3()
        .items_center()
        .px_3()
        .py(px(7.))
        .when(index > 0, |row| row.border_t_1().border_color(color::border()))
        .hover(|row| row.bg(color::surface_2()))
}

fn td_mono(text: impl Into<SharedString>, mono: &SharedString) -> Div {
    div().text_xs().font_family(mono.clone()).truncate().child(text.into())
}

/// A small confirmation modal: title, message and buttons.
fn confirm_modal(
    id: &'static str,
    icon: IconName,
    tint: Hsla,
    title: &str,
    message: impl Into<SharedString>,
    buttons: Vec<Button>,
) -> AnyElement {
    ui::modal(
        id,
        460.,
        v_flex()
            .gap_4()
            .child(ui::modal_title(icon, tint, title.to_string()))
            .child(div().text_sm().text_color(color::muted()).child(message.into()))
            .child(ui::modal_actions(buttons.into_iter().map(IntoElement::into_any_element))),
    )
    .into_any_element()
}

/// Installed → available version comparison modal.
fn update_modal(
    id: &'static str,
    title: &str,
    message: &str,
    installed: String,
    available: String,
    note: Option<&str>,
    buttons: Vec<Button>,
) -> AnyElement {
    let version_box = |caption: &str, version: String, tint: Hsla| {
        v_flex()
            .flex_1()
            .gap_1()
            .p_3()
            .rounded(px(10.))
            .bg(color::surface_2())
            .border_1()
            .border_color(color::border())
            .child(div().text_xs().text_color(color::muted()).child(caption.to_string()))
            .child(div().text_lg().font_semibold().text_color(tint).child(version))
    };
    ui::modal(
        id,
        500.,
        v_flex()
            .gap_4()
            .child(ui::modal_title(IconName::Sparkles, color::accent(), title.to_string()))
            .child(div().text_sm().text_color(color::muted()).child(message.to_string()))
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(version_box("Installed", installed, color::text()))
                    .child(div().text_color(color::faint()).child(Icon::new(IconName::ArrowRight).small()))
                    .child(version_box("Available", available, color::success())),
            )
            .when_some(note, |c, note| c.child(ui::help(note.to_string())))
            .child(ui::modal_actions(buttons.into_iter().map(IntoElement::into_any_element))),
    )
    .into_any_element()
}

/// Containers to show: the one-shot init container is hidden once it has
/// completed successfully (still shown while running or if it failed).
fn visible_containers(statuses: &[ContainerStatus]) -> Vec<&ContainerStatus> {
    statuses
        .iter()
        .filter(|c| !(c.service == "openc3-cosmos-init" && c.run_state() == RunState::ExitedSuccess))
        .collect()
}

/// Overall local COSMOS state for the title-bar pill.
fn cosmos_state(s: &AppState) -> (&'static str, Hsla) {
    let started = s.statuses.iter().any(|c| c.is_running());
    if !s.status_loaded {
        ("Checking", color::muted())
    } else if !started {
        ("Stopped", color::muted())
    } else if s.cosmos_ready {
        ("Running", color::success())
    } else {
        ("Starting", color::warning())
    }
}

/// Bridge connection label + color.
fn bridge_state(s: &AppState) -> (&'static str, Hsla) {
    if !s.bridge_enabled() {
        ("Disabled", color::faint())
    } else if s.bridge_status.connected {
        ("Connected", color::success())
    } else if s.bridge_status.configured {
        ("Connecting", color::warning())
    } else {
        ("Not paired", color::muted())
    }
}

/// True when a running container has been up for less than a minute. Docker
/// reports sub-minute uptime in seconds ("Up 12 seconds", "Up Less than a
/// second"), switching to minutes/hours afterward.
fn started_recently(c: &ContainerStatus) -> bool {
    let s = c.status.to_lowercase();
    s.contains("up") && s.contains("second")
}

/// A container's status label and color: freshly started containers show
/// yellow, running-but-unhealthy orange, healthy green, failed red.
fn state_style(c: &ContainerStatus) -> (String, Hsla) {
    let tint = match c.run_state() {
        RunState::Running => {
            if started_recently(c) {
                color::yellow()
            } else if c.health.eq_ignore_ascii_case("unhealthy") || c.health.eq_ignore_ascii_case("starting") {
                color::orange()
            } else {
                color::success()
            }
        }
        RunState::ExitedSuccess => color::success(),
        RunState::ExitedFailure => color::danger(),
        RunState::Restarting => color::orange(),
        RunState::Paused | RunState::Unknown => color::muted(),
    };
    (c.display_status(), tint)
}

/// Display color for a log level.
fn level_color(level: &str) -> Hsla {
    match level {
        "DEBUG" => color::faint(),
        "WARN" => color::warning(),
        "ERROR" | "FATAL" => color::danger(),
        _ => color::success(),
    }
}
