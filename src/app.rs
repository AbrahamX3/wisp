//! The Wisp window.
//!
//! Layout: a slim sidebar of projects beside a detail pane. Every card is the
//! shared "tray and plate" primitive, the brand accent marks the active project
//! and the primary action, and nothing else carries colour.

use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::form::{Field, Form};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::link::Link;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme as _, Icon, Side, Sizable as _, Theme, ThemeMode, WindowExt as _,
};
use gpui_kit::*;

use crate::assets::APP_ICON;
use crate::autostart;
use crate::brand;
use crate::db::{self, Project, ProjectDraft, Store};
use crate::platform;
use crate::settings::{Settings, ThemePreference};
use crate::tray;
use crate::ui;
use crate::update;

/// Delay before the automatic launch update check, so start-up stays snappy.
const AUTO_CHECK_DELAY: Duration = Duration::from_secs(6);

/// The PHP major/minor lines offered in the editor before runtimes are managed.
const PHP_VERSIONS: &[&str] = &["8.4", "8.3", "8.2", "8.1", "8.0", "7.4"];

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

/// The create/edit dialog's live state. `id` is `None` while creating.
struct EditorState {
    id: Option<i64>,
    name: Entity<InputState>,
    app_name: Entity<InputState>,
    root_path: Entity<InputState>,
    domain: Entity<InputState>,
    php_version: Option<String>,
    ssl_enabled: bool,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

pub struct Wisp {
    theme: ThemePreference,
    settings: Settings,
    toasts: Vec<(ToastKind, String)>,
    update: UpdateState,
    autostart: bool,

    store: Option<Store>,
    projects: Vec<Project>,
    selected: Option<i64>,
    editor: Option<EditorState>,
    /// A persistent problem (e.g. the database could not be opened).
    error: Option<String>,
}

impl Wisp {
    pub fn new(settings: Settings, cx: &mut Context<Self>) -> Self {
        let (store, error) = match Store::open() {
            Ok(store) => (Some(store), None),
            Err(error) => (None, Some(format!("Could not open the project database: {error}"))),
        };

        let mut this = Self {
            theme: settings.theme,
            settings,
            toasts: Vec::new(),
            update: UpdateState::Idle,
            autostart: autostart::is_enabled(),
            store,
            projects: Vec::new(),
            selected: None,
            editor: None,
            error,
        };
        this.reload(cx);

        Self::spawn_auto_update_check(cx);
        this
    }

    // ---- projects --------------------------------------------------------

    fn reload(&mut self, cx: &mut Context<Self>) {
        if let Some(store) = &self.store {
            match store.list_projects() {
                Ok(projects) => {
                    self.projects = projects;
                    self.error = None;
                    if let Some(id) = self.selected {
                        if !self.projects.iter().any(|p| p.id == id) {
                            self.selected = None;
                        }
                    }
                }
                Err(error) => self.error = Some(error.to_string()),
            }
        }
        cx.notify();
    }

    fn selected_project(&self) -> Option<&Project> {
        let id = self.selected?;
        self.projects.iter().find(|p| p.id == id)
    }

    fn select(&mut self, id: i64, cx: &mut Context<Self>) {
        self.selected = Some(id);
        cx.notify();
    }

    fn push_toast(&mut self, kind: ToastKind, message: String) {
        self.toasts.push((kind, message));
    }

    // ---- editor ----------------------------------------------------------

    fn open_editor(
        &mut self,
        existing: Option<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let draft = existing
            .as_ref()
            .map(ProjectDraft::from_project)
            .unwrap_or_else(|| ProjectDraft {
                ssl_enabled: true,
                php_ini_overrides: "{}".to_string(),
                ..Default::default()
            });

        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("My Shop")
                .default_value(draft.name.clone())
        });
        let app_name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("my-shop")
                .default_value(draft.app_name.clone())
        });
        let root_path = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(r"C:\Sites\my-shop")
                .default_value(draft.root_path.clone())
        });
        let domain = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("my-shop.test")
                .default_value(draft.domain.clone())
        });

        // Deriving the domain from the app name while the domain is still empty
        // gives a sensible default without fighting a manual edit.
        let subscription = cx.subscribe_in(
            &app_name,
            window,
            |this, _state, event, window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.sync_domain(window, cx);
                }
            },
        );

        self.editor = Some(EditorState {
            id: existing.as_ref().map(|p| p.id),
            name,
            app_name,
            root_path,
            domain,
            php_version: draft.php_version,
            ssl_enabled: draft.ssl_enabled,
            error: None,
            _subscriptions: vec![subscription],
        });

        let entity = cx.entity();
        window.open_dialog(cx, move |dialog, _window, cx| {
            let Some(editor) = entity.read(cx).editor.as_ref() else {
                return dialog;
            };
            let title = if editor.id.is_some() {
                "Edit project"
            } else {
                "New project"
            };
            let error = editor.error.clone();
            let is_new = editor.id.is_none();

            dialog
                .title(title)
                .w(px(580.))
                .close_button(true)
                .child(editor_form(editor, &entity, error))
                .footer(editor_footer(entity.clone(), is_new))
        });
    }

    /// Fill the domain from the app name while the domain is still untouched.
    fn sync_domain(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.as_ref() else {
            return;
        };
        let domain_is_empty = editor.domain.read(cx).value().trim().is_empty();
        if !domain_is_empty {
            return;
        }
        let slug = db::slugify(&editor.app_name.read(cx).value());
        if slug.is_empty() {
            return;
        }
        editor
            .domain
            .update(cx, |state, cx| state.set_value(format!("{slug}.test"), window, cx));
    }

    fn cancel_editor(&mut self, cx: &mut Context<Self>) {
        self.editor = None;
        cx.notify();
    }

    /// Persist the editor. Returns whether it saved (and the dialog may close).
    fn save_editor(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(editor) = self.editor.as_ref() else {
            return false;
        };
        let draft = ProjectDraft {
            name: editor.name.read(cx).value().to_string(),
            app_name: editor.app_name.read(cx).value().to_string(),
            root_path: editor.root_path.read(cx).value().to_string(),
            domain: editor.domain.read(cx).value().to_string(),
            php_version: editor.php_version.clone(),
            ssl_enabled: editor.ssl_enabled,
            php_ini_overrides: "{}".to_string(),
        };
        let id = editor.id;

        let Some(store) = &self.store else {
            self.set_editor_error("The project database is unavailable.".to_string(), cx);
            return false;
        };

        let result = match id {
            Some(id) => store.update_project(id, &draft).map(|()| id),
            None => store.create_project(&draft),
        };

        match result {
            Ok(saved_id) => {
                self.editor = None;
                self.selected = Some(saved_id);
                self.reload(cx);
                self.push_toast(
                    ToastKind::Success,
                    if id.is_some() {
                        format!("Saved {}.", draft.name.trim())
                    } else {
                        format!("Created {}.", draft.name.trim())
                    },
                );
                cx.notify();
                true
            }
            Err(error) => {
                self.set_editor_error(error.to_string(), cx);
                false
            }
        }
    }

    fn set_editor_error(&mut self, message: String, cx: &mut Context<Self>) {
        if let Some(editor) = self.editor.as_mut() {
            editor.error = Some(message);
        }
        cx.notify();
    }

    fn confirm_delete(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.projects.iter().find(|p| p.id == id).cloned() else {
            return;
        };
        let entity = cx.entity();
        let name = project.name.clone();
        let root = project.root_path.clone();

        window.open_dialog(cx, move |dialog, _window, cx| {
            let body = div()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .text_sm()
                        .child("This removes the project from Wisp only."),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!(
                            "Its files on disk stay untouched \u{2014} {root}"
                        )),
                );

            let cancel = Button::new("delete-cancel")
                .label("Cancel")
                .outline()
                .on_click(|_, window, cx| window.close_dialog(cx));
            let delete = Button::new("delete-confirm")
                .label("Remove project")
                .danger()
                .on_click({
                    let entity = entity.clone();
                    move |_, window, cx| {
                        entity.update(cx, |this, cx| this.delete_project(id, cx));
                        window.close_dialog(cx);
                    }
                });

            dialog
                .title(format!("Remove {name}?"))
                .w(px(460.))
                .close_button(true)
                .child(body)
                .footer(
                    div()
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap_2()
                        .child(cancel)
                        .child(delete),
                )
        });
    }

    fn delete_project(&mut self, id: i64, cx: &mut Context<Self>) {
        let name = self
            .projects
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.name.clone())
            .unwrap_or_default();

        let outcome = self
            .store
            .as_ref()
            .map(|store| store.delete_project(id));

        match outcome {
            Some(Ok(())) => {
                if self.selected == Some(id) {
                    self.selected = None;
                }
                self.reload(cx);
                self.push_toast(ToastKind::Success, format!("Removed {name} from Wisp."));
            }
            Some(Err(error)) => self.push_toast(ToastKind::Error, error.to_string()),
            None => self.push_toast(
                ToastKind::Error,
                "The project database is unavailable.".to_string(),
            ),
        }
        cx.notify();
    }

    // ---- settings toggles ------------------------------------------------

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
                            this.update =
                                if should_offer_update(manual, &version, skipped.as_deref()) {
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

    fn spawn_auto_update_check(cx: &mut Context<Self>) {
        cx.spawn(async move |entity, cx| {
            cx.background_executor().timer(AUTO_CHECK_DELAY).await;
            entity
                .update(cx, |this, cx| {
                    if this.settings.update_check_due() {
                        this.check_updates(cx, false);
                        return;
                    }
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

    fn dismiss_update(&mut self, version: String, cx: &mut Context<Self>) {
        self.settings.skipped_version = Some(version);
        self.settings.save();
        self.update = UpdateState::Idle;
        cx.notify();
    }

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
                .px(px(20.))
                .pb(px(12.))
                .child(
                    row.px(px(18.))
                        .py(px(12.))
                        .rounded_lg()
                        .bg(cx.theme().secondary)
                        .border_1()
                        .border_color(cx.theme().border),
                )
                .into_any_element(),
        )
    }

    // ---- chrome ----------------------------------------------------------

    fn top_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_4()
            .px(px(20.))
            .py(px(14.))
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
                            .size(px(30.))
                            .rounded_md()
                            .child(img(APP_ICON).size(px(30.))),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Wisp"),
                            )
                            .child(ui::eyebrow("Local PHP", cx)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("new-project")
                            .primary()
                            .small()
                            .icon(icon(IconName::Plus))
                            .label("New project")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_editor(None, window, cx)
                            })),
                    )
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
                    .submenu_with_icon(Some(icon(IconName::Palette)), "Theme", window, cx, {
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
                    })
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

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.selected;

        let body = if self.projects.is_empty() {
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap_2()
                .px(px(20.))
                .py(px(40.))
                .text_center()
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("No projects yet"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child("Add one to serve it on a .test domain."),
                )
                .into_any_element()
        } else {
            div()
                .id("project-list")
                .flex()
                .flex_col()
                .gap_1()
                .p(px(10.))
                .overflow_y_scroll()
                .children(self.projects.iter().map(|project| {
                    let is_selected = selected == Some(project.id);
                    project_row(project, is_selected, cx)
                }))
                .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .w(px(300.))
            .h_full()
            .flex_shrink(0.0)
            .border_r_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().muted)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px(px(16.))
                    .py(px(12.))
                    .child(ui::eyebrow("Projects", cx))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("{}", self.projects.len())),
                    ),
            )
            .child(div().flex_1().overflow_hidden().child(body))
    }

    fn content(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(project) = self.selected_project() else {
            return div()
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
                        .child("No project selected"),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("Choose a project, or create a new one."),
                )
                .into_any_element();
        };

        let id = project.id;
        let name = project.name.clone();
        let domain = project.domain.clone();
        let root = project.root_path.clone();
        let php = project
            .php_version
            .clone()
            .unwrap_or_else(|| "Not set".to_string());
        let ssl = project.ssl_enabled;
        let slug = project.slug();

        let header = div()
            .flex()
            .flex_row()
            .items_start()
            .justify_between()
            .gap_4()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_2xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(name),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(ui::dot(brand::accent()))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format!("{}{}", scheme_prefix(ssl), domain)),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("edit-project")
                            .outline()
                            .small()
                            .icon(icon(IconName::SquarePen))
                            .label("Edit")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let existing =
                                    this.projects.iter().find(|p| p.id == id).cloned();
                                this.open_editor(existing, window, cx);
                            })),
                    )
                    .child(
                        Button::new("delete-project")
                            .ghost()
                            .small()
                            .icon(icon(IconName::Trash))
                            .label("Remove")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.confirm_delete(id, window, cx)
                            })),
                    ),
            );

        let details = ui::plate(
            cx,
            div()
                .flex()
                .flex_col()
                .p(px(18.))
                .gap_4()
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(cx.theme().muted_foreground)
                        .child("PROJECT"),
                )
                .child(detail_row("Root folder", &root, cx))
                .child(detail_row("Domain", &format!("{}{}", scheme_prefix(ssl), domain), cx))
                .child(detail_row("HTTPS", if ssl { "Trusted certificate" } else { "Off" }, cx))
                .child(detail_row("PHP", &php, cx))
                .child(detail_row("Config folder", &format!("sites/{slug}"), cx)),
        );

        let services = ui::plate(
            cx,
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .p(px(18.))
                .child(icon(IconName::Server).text_color(brand::accent()))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .child("Services"),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("nginx and PHP start here once the runtime manager lands next."),
                        ),
                ),
        );

        div()
            .id("content")
            .flex()
            .flex_col()
            .size_full()
            .gap_6()
            .p(px(28.))
            .overflow_y_scroll()
            .child(header)
            .child(details)
            .child(services)
            .into_any_element()
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
            .child(self.top_bar(cx));

        if let Some(message) = self.error.clone() {
            root = root.child(
                div().px(px(20.)).pb(px(12.)).child(
                    Alert::error("db-error", message)
                        .title("Something went wrong")
                        .banner(),
                ),
            );
        }

        if let Some(banner) = self.update_banner(cx) {
            root = root.child(banner);
        }

        root.child(
            div()
                .flex()
                .flex_row()
                .flex_1()
                .overflow_hidden()
                .child(self.sidebar(cx))
                .child(self.content(cx)),
        )
    }
}

/// A single project in the sidebar.
fn project_row(project: &Project, selected: bool, cx: &mut Context<Wisp>) -> impl IntoElement {
    let id = project.id;
    let name = project.name.clone();
    let domain = project.domain.clone();

    let core = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .px(px(12.))
        .py(px(10.))
        .rounded_lg()
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.))
                .gap_1()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(name),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(domain),
                ),
        )
        .child(if project.ssl_enabled {
            icon(IconName::Lock)
                .small()
                .text_color(brand::accent())
                .into_any_element()
        } else {
            div().into_any_element()
        });

    let base = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .rounded_lg();

    let styled = if selected {
        base.bg(brand::accent_wash())
            .border_1()
            .border_color(brand::accent_tint())
    } else {
        base
    };

    styled
        .id(("project", id as u64))
        .hover(|d| d.bg(cx.theme().list_hover))
        .on_click(cx.listener(move |this, _, _window, cx| this.select(id, cx)))
        .child(if selected {
            ui::accent_rail().into_any_element()
        } else {
            div().w(px(3.)).into_any_element()
        })
        .child(core)
}

/// A `label  value` row, the label muted and fixed-width.
fn detail_row(label: &str, value: &str, cx: &App) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_start()
        .gap_4()
        .child(
            div()
                .w(px(120.))
                .flex_shrink(0.0)
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(label.to_string()),
        )
        .child(
            div()
                .flex_1()
                .text_sm()
                .whitespace_normal()
                .child(value.to_string()),
        )
}

/// The body of the create/edit dialog.
fn editor_form(editor: &EditorState, entity: &Entity<Wisp>, error: Option<String>) -> impl IntoElement {
    let php_label = editor
        .php_version
        .clone()
        .unwrap_or_else(|| "Not set".to_string());

    let php_menu = Button::new("php-version")
        .outline()
        .label(php_label)
        .dropdown_menu_with_anchor(Anchor::BottomLeft, {
            let entity = entity.clone();
            move |menu, _, _| {
                let entity = entity.clone();
                let mut menu = menu.check_side(Side::Right);
                for version in PHP_VERSIONS {
                    let version = version.to_string();
                    menu = menu.item(
                        PopupMenuItem::new(version.clone()).on_click({
                            let entity = entity.clone();
                            move |_, _, cx| {
                                let version = version.clone();
                                entity.update(cx, |this, cx| {
                                    if let Some(editor) = this.editor.as_mut() {
                                        editor.php_version = Some(version.clone());
                                    }
                                    cx.notify();
                                });
                            }
                        }),
                    );
                }
                menu
            }
        });

    let mut form = Form::new()
        .child(
            Field::new()
                .label("Name")
                .description("How the project is labelled in Wisp.")
                .child(Input::new(&editor.name)),
        )
        .child(
            Field::new()
                .label("App name")
                .description("Used for the default domain and config folder.")
                .child(Input::new(&editor.app_name)),
        )
        .child(
            Field::new()
                .label("Root folder")
                .description("The project's document root.")
                .child(Input::new(&editor.root_path)),
        )
        .child(
            Field::new()
                .label("Domain")
                .description("Must end with .test. HTTPS uses a locally-trusted certificate.")
                .child(Input::new(&editor.domain)),
        )
        .child(Field::new().label("PHP").child(php_menu))
        .child(
            Field::new().label("HTTPS").child({
                let entity = entity.clone();
                Switch::new("ssl")
                    .checked(editor.ssl_enabled)
                    .label("Trusted certificate")
                    .on_change(move |checked, _, cx| {
                        let checked = *checked;
                        entity.update(cx, |this, cx| {
                            if let Some(editor) = this.editor.as_mut() {
                                editor.ssl_enabled = checked;
                            }
                            cx.notify();
                        });
                    })
            }),
        );

    if let Some(message) = error {
        form = form.footer(
            div()
                .w_full()
                .text_sm()
                .text_color(cx_error())
                .child(message),
        );
    }

    form
}

/// The dialog footer: Cancel and Save.
fn editor_footer(entity: Entity<Wisp>, is_new: bool) -> impl IntoElement {
    let cancel = Button::new("editor-cancel")
        .label("Cancel")
        .outline()
        .on_click({
            let entity = entity.clone();
            move |_, window, cx| {
                entity.update(cx, |this, cx| this.cancel_editor(cx));
                window.close_dialog(cx);
            }
        });

    let save = Button::new("editor-save")
        .label(if is_new { "Create project" } else { "Save changes" })
        .primary()
        .on_click(move |_, window, cx| {
            let saved = entity.update(cx, |this, cx| this.save_editor(cx));
            if saved {
                window.close_dialog(cx);
            }
        });

    div()
        .flex()
        .flex_row()
        .justify_end()
        .gap_2()
        .child(cancel)
        .child(save)
}

fn cx_error() -> Hsla {
    rgb(0xEF4444).into()
}

fn scheme_prefix(ssl: bool) -> &'static str {
    if ssl {
        "https://"
    } else {
        "http://"
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

fn repository_label() -> &'static str {
    REPOSITORY_URL
        .strip_prefix("https://")
        .unwrap_or(REPOSITORY_URL)
}

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

/// Apply a three-way theme preference, then reassert the brand on top.
pub fn apply_theme(pref: ThemePreference, window: Option<&mut Window>, cx: &mut App) {
    match pref {
        ThemePreference::Light => Theme::change(ThemeMode::Light, window, cx),
        ThemePreference::Dark => Theme::change(ThemeMode::Dark, window, cx),
        ThemePreference::System => Theme::sync_system_appearance(window, cx),
    }
    brand::apply(cx);
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
