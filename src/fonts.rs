//! The bundled UI typeface, Plus Jakarta Sans (SIL Open Font License).
//!
//! The static faces are registered with GPUI's text system before the first
//! window, and the family name is read back from the text system rather than
//! assumed, so a rename in an upstream release cannot panic layout.

use std::borrow::Cow;
use std::sync::OnceLock;

use gpui_kit::{App as GpuiApp, SharedString};

const REGULAR: &[u8] = include_bytes!("../assets/fonts/PlusJakartaSans-Regular.ttf");
const MEDIUM: &[u8] = include_bytes!("../assets/fonts/PlusJakartaSans-Medium.ttf");
const SEMIBOLD: &[u8] = include_bytes!("../assets/fonts/PlusJakartaSans-SemiBold.ttf");
const BOLD: &[u8] = include_bytes!("../assets/fonts/PlusJakartaSans-Bold.ttf");

/// The resolved family name, cached for the process once registration succeeds.
static FAMILY: OnceLock<SharedString> = OnceLock::new();

/// Register the four weights and remember the resulting family name.
///
/// Call once, before the first window. A second call is a no-op.
pub fn install(cx: &mut GpuiApp) {
    if FAMILY.get().is_some() {
        return;
    }
    let faces: Vec<Cow<'static, [u8]>> = vec![
        Cow::Borrowed(REGULAR),
        Cow::Borrowed(MEDIUM),
        Cow::Borrowed(SEMIBOLD),
        Cow::Borrowed(BOLD),
    ];
    if cx.text_system().add_fonts(faces).is_err() {
        return;
    }

    if let Some(family) = cx
        .text_system()
        .all_font_names()
        .into_iter()
        .find(|name| name.to_lowercase().contains("plus jakarta"))
    {
        let _ = FAMILY.set(SharedString::from(family));
    }
}

/// The registered family name, or `None` when bundling failed.
pub fn family() -> Option<SharedString> {
    FAMILY.get().cloned()
}
