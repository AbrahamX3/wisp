//! Shared visual primitives.
//!
//! The app's core motif is the "tray and plate": an outer hairline shell holding
//! a raised inner surface, borrowed from the icon's glass-plate framing. Every
//! card, field group and list row is built from it so depth stays consistent.

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::*;

use crate::brand;

/// An outer tray wrapping a raised inner plate.
pub fn plate(cx: &App, inner: impl IntoElement) -> impl IntoElement {
    div()
        .p(px(5.))
        .rounded_xl()
        .bg(cx.theme().muted)
        .border_1()
        .border_color(cx.theme().border)
        .child(
            div()
                .rounded_lg()
                .bg(cx.theme().secondary)
                .border_1()
                .border_color(cx.theme().border)
                .child(inner),
        )
}

/// A small uppercase label that introduces a group of content.
pub fn eyebrow(label: &str, cx: &App) -> impl IntoElement {
    div()
        .px(px(8.))
        .py(px(3.))
        .rounded_full()
        .bg(cx.theme().muted)
        .border_1()
        .border_color(cx.theme().border)
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .text_color(cx.theme().muted_foreground)
        .child(uppercase(label))
}

/// The brand accent as a small filled dot, for status and selection.
pub fn dot(color: Hsla) -> impl IntoElement {
    div().size(px(8.)).rounded_full().bg(color)
}

/// A 2px vertical rail in the brand accent, marking the active sidebar item.
pub fn accent_rail() -> impl IntoElement {
    div()
        .w(px(3.))
        .h_full()
        .rounded_full()
        .bg(brand::gradient())
}

fn uppercase(label: &str) -> String {
    label.to_uppercase()
}
