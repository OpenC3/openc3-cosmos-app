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

//! The app's visual language: palette, theme setup, and small reusable pieces
//! (cards, pills, stat tiles, modals) shared by the views.

use gpui_kit::component::{h_flex, v_flex, ActiveTheme, Icon, Sizable, StyledExt, Theme, ThemeMode};
use gpui_kit::prelude::*;
use gpui_kit::{
    div, px, rgb, AnyElement, App, Div, Hsla, IntoElement, ParentElement, SharedString, Stateful,
    Styled,
};

/// Brand palette (dark). The gpui-component theme is tuned to match in
/// [`apply_theme`] so stock components blend in.
pub mod color {
    use gpui_kit::{rgb, Hsla};

    fn hex(v: u32) -> Hsla {
        rgb(v).into()
    }
    /// Window background.
    pub fn bg() -> Hsla {
        hex(0x0b0e14)
    }
    /// Card surface.
    pub fn surface() -> Hsla {
        hex(0x121620)
    }
    /// Raised surface inside a card (table header, inset panels).
    pub fn surface_2() -> Hsla {
        hex(0x181d29)
    }
    pub fn border() -> Hsla {
        hex(0x242a38)
    }
    pub fn text() -> Hsla {
        hex(0xe7eaf0)
    }
    pub fn muted() -> Hsla {
        hex(0x8b93a7)
    }
    pub fn faint() -> Hsla {
        hex(0x5b6378)
    }
    /// OpenC3 accent blue.
    pub fn accent() -> Hsla {
        hex(0x3b82f6)
    }
    pub fn accent_hover() -> Hsla {
        hex(0x2f6fe0)
    }
    pub fn accent_active() -> Hsla {
        hex(0x255fc9)
    }
    pub fn success() -> Hsla {
        hex(0x22c55e)
    }
    pub fn warning() -> Hsla {
        hex(0xf59e0b)
    }
    pub fn orange() -> Hsla {
        hex(0xfb923c)
    }
    pub fn yellow() -> Hsla {
        hex(0xfacc15)
    }
    pub fn danger() -> Hsla {
        hex(0xef4444)
    }
    pub fn info() -> Hsla {
        hex(0x38bdf8)
    }
}

/// Dark theme with the brand palette applied to the stock components.
pub fn apply_theme(cx: &mut App) {
    Theme::change(ThemeMode::Dark, None, cx);
    Theme::update(cx, |theme| {
        theme.font_size = px(14.);
        theme.radius = px(8.);
        theme.radius_lg = px(12.);
        theme.background = color::bg();
        theme.foreground = color::text();
        theme.border = color::border();
        theme.input = color::border();
        theme.muted = color::surface_2();
        theme.muted_foreground = color::muted();
        theme.popover = color::surface();
        theme.title_bar = color::bg();
        theme.title_bar_border = color::border();
        theme.ring = color::accent();
        theme.caret = color::accent();
        theme.selection = color::accent().opacity(0.35);
        theme.primary = color::accent();
        theme.primary_hover = color::accent_hover();
        theme.primary_active = color::accent_active();
        theme.primary_foreground = rgb(0xffffff).into();
        theme.button_primary = color::accent();
        theme.button_primary_hover = color::accent_hover();
        theme.button_primary_active = color::accent_active();
        theme.button_primary_foreground = rgb(0xffffff).into();
        theme.secondary = color::surface_2();
        theme.secondary_hover = color::border();
        theme.secondary_active = color::border();
        theme.secondary_foreground = color::text();
        theme.success = color::success();
        theme.warning = color::warning();
        theme.danger = color::danger();
        theme.switch = color::border();
        theme.progress_bar = color::accent();
        theme.scrollbar_thumb = color::border();
        theme.scrollbar_thumb_hover = color::faint();
    });
}

/// The monospace font family for tabular/log text.
pub fn mono(cx: &App) -> SharedString {
    cx.theme().mono_font_family.clone()
}

/// A rounded card surface.
pub fn card() -> Div {
    v_flex()
        .bg(color::surface())
        .border_1()
        .border_color(color::border())
        .rounded(px(12.))
        .p_4()
        .gap_3()
}

/// A card section heading: an icon badge, a title, and optional right-side
/// content.
pub fn card_header(icon: impl Into<Icon>, title: impl Into<SharedString>) -> Div {
    h_flex()
        .gap_2p5()
        .items_center()
        .child(icon_badge(icon, color::accent()))
        .child(div().text_base().font_semibold().child(title.into()))
}

/// A small tinted square holding an icon.
pub fn icon_badge(icon: impl Into<Icon>, tint: Hsla) -> Div {
    div()
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .size(px(28.))
        .rounded(px(7.))
        .bg(tint.opacity(0.14))
        .text_color(tint)
        .child(icon.into().small())
}

/// A colored dot status marker.
pub fn dot(tint: Hsla) -> Div {
    div().flex_shrink_0().size(px(7.)).rounded_full().bg(tint)
}

/// A rounded status pill: colored dot + label on a tinted background.
pub fn pill(label: impl Into<SharedString>, tint: Hsla) -> Div {
    h_flex()
        .flex_shrink_0()
        .gap_1p5()
        .items_center()
        .px_2()
        .py(px(2.))
        .rounded_full()
        .bg(tint.opacity(0.13))
        .text_color(tint)
        .text_xs()
        .font_medium()
        .child(dot(tint))
        .child(label.into())
}

/// A neutral rounded tag (e.g. a version label).
pub fn tag(label: impl Into<SharedString>) -> Div {
    div()
        .flex_shrink_0()
        .px_2()
        .py(px(1.))
        .rounded(px(6.))
        .bg(color::surface_2())
        .border_1()
        .border_color(color::border())
        .text_color(color::muted())
        .text_xs()
        .child(label.into())
}

/// A dashboard stat tile.
pub fn stat_tile(
    icon: impl Into<Icon>,
    tint: Hsla,
    label: impl Into<SharedString>,
    value: impl IntoElement,
    detail: impl Into<SharedString>,
) -> Div {
    card()
        .flex_1()
        .min_w(px(160.))
        .gap_2()
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(icon_badge(icon, tint))
                .child(
                    div()
                        .text_xs()
                        .text_color(color::muted())
                        .font_medium()
                        .child(label.into()),
                ),
        )
        .child(div().text_xl().font_semibold().child(value))
        .child(div().text_xs().text_color(color::faint()).truncate().child(detail.into()))
}

/// Muted helper text.
pub fn help(text: impl Into<SharedString>) -> Div {
    div().text_xs().text_color(color::muted()).child(text.into())
}

/// A full-window modal: a dimmed scrim (which swallows clicks to the page
/// behind) with `content` centered on a card.
pub fn modal(id: &'static str, width: f32, content: impl IntoElement) -> Stateful<Div> {
    div()
        .id(id)
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .p_6()
        .bg(gpui_kit::black().opacity(0.62))
        .occlude()
        .child(
            v_flex()
                .w(px(width))
                .max_w_full()
                .max_h_full()
                .bg(color::surface())
                .border_1()
                .border_color(color::border())
                .rounded(px(14.))
                .shadow_lg()
                .p_6()
                .gap_4()
                .child(content),
        )
}

/// A modal title row with an icon badge.
pub fn modal_title(icon: impl Into<Icon>, tint: Hsla, title: impl Into<SharedString>) -> Div {
    h_flex()
        .gap_3()
        .items_center()
        .child(icon_badge(icon, tint))
        .child(div().text_lg().font_semibold().child(title.into()))
}

/// Right-aligned modal button row.
pub fn modal_actions(children: impl IntoIterator<Item = AnyElement>) -> Div {
    h_flex().gap_2().justify_end().pt_1().children(children)
}
