//! The system tray icon, present while "Run in background" is on.
//!
//! `gpui-tray` is built for GPUI: it does not run a second event loop, posting
//! tray events onto GPUI's own foreground executor instead, and its menu entries
//! are ordinary `MenuItem`s that dispatch ordinary GPUI actions. That is why it
//! coexists with the window rather than competing with it for the message pump.

use gpui_kit::*;
use gpui_tray::{Icon, Tray};

/// The app icon as non-premultiplied 32x32 RGBA, rasterised from `icon.svg` by
/// `build.rs`.
const ICON: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/tray-32.rgba"));
const ICON_SIZE: u32 = 32;

actions!(wisp_tray, [OpenWindow, QuitApp]);

/// Whether the tray exists and what it is, held process-wide so the menu can
/// add or remove it while the app runs.
#[derive(Default)]
pub struct TrayState {
    enabled: bool,
    tray: Option<Tray>,
}

impl Global for TrayState {}

/// True while closing the window should hide to the tray instead of exiting.
pub fn is_enabled(cx: &App) -> bool {
    cx.try_global::<TrayState>().is_some_and(|state| state.enabled)
}

/// Create the tray icon and mark background mode on.
pub fn install(cx: &mut App) -> gpui_tray::Result<()> {
    let already = cx.global::<TrayState>().tray.is_some();
    if !already {
        let tray = Tray::builder()
            .icon(Icon::from_rgba(ICON, ICON_SIZE, ICON_SIZE)?)
            .title("Wisp")
            .tooltip("Wisp")
            .on_activate(OpenWindow)
            .menu(|_cx| {
                vec![
                    MenuItem::action("Open Wisp", OpenWindow),
                    MenuItem::separator(),
                    MenuItem::action("Quit", QuitApp),
                ]
            })
            .build(cx)?;

        let state = cx.global_mut::<TrayState>();
        state.tray = Some(tray);
    }

    cx.global_mut::<TrayState>().enabled = true;
    Ok(())
}

/// Remove the tray icon and mark background mode off. Safe to call twice.
pub fn remove(cx: &mut App) {
    if let Some(tray) = cx.global_mut::<TrayState>().tray.take() {
        let _ = tray.close(cx);
    }
    cx.global_mut::<TrayState>().enabled = false;
}
