//! Application assets.
//!
//! Serves the brand icon and the extra Lucide icons this app uses, falling back
//! to GPUI Kit's default component icons.

use std::borrow::Cow;

use gpui_kit::assets::icon_assets;
use gpui_kit::{AssetSource, SharedString};

/// Asset key for the app's own icon, as passed to `img(...)`.
pub const APP_ICON: &str = "icons/icon.svg";

const APP_ICON_BYTES: &[u8] = include_bytes!("../assets/icon.svg");

// Icons beyond GPUI Kit's default set, embedded by name from the shared
// catalog. Adding one here is all that is needed to reference it.
icon_assets!(
    ExtraIcons,
    [
        Play,
        Square,
        RotateCcw,
        RefreshCw,
        Ellipsis,
        Info,
        Power,
        ShieldCheck,
        Palette,
        Monitor,
        Sun,
        Moon,
        Download,
        Github,
        Rocket,
        X,
    ]
);

pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        if path == APP_ICON {
            return Ok(Some(Cow::Borrowed(APP_ICON_BYTES)));
        }
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        if APP_ICON.starts_with(path) {
            paths.push(APP_ICON.into());
        }
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}
