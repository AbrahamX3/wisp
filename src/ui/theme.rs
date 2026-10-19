//! The brand, expressed as theme tokens.
//!
//! Per the design guides, raw color values live only in the theme layer;
//! application UI resolves color from `cx.theme()` by semantic role. Wisp's
//! teal therefore becomes the theme's `primary` and focus `ring`, and the
//! sidebar's accent, rather than being painted at call sites.

use gpui_kit::component::theme::Theme;
use gpui_kit::{rgb, App};

/// The icon's teal.
#[expect(clippy::unreadable_literal, reason = "hex colour reads as RGB")]
const TEAL: u32 = 0x2DD4BF;

/// Push the brand onto the active theme, and the bundled font.
///
/// Called after every theme change, because switching light/dark replaces the
/// color tokens; the font family survives, but the accent must be reasserted.
pub fn apply(cx: &mut App) {
    let family = crate::fonts::family();
    Theme::update(cx, move |theme| {
        if let Some(family) = family.clone() {
            theme.font_family = family;
        }
        theme.colors.primary = rgb(TEAL).into();
        theme.colors.ring = rgb(TEAL).into();
        theme.colors.sidebar_primary = rgb(TEAL).into();
    });
}
