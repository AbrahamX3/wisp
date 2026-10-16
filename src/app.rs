//! The Wisp window.
//!
//! Phase 1 ships the shell: a header with the single settings menu, the update
//! banner, toasts and the About dialog. The project list, the services page and
//! the log viewer land on top of this frame in later phases.

use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::link::Link;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{
    ActiveTheme as _, Icon, Side, Sizable as _, Theme, ThemeMode, WindowExt as _,
};
use gpui_kit::*;

use crate::assets::APP_ICON;
use crate::autostart;
use crate::platform;
use crate::settings::{Settings, ThemePreference};
use crate::tray;
use crate::update;

/// Delay before the automatic launch update check, so start-up stays snappy.
const AUTO_CHECK_DELAY: Duration = Duration::from_secs(6);

/// Repository URL, taken from `Cargo.toml` so it cannot drift from the updater's
/// own `REPO_OWNER` / `REPO_NAME` (a test below keeps the two in step).
const REPOSITORY_URL: &str = env!("CARGO_PKG_REPOSITORY");

#[derive(Clone, Copy)]
enum ToastKind {
    Success,
    /// Reserved for advisory notices the later phases raise.
    #[allow(dead_code)]
    Warning,
    Error,
}

/// Progress of an in-app update check / install.
#[derive(Debug, Default)]
enum UpdateState {
    #[default]
    Idle,
    Checking,
    Available(String),
    Installing(String),
    ReadyToRestart(String),
}

pub struct Wisp {
    theme: ThemePreference,
    settings: Settings,
    toasts: Vec<(ToastKind, String)>,
    update: UpdateState,
    /// Whether the per-user Run entry points at this executable. Read from the
    /// registry at start-up and after each toggle, never persisted here.
    autostart: bool,
}

impl Wisp {
    pub fn new(settings: Settings, cx: &mut Context<Self>) -> Self {
        let this = Self {
            theme: settings.theme,
            settings,
            toasts: Vec::new(),
            update: UpdateState::Idle,
            autostart: autostart::is_enabled(),
        };

        Self::spawn_auto_update_check(cx);
        this
    }

    fn push_toast(&mut self, kind: ToastKind, message: String) {
        self.toasts.push((kind, message));
    }

    // ---- settings toggles ------------------------------------------------

    /// Turn the tray icon, and with it "closing hides the window", on or off.
    fn toggle_background(&mut self, cx: &mut Context<Self>) {
        let wanted = !self.settings.background;

        if wanted {
            if let Err(error) = tray::install(cx) {
                self.push_toast(
                    ToastKind::Error,
                    format!("Could not create the tray icon: {error}"),
                );
                cx.notify();
                return;
            }
        } else {
            tray::remove(cx);
        }

        self.settings.background = wanted;
        self.settings.save();
        self.push_toast(
            ToastKind::Success,
            if wanted {
                "Run in background is on \u{2014} closing the window keeps Wisp in the tray."
            } else {
                "Run in background is off \u{2014} closing the window exits."
            }
            .to_string(),
        );
        cx.notify();
    }

    /// Switch the per-user `Run` entry on or off. No elevation: it is HKCU.
    fn toggle_autostart(&mut self, cx: &mut Context<Self>) {
        let wanted = !self.autostart;
        match autostart::set(wanted) {
            Ok(()) => {
                self.autostart = wanted;
                self.push_toast(
                    ToastKind::Success,
                    if wanted {
                        "Start with Windows is on \u{2014} Wisp opens when you sign in."
                    } else {
                        "Start with Windows is off."
                    }
                    .to_string(),
                );
            }
            Err(error) => self.push_toast(
                ToastKind::Error,
                format!("Could not change the Windows start-up entry: {error}"),
            ),
        }
        cx.notify();
    }

    fn set_theme(&mut self, pref: ThemePreference, window: &mut Window, cx: &mut Context<Self>) {
        if self.theme == pref {
            return;
        }
        self.theme = pref;
        self.settings.theme = pref;
        self.settings.save();
        apply_theme(pref, Some(window), cx);
        cx.notify();
    }

    // ---- updates ---------------------------------------------------------

    /// Ask GitHub Releases whether a newer version exists.
    ///
    /// `manual` is the Menu action: it always reports a result (including "up to
    /// date" and errors). The automatic launch check is cached, stays silent on
    /// failure, and respects a dismissed version.
    fn check_updates(&mut self, cx: &mut Context<Self>, manual: bool) {
        if matches!(
            self.update,
            UpdateState::Checking | UpdateState::Installing(_)
        ) {
            return;
        }
        self.update = UpdateState::Checking;
        cx.notify();

        let skipped = self.settings.skipped_version.clone();

        cx.spawn(async move |entity, cx| {
            let result = cx
                .background_executor()
                .spawn(async { update::check() })
                .await;

            entity
                .update(cx, |this, cx| {
                    this.settings.record_update_check();

                    match result {
                        Ok(Some(version)) => {
                            this.settings.available_version = Some(version.clone());
                            this.update = if should_offer_update(manual, &version, skipped.as_deref())
                            {
                                UpdateState::Available(version)
                            } else {
                                UpdateState::Idle
                            };
                        }
                        Ok(None) => {
                            this.settings.available_version = None;
                            this.update = UpdateState::Idle;
                            if manual {
                                this.push_toast(
                                    ToastKind::Success,
                                    format!(
                                        "Wisp v{} is the latest version.",
                                        env!("CARGO_PKG_VERSION")
                                    ),
                                );
                            }
                        }
                        Err(error) => {
                            this.update = UpdateState::Idle;
                            if manual {
                                this.push_toast(ToastKind::Error, error.to_string());
                            }
                        }
                    }

                    this.settings.save();
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    /// Check once shortly after launch, at most once a day.
    fn spawn_auto_update_check(cx: &mut Context<Self>) {
        cx.spawn(async move |entity, cx| {
            cx.background_executor().timer(AUTO_CHECK_DELAY).await;

            entity
                .update(cx, |this, cx| {
                    if this.settings.update_check_due() {
                        this.check_updates(cx, false);
                        return;
                    }

                    // Inside the throttle window: no network call, but still
                    // show an update a previous check already found.
                    let Some(cached) = this.settings.available_version.clone() else {
                        return;
                    };
                    let skipped = this.settings.skipped_version.clone();
                    if is_newer(&cached, env!("CARGO_PKG_VERSION"))
                        && should_offer_update(false, &cached, skipped.as_deref())
                    {
                        this.update = UpdateState::Available(cached);
                        cx.notify();
                    }
                })
                .ok();
        })
        .detach();
    }

    /// Remember the dismissed version so the banner does not come back for it.
    fn dismiss_update(&mut self, version: String, cx: &mut Context<Self>) {
        self.settings.skipped_version = Some(version);
        self.settings.save();
        self.update = UpdateState::Idle;
        cx.notify();
    }

    /// Download and install the available update (off the UI thread).
    fn install_update(&mut self, cx: &mut Context<Self>) {
        let UpdateState::Available(version) = &self.update else {
            return;
        };
        self.update = UpdateState::Installing(version.clone());
        cx.notify();

        cx.spawn(async move |entity, cx| {
            let result = cx
                .background_executor()
                .spawn(async { update::install() })
                .await;

            entity
                .update(cx, |this, cx| {
                    match result {
                        Ok(Some(version)) => {
                            this.push_toast(
                                ToastKind::Success,
                                format!("Updated to v{version}. Restart to finish."),
                            );
                            this.update = UpdateState::ReadyToRestart(version);
                        }
                        Ok(None) => {
                            this.update = UpdateState::Idle;
                            this.push_toast(ToastKind::Success, "Already up to date.".to_string());
                        }
                        Err(error) => {
                            this.update = UpdateState::Idle;
                            this.push_toast(ToastKind::Error, error.to_string());
                        }
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    /// The update banner, if there is anything to say about updates.
    fn update_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let row = match &self.update {
            UpdateState::Idle | UpdateState::Checking => return None,
            UpdateState::Available(version) => {
                let text = format!("Wisp v{version} is available.");
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .child(icon(IconName::Download))
                    .child(div().flex_grow(1.0).text_sm().child(text))
                    .child(
                        Button::new("update-install")
                            .small()
                            .primary()
                            .label("Install and restart")
                            .on_click(cx.listener(|this, _, _window, cx| this.install_update(cx))),
                    )
                    .child(
                        Button::new("update-dismiss")
                            .ghost()
                            .small()
                            .icon(icon(IconName::X))
                            .tooltip("Skip this version")
                            .on_click({
                                let version = version.clone();
                                cx.listener(move |this, _, _window, cx| {
                                    this.dismiss_update(version.clone(), cx)
                                })
                            }),
                    )
            }
            UpdateState::Installing(version) => {
                let text = format!("Installing v{version}\u{2026}");
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .child(Spinner::new().small())
                    .child(div().flex_grow(1.0).text_sm().child(text))
            }
            UpdateState::ReadyToRestart(version) => {
                let text = format!("Updated to v{version}. Restart to finish.");
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .child(icon(IconName::Download))
                    .child(div().flex_grow(1.0).text_sm().child(text))
                    .child(
                        Button::new("update-restart")
                            .small()
                            .primary()
                            .label("Restart")
                            .on_click(cx.listener(|_this, _, _window, cx| cx.restart())),
                    )
            }
        };

        Some(
            div()
                .px(px(28.))
                .pt(px(20.))
                .child(
                    row.px(px(18.))
                        .py(px(14.))
                        .rounded_lg()
                        .bg(cx.theme().secondary)
                        .border_1()
                        .border_color(cx.theme().border),
                )
                .into_any_element(),
        )
    }

    // ---- chrome ----------------------------------------------------------

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap_3()
            .px(px(28.))
            .py(px(18.))
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .size(px(28.))
                            .rounded_md()
                            .child(img(APP_ICON).size(px(28.))),
                    )
                    .child(
                        div()
                            .text_xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Wisp"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_shrink(0.0)
                    .items_center()
                    .gap_3()
                    .child(self.settings_menu(cx)),
            )
    }

    fn settings_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let theme = self.theme;
        let autostart = self.autostart;
        let background = self.settings.background;

        Button::new("settings-menu")
            .icon(icon(IconName::Ellipsis))
            .tooltip("Menu")
            .ghost()
            .small()
            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, window, cx| {
                let menu = menu
                    .check_side(Side::Right)
                    .item(
                        PopupMenuItem::new("Start with Windows")
                            .icon(icon(IconName::Rocket))
                            .checked(autostart)
                            .on_click({
                                let entity = entity.clone();
                                move |_, _, cx| {
                                    entity.update(cx, Wisp::toggle_autostart);
                                }
                            }),
                    )
                    .item(
                        PopupMenuItem::new("Run in background")
                            .icon(icon(IconName::Monitor))
                            .checked(background)
                            .on_click({
                                let entity = entity.clone();
                                move |_, _, cx| {
                                    entity.update(cx, Wisp::toggle_background);
                                }
                            }),
                    )
                    .separator();

                let menu = menu
                    .submenu_with_icon(
                        Some(icon(IconName::Palette)),
                        "Theme",
                        window,
                        cx,
                        {
                            let entity = entity.clone();
                            move |menu, _, _| {
                                let menu = menu.check_side(Side::Right);
                                menu.item(theme_item(
                                    "System",
                                    ThemePreference::System,
                                    theme,
                                    IconName::Monitor,
                                    &entity,
                                ))
                                .item(theme_item(
                                    "Light",
                                    ThemePreference::Light,
                                    theme,
                                    IconName::Sun,
                                    &entity,
                                ))
                                .item(theme_item(
                                    "Dark",
                                    ThemePreference::Dark,
                                    theme,
                                    IconName::Moon,
                                    &entity,
                                ))
                            }
                        },
                    )
                    .separator();

                menu.item(
                    PopupMenuItem::new("Check for updates")
                        .icon(icon(IconName::Download))
                        .on_click({
                            let entity = entity.clone();
                            move |_, _, cx| {
                                entity.update(cx, |this, cx| this.check_updates(cx, true));
                            }
                        }),
                )
                .item(
                    PopupMenuItem::new("About")
                        .icon(icon(IconName::Info))
                        .on_click(|_, window, cx| open_about(window, cx)),
                )
            })
    }

    /// The empty state until the project list lands in a later phase.
    fn body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .child(div().size(px(72.)).child(img(APP_ICON).size(px(72.))))
            .child(
                div()
                    .text_base()
                    .font_weight(FontWeight::MEDIUM)
                    .child("No projects yet"),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("Projects and the PHP/nginx services arrive in the next build."),
            )
    }
}

impl Render for Wisp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        flush_toasts(&mut self.toasts, window, cx);

        let mut root = div()
            .flex()
            .flex_col()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.header(cx));

        if let Some(banner) = self.update_banner(cx) {
            root = root.child(banner);
        }

        root.child(div().flex_1().overflow_hidden().child(self.body(cx)))
    }
}

fn theme_item(
    label: &str,
    pref: ThemePreference,
    current: ThemePreference,
    icon_name: IconName,
    entity: &Entity<Wisp>,
) -> PopupMenuItem {
    let entity = entity.clone();
    PopupMenuItem::new(label)
        .icon(icon(icon_name))
        .checked(current == pref)
        .on_click(move |_, window, cx| {
            entity.update(cx, |this, cx| this.set_theme(pref, window, cx));
        })
}

fn open_about(window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, |dialog, _window, cx| {
        dialog
            .title("About Wisp")
            .close_button(true)
            .w(px(440.))
            .child(about_content(cx))
    });
}

fn about_content(cx: &App) -> impl IntoElement {
    let muted = cx.theme().muted_foreground;

    div()
        .flex()
        .flex_col()
        .w_full()
        .gap_4()
        .py_2()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .w_full()
                .gap_4()
                .child(icon_tile(cx))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_lg()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("Wisp"),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(muted)
                                .child(format!("Version {}", env!("CARGO_PKG_VERSION"))),
                        ),
                ),
        )
        .child(
            div()
                .w_full()
                .text_sm()
                .text_color(muted)
                .child(env!("CARGO_PKG_DESCRIPTION")),
        )
        .child(div().h(px(1.)).w_full().bg(cx.theme().border))
        .child(
            div()
                .flex()
                .flex_col()
                .w_full()
                .gap_1()
                .child(div().text_xs().text_color(muted).child("SOURCE"))
                .child(
                    Link::new("about-repository")
                        .href(REPOSITORY_URL)
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .text_sm()
                        .child(icon(IconName::Github).small())
                        .child(repository_label()),
                ),
        )
        .child(
            div()
                .w_full()
                .text_xs()
                .text_color(muted)
                .child(format!(
                    "Built with Rust and GPUI Kit \u{00b7} {} licensed",
                    env!("CARGO_PKG_LICENSE")
                )),
        )
}

/// The repository URL without its scheme, as shown in the About dialog.
fn repository_label() -> &'static str {
    REPOSITORY_URL
        .strip_prefix("https://")
        .unwrap_or(REPOSITORY_URL)
}

/// The brand icon inside a nested tray, matching the card treatment.
fn icon_tile(cx: &App) -> impl IntoElement {
    div()
        .p(px(5.))
        .rounded_xl()
        .bg(cx.theme().muted)
        .border_1()
        .border_color(cx.theme().border)
        .child(
            div()
                .size(px(64.))
                .rounded_lg()
                .bg(cx.theme().secondary)
                .child(img(APP_ICON).size(px(64.))),
        )
}

fn icon(name: IconName) -> Icon {
    Icon::new(name)
}

fn flush_toasts(
    toasts: &mut Vec<(ToastKind, String)>,
    window: &mut Window,
    cx: &mut Context<Wisp>,
) {
    for (kind, message) in std::mem::take(toasts) {
        let notification = match kind {
            ToastKind::Success => Notification::success(message),
            ToastKind::Warning => Notification::warning(message),
            ToastKind::Error => Notification::error(message).autohide(false),
        };
        window.push_notification(notification, cx);
    }
}

/// Whether `candidate` is a later release than `current`.
///
/// Compares the dotted numeric parts. Anything unparsable - a prerelease tag,
/// say - counts as not-later, so a surprising tag can never raise a prompt.
fn is_newer(candidate: &str, current: &str) -> bool {
    fn parts(version: &str) -> Option<Vec<u64>> {
        version
            .trim_start_matches('v')
            .split('.')
            .map(|part| part.parse::<u64>().ok())
            .collect()
    }

    match (parts(candidate), parts(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => false,
    }
}

/// Whether an available version should be surfaced to the user.
///
/// A manual check always reports what it found, including a version the user
/// dismissed earlier; the automatic launch check stays quiet about it.
fn should_offer_update(manual: bool, available: &str, skipped: Option<&str>) -> bool {
    manual || skipped != Some(available)
}

/// Closing the window hides it to the tray while "Run in background" is on, and
/// exits the app otherwise.
pub fn window_should_close(_window: &mut Window, cx: &mut App) -> bool {
    if tray::is_enabled(cx) {
        platform::show_window(platform::WINDOW_TITLE, false);
        return false;
    }
    true
}

/// Apply a three-way theme preference to the global theme.
pub fn apply_theme(pref: ThemePreference, window: Option<&mut Window>, cx: &mut App) {
    match pref {
        ThemePreference::Light => Theme::change(ThemeMode::Light, window, cx),
        ThemePreference::Dark => Theme::change(ThemeMode::Dark, window, cx),
        ThemePreference::System => Theme::sync_system_appearance(window, cx),
    }
}

#[cfg(test)]
mod tests {
    use super::{is_newer, should_offer_update, REPOSITORY_URL};
    use crate::update::{REPO_NAME, REPO_OWNER};

    #[test]
    fn a_cached_version_newer_than_ours_is_offered() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("v0.1.1", "0.1.0"));
    }

    #[test]
    fn a_cached_version_we_already_run_is_not_offered() {
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
    }

    #[test]
    fn an_unparsable_cached_version_is_not_offered() {
        assert!(!is_newer("0.2.0-rc1", "0.1.0"));
        assert!(!is_newer("nightly", "0.1.0"));
    }

    #[test]
    fn about_dialog_repository_url_matches_the_updater() {
        assert_eq!(
            REPOSITORY_URL,
            format!("https://github.com/{REPO_OWNER}/{REPO_NAME}"),
            "Cargo.toml's repository and the updater's release target must agree"
        );
    }

    #[test]
    fn manual_check_offers_any_available_version() {
        assert!(should_offer_update(true, "0.2.0", None));
    }

    #[test]
    fn automatic_check_offers_a_newer_version() {
        assert!(should_offer_update(false, "0.2.0", Some("0.1.9")));
    }

    #[test]
    fn automatic_check_stays_quiet_for_a_skipped_version() {
        assert!(!should_offer_update(false, "0.2.0", Some("0.2.0")));
    }

    #[test]
    fn manual_check_still_offers_a_skipped_version() {
        assert!(should_offer_update(true, "0.2.0", Some("0.2.0")));
    }
}
