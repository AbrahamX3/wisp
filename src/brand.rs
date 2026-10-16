//! The visual vocabulary taken from the app icon: a dark glass plate with a
//! single teal-to-violet accent.
//!
//! The accent is deliberately restrained. It appears on the primary action, the
//! focus ring, the active sidebar rail and running status, and nowhere else —
//! everything else is neutral theme tokens.

use gpui_kit::component::Theme;
use gpui_kit::{linear_color_stop, linear_gradient, rgb, rgba, App, Background, Hsla};

/// The three stops of the icon's gradient.
pub const TEAL: u32 = 0x2DD4BF;
pub const VIOLET: u32 = 0x8B5CF6;

/// The primary accent, as a themed colour.
pub fn accent() -> Hsla {
    rgb(TEAL).into()
}

/// The accent at 20% for tints and rails.
pub fn accent_tint() -> Hsla {
    rgba(0x2DD4BF33).into()
}

/// The accent at 12% for the softest washes.
pub fn accent_wash() -> Hsla {
    rgba(0x2DD4BF1F).into()
}

/// The icon's teal→violet sweep, used for a single hero element.
pub fn gradient() -> Background {
    linear_gradient(
        135.0,
        linear_color_stop(rgb(TEAL), 0.0),
        linear_color_stop(rgb(VIOLET), 1.0),
    )
}

/// Reapply the brand on top of the active theme.
///
/// Called after every theme change, because switching light/dark replaces the
/// colour tokens; the font family survives, but the accent must be reasserted.
pub fn apply(cx: &mut App) {
    let family = crate::fonts::family();
    Theme::update(cx, move |theme| {
        if let Some(family) = family.clone() {
            theme.font_family = family;
        }
        theme.colors.primary = rgb(TEAL).into();
        theme.colors.ring = rgb(TEAL).into();
    });
}
