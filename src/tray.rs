//! The system tray icon, present while "Run in background" is on.
//!
//! `gpui-tray` is built for GPUI: it does not run a second event loop, posting
//! tray events onto GPUI's own foreground executor instead, and its menu entries
//! are ordinary `MenuItem`s that dispatch ordinary GPUI actions. That is why it
//! coexists with the window rather than competing with it for the message pump.
//!
//! The menu is rebuilt on demand from [`TrayMenuData`], which the window keeps
//! current, so the site submenus always reflect what is actually running.

use gpui_kit::*;
use gpui_tray::{Icon, Tray};

/// The app icon as non-premultiplied 32x32 RGBA, rasterised from `icon.svg` by
/// `build.rs`.
const ICON: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/tray-32.rgba"));
const ICON_SIZE: u32 = 32;

actions!(
    wisp_tray,
    [OpenWindow, QuitApp, StartAll, StopAll, RestartAll]
);

/// What a site's submenu can ask for.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Deserialize, schemars::JsonSchema)]
pub enum SiteCommand {
    Start,
    Stop,
    Restart,
    /// Open the site's address in the default browser.
    Open,
    /// Show the project's own files in File Explorer.
    OpenRoot,
    /// Show the folder holding this site's logs and generated config.
    OpenFiles,
}

/// A command from a site's tray submenu, carrying which site it refers to.
#[derive(Clone, PartialEq, Eq, Debug, serde::Deserialize, schemars::JsonSchema, Action)]
#[action(namespace = wisp_tray)]
pub struct SiteAction {
    pub id: i64,
    pub command: SiteCommand,
}

/// What "all sites" can ask for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AllCommand {
    Start,
    Stop,
    Restart,
}

/// A tray request handed to the window, which owns the single source of truth.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrayRequest {
    /// Act on one project.
    Site { id: i64, command: SiteCommand },
    /// Act on every project.
    All(AllCommand),
}

/// One project as the tray menu should show it.
#[derive(Clone, PartialEq, Eq)]
pub struct TraySite {
    pub id: i64,
    pub name: String,
    pub url: String,
    pub running: bool,
    pub partial: bool,
}

/// The data the tray menu is rebuilt from. The window overwrites it whenever the
/// set of projects or their running state changes, then calls [`refresh`].
#[derive(Default)]
pub struct TrayMenuData {
    pub sites: Vec<TraySite>,
}

impl Global for TrayMenuData {}

/// The window's handler for tray requests: a boxed callback so the tray module
/// never needs to know about the window type.
pub type TrayHandler = Box<dyn Fn(TrayRequest, &mut App) + 'static>;

/// The window's handler for tray requests. Installed by `Wisp::new`, because only
/// it can reach the running state that start/stop/restart act on.
#[derive(Default)]
pub struct TrayHooks(pub Option<TrayHandler>);

impl Global for TrayHooks {}

/// Route a tray request to the window, if a window is listening.
pub fn dispatch(request: TrayRequest, cx: &mut App) {
    if cx.try_global::<TrayHooks>().is_none() {
        return;
    }
    // Take the handler out first: calling it needs `&mut App`, which would
    // otherwise clash with the borrow of the global holding it.
    let Some(handler) = cx.global_mut::<TrayHooks>().0.take() else {
        return;
    };
    (*handler)(request, cx);
    cx.global_mut::<TrayHooks>().0 = Some(handler);
}

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
    cx.try_global::<TrayState>()
        .is_some_and(|state| state.enabled)
}

/// Rebuild the tray menu from the current [`TrayMenuData`]. A no-op when the
/// tray is not installed.
pub fn refresh(cx: &mut App) {
    let tray = cx.global_mut::<TrayState>().tray.take();
    if let Some(tray) = tray.as_ref() {
        let _ = tray.refresh_menu(cx);
    }
    cx.global_mut::<TrayState>().tray = tray;
}

/// The mark shown before a site's name in the menu.
fn site_mark(site: &TraySite) -> &'static str {
    if site.running {
        "\u{25CF}" // solid: both services up
    } else if site.partial {
        "\u{25D0}" // half: one of the two
    } else {
        "\u{25CB}" // hollow: stopped
    }
}

fn site_submenu(site: &TraySite) -> MenuItem {
    let id = site.id;
    let running = site.running;
    let partial = site.partial;
    let any_up = running || partial;

    let mut items = vec![
        MenuItem::action(
            "Start",
            SiteAction {
                id,
                command: SiteCommand::Start,
            },
        )
        .disabled(any_up),
        MenuItem::action(
            "Stop",
            SiteAction {
                id,
                command: SiteCommand::Stop,
            },
        )
        .disabled(!any_up),
        MenuItem::action(
            "Restart",
            SiteAction {
                id,
                command: SiteCommand::Restart,
            },
        )
        .disabled(!any_up),
        MenuItem::separator(),
        MenuItem::action(
            "Open in Explorer",
            SiteAction {
                id,
                command: SiteCommand::OpenRoot,
            },
        ),
        MenuItem::action(
            "Open config folder",
            SiteAction {
                id,
                command: SiteCommand::OpenFiles,
            },
        ),
    ];

    // A short hint of the address, so the submenu is identifiable at a glance.
    if !site.url.is_empty() {
        items.push(MenuItem::separator());
        items.push(MenuItem::action(
            site.url.clone(),
            SiteAction {
                id,
                command: SiteCommand::Open,
            },
        ));
    }

    MenuItem::submenu(Menu {
        name: format!("{} {}", site_mark(site), site.name).into(),
        items,
        disabled: false,
    })
}

/// Build the whole tray menu from the published data.
fn build_menu(cx: &mut App) -> Vec<MenuItem> {
    let sites: Vec<TraySite> = cx
        .try_global::<TrayMenuData>()
        .map(|data| data.sites.clone())
        .unwrap_or_default();

    let mut items = vec![
        MenuItem::action("Open Wisp", OpenWindow),
        MenuItem::separator(),
        MenuItem::action("Start all sites", StartAll),
        MenuItem::action("Stop all sites", StopAll),
        MenuItem::action("Restart all sites", RestartAll),
    ];

    if !sites.is_empty() {
        items.push(MenuItem::separator());
        items.extend(sites.iter().map(site_submenu));
    }

    items.push(MenuItem::separator());
    items.push(MenuItem::action("Quit", QuitApp));
    items
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
            .menu(build_menu)
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
