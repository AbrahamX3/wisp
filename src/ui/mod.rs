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
    section_base(cx).child(inner)
}

/// A section that grows to fill the space left in its parent.
///
/// For a card whose contents scroll inside themselves rather than letting the
/// page scroll: the parent column hands it whatever is left after the header and
/// the tabs, and `min_h(0)` lets it shrink instead of overflowing.
pub fn section_grow(cx: &App, inner: impl IntoElement) -> impl IntoElement {
    section_base(cx).flex_1().min_h(px(0.)).child(inner)
}

fn section_base(cx: &App) -> Div {
    div()
        .w_full()
        .rounded_lg()
        .bg(cx.theme().secondary)
        .border_1()
        .border_color(cx.theme().border)
}

/// The brand accent as a small filled dot, for status and selection.
pub fn dot(color: Hsla) -> impl IntoElement {
    div().size(px(8.)).rounded_full().bg(color)
}

