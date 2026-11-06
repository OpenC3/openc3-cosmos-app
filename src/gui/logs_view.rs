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

//! The separate, draggable container-logs window: the last lines of one
//! service's `docker compose logs`, read-only but selectable/copyable.

use super::ui::{self, color};
use super::worker::run_blocking;
use crate::context::Context;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{h_flex, v_flex, IconName, Sizable, StyledExt, TitleBar};
use gpui_kit::prelude::*;
use gpui_kit::{div, AppContext, Context as Cx, Entity, IntoElement, Render, Window};

/// How many trailing log lines to fetch.
const TAIL_LINES: u32 = 500;

pub struct LogsView {
    ctx: Context,
    service: String,
    content: Entity<TextareaState>,
    loading: bool,
    /// Bumped per fetch so a slow, superseded fetch can't overwrite a newer one.
    fetch_id: u64,
}

impl LogsView {
    pub fn new(ctx: Context, service: String, window: &mut Window, cx: &mut Cx<Self>) -> Self {
        let content = cx.new(|cx| TextareaState::new(window, cx));
        let mut view = Self { ctx, service: String::new(), content, loading: false, fetch_id: 0 };
        view.load(service, window, cx);
        view
    }

    /// Show `service`'s logs, fetching them on a worker thread (`docker compose
    /// logs` can take a while for a chatty container).
    pub fn load(&mut self, service: String, window: &mut Window, cx: &mut Cx<Self>) {
        self.service = service.clone();
        self.loading = true;
        self.fetch_id += 1;
        let fetch_id = self.fetch_id;
        window.set_window_title(&format!("OpenC3 COSMOS — Logs: {service}"));
        let ctx = self.ctx.clone();
        cx.spawn_in(window, async move |this, cx| {
            let text = run_blocking(move || {
                crate::docker::capture_logs(&ctx, &service, TAIL_LINES)
                    .unwrap_or_else(|e| format!("Failed to fetch logs: {e}"))
            })
            .await
            .unwrap_or_default();
            let _ = this.update_in(cx, |view, window, cx| {
                if view.fetch_id != fetch_id {
                    return;
                }
                view.loading = false;
                view.content.update(cx, |state, cx| state.set_value(text, window, cx));
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

impl Render for LogsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Cx<Self>) -> impl IntoElement {
        let mono = ui::mono(cx);
        let title = h_flex()
            .gap_2()
            .items_center()
            .child(ui::icon_badge(IconName::FileText, color::accent()))
            .child(div().text_sm().font_semibold().child("Container logs"))
            .child(ui::tag(self.service.clone()));
        let refresh = Button::new("refresh-logs")
            .small()
            .ghost()
            .icon(IconName::RefreshCw)
            .label("Refresh")
            .loading(self.loading)
            .on_click(cx.listener(|view, _, window, cx| {
                let service = view.service.clone();
                view.load(service, window, cx);
            }));
        let close = Button::new("close-logs")
            .small()
            .ghost()
            .label("Close")
            .on_click(|_, window, _| window.remove_window());

        v_flex()
            .size_full()
            .bg(color::bg())
            .text_color(color::text())
            .child(
                TitleBar::new().child(
                    h_flex()
                        .w_full()
                        .pr_2()
                        .justify_between()
                        .items_center()
                        .child(title)
                        .child(h_flex().gap_1().child(refresh).child(close)),
                ),
            )
            .child(
                div().flex_1().min_h_0().p_3().child(
                    div()
                        .relative()
                        .size_full()
                        .child(
                            Textarea::new(&self.content)
                                .readonly(true)
                                .h_full()
                                .font_family(mono)
                                .text_xs(),
                        )
                        .when(self.loading, |this| {
                            this.child(
                                h_flex()
                                    .absolute()
                                    .top_2()
                                    .right_3()
                                    .gap_2()
                                    .items_center()
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .bg(color::surface_2())
                                    .text_xs()
                                    .text_color(color::muted())
                                    .child(Spinner::new().xsmall())
                                    .child("Loading logs…"),
                            )
                        }),
                ),
            )
    }
}
