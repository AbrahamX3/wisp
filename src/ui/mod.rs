//! Shared visual primitives.
//!
//! Deliberately thin: the design guides ask for one background, one hairline
//! and one spacing step rather than cards nested inside cards, and for
//! published components wherever one exists. What is left here is the small
//! amount of composition the product actually owns.

pub mod theme;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::*;

/// A raised section: a surface, a hairline boundary, and the caller's padding.
///
/// One level only — no outer tray wrapping an inner plate.
pub fn section(cx: &App, inner: impl IntoElement) -> impl IntoElement {
    div()
        .w_full()
        .rounded_lg()
        .bg(cx.theme().secondary)
        .border_1()
        .border_color(cx.theme().border)
        .child(inner)
}

/// The brand accent as a small filled dot, for status and selection.
pub fn dot(color: Hsla) -> impl IntoElement {
    div().size(px(8.)).rounded_full().bg(color)
}

