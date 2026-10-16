//! The Wisp window.
//!
//! Layout: a slim sidebar of projects beside a detail pane. Every card is the
//! shared "tray and plate" primitive, the brand accent marks the active project
//! and the primary action, and nothing else carries colour.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::base::Disableable as _;
use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::chart::AreaChart;
use gpui_kit::component::command::{Command, CommandItem, CommandState};
use gpui_kit::component::form::{Field, Form};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::link::Link;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::setting::{
    SettingField, SettingGroup, SettingItem, SettingPage, Settings as SettingsView,
};
use gpui_kit::component::separator::Separator;
use gpui_kit::component::sidebar::{
    Sidebar, SidebarFooter, SidebarGroup, SidebarHeader, SidebarMenu, SidebarMenuItem,
};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::toolbar::Toolbar;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme as _, Icon, Side, Sizable as _, Theme, ThemeMode, WindowExt as _,
};
use gpui_kit::*;

use crate::assets::APP_ICON;
use crate::autostart;
use crate::db::{self, InstalledRuntime, Project, ProjectDraft, Store};
use crate::elevation::{self, ActionOutcome, ControlMode};
use crate::helper_task::{self, TaskError};
use crate::hosts;
use crate::logs;
use crate::platform;
use crate::runtime::{self, catalog::Catalog, catalog::Release, RuntimeKind};
use crate::serve::{self, SiteSpec};
use crate::settings::{Settings, ThemePreference};
use crate::shell;
use crate::tls;
use crate::tray;
use crate::ui;
use crate::update;
use crate::vcrt;

/// Delay before the automatic launch update check, so start-up stays snappy.
const AUTO_CHECK_DELAY: Duration = Duration::from_secs(6);

/// How long a fetched runtime catalog stays fresh before Services refreshes it.
const CATALOG_TTL_SECS: u64 = 12 * 60 * 60;

/// CPU/RAM samples kept for the sparklines: 150 at 2s each is five minutes.
const METRIC_SAMPLES: usize = 150;

actions!(
    wisp,
    [TogglePalette, GoRuntimes, GoSettings, NewProject]
);

/// Seconds since the Unix epoch.
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

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

/// Which top-level screen the sidebar is showing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Projects,
    Services,
    Settings,
}

/// Which pane of a project's detail is showing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ProjectTab {
    Overview,
    Services,
    Logs,
    Settings,
}

impl ProjectTab {
    fn index(self) -> usize {
        match self {
            ProjectTab::Overview => 0,
            ProjectTab::Services => 1,
            ProjectTab::Logs => 2,
            ProjectTab::Settings => 3,
        }
    }

    fn from_index(index: usize) -> Self {
        match index {
            1 => ProjectTab::Services,
            2 => ProjectTab::Logs,
            3 => ProjectTab::Settings,
            _ => ProjectTab::Overview,
        }
    }
}

/// Which pane of the Runtimes screen is showing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RuntimeTab {
    Php,
    Nginx,
    Https,
    Hosts,
    Resources,
}

impl RuntimeTab {
    fn index(self) -> usize {
        match self {
            RuntimeTab::Php => 0,
            RuntimeTab::Nginx => 1,
            RuntimeTab::Https => 2,
            RuntimeTab::Hosts => 3,
            RuntimeTab::Resources => 4,
        }
    }

    fn from_index(index: usize) -> Self {
        match index {
            1 => RuntimeTab::Nginx,
            2 => RuntimeTab::Https,
            3 => RuntimeTab::Hosts,
            4 => RuntimeTab::Resources,
            _ => RuntimeTab::Php,
        }
    }
}

/// Process-wide owner of nginx and the PHP workers, so the app-quit hook can
/// stop them, and the background sync can move it in and out.
pub struct ServerGlobal(pub serve::Supervisor);

impl Global for ServerGlobal {}

/// Which log file the project detail is showing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LogKind {
    Access,
    Errors,
    Php,
    Worker,
}

impl LogKind {
    fn label(self) -> &'static str {
        match self {
            LogKind::Access => "Access",
            LogKind::Errors => "Errors",
            LogKind::Php => "PHP",
            LogKind::Worker => "Worker",
        }
    }

    fn file(self) -> &'static str {
        match self {
            LogKind::Access => "access.log",
            LogKind::Errors => "error.log",
            LogKind::Php => "php-error.log",
            LogKind::Worker => "php-cgi.log",
        }
    }
}

/// The create/edit dialog's live state. `id` is `None` while creating.
struct EditorState {
    id: Option<i64>,
    name: Entity<InputState>,
    app_name: Entity<InputState>,
    root_path: Entity<InputState>,
    domain: Entity<InputState>,
    /// The suffix chosen next to the domain label, e.g. `.test`.
    domain_suffix: String,
    /// While true, the domain label mirrors the app name; typing in the domain
    /// turns it off.
    domain_auto: bool,
    php_version: Option<String>,
    ssl_enabled: bool,
    /// Extra `php.ini` directives, one per line, appended last.
    overrides: Entity<TextareaState>,
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

    screen: Screen,
    runtimes: Vec<InstalledRuntime>,
    catalog: Option<Catalog>,
    catalog_loading: bool,
    catalog_error: Option<String>,
    /// Unix time the catalog was last fetched, so it can refresh when stale.
    catalog_fetched_at: Option<u64>,
    /// The runtime action in flight, keyed as `"php/8.4"` or `"nginx/1.27.0"`.
    busy_runtime: Option<String>,

    /// Projects whose server block nginx should serve.
    nginx_on: HashSet<i64>,
    /// Projects whose php-cgi worker should run.
    php_on: HashSet<i64>,
    serve_status: serve::Status,
    serve_error: Option<String>,
    serve_busy: bool,
    trust_busy: bool,

    /// How privileged hosts writes are performed (probed on start-up).
    mode: ControlMode,
    elevation_busy: bool,
    /// The domains last written to the hosts file, so we only elevate on change.
    hosts_written: Vec<String>,
    /// A folder chosen in the native picker, applied to the editor on next render
    /// (which is where a `Window` is available).
    pending_root: Option<String>,
    /// Which log the project detail shows, and its tail.
    log_kind: LogKind,
    log_lines: Vec<String>,
    /// Which pane of a project's detail is showing.
    project_tab: ProjectTab,
    /// Filters the sidebar's project list.
    project_filter: Entity<InputState>,
    /// Log search, follow and scroll state.
    log_filter: Entity<InputState>,
    log_case: bool,
    log_follow: bool,
    log_match: usize,
    log_scroll: ScrollHandle,
    /// The last set of sites published to the tray, to avoid needless rebuilds.
    tray_sites: Vec<tray::TraySite>,
    /// CPU and RAM percentages, one per 2s sample, capped at five minutes.
    metrics: VecDeque<(f64, f64)>,
    /// The Ctrl-K command palette and whether it is showing.
    palette: Entity<CommandState>,
    palette_open: bool,
    /// Which pane of the Runtimes screen is showing.
    runtime_tab: RuntimeTab,
    _subscriptions: Vec<Subscription>,
}

impl Wisp {
    pub fn new(settings: Settings, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (store, error) = match Store::open() {
            Ok(store) => (Some(store), None),
            Err(error) => (
                None,
                Some(format!("Could not open the project database: {error}")),
            ),
        };

        // Filters the project list; typing re-renders the sidebar.
        let project_filter = cx.new(|cx| InputState::new(window, cx).placeholder("Search projects"));
        let log_filter = cx.new(|cx| InputState::new(window, cx).placeholder("Find in log"));
        let palette = cx.new(|cx| CommandState::new(window, cx));
        let _subscriptions = vec![
            cx.subscribe_in(
                &project_filter,
                window,
                |_this, _state, event, _window, cx| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(&log_filter, window, |this, _state, event, _window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.log_match = 0;
                    cx.notify();
                }
            }),
        ];

        let mut this = Self {
            theme: settings.theme,            settings,
            toasts: Vec::new(),
            update: UpdateState::Idle,
            autostart: autostart::is_enabled(),
            store,
            projects: Vec::new(),
            selected: None,
            editor: None,
            error,
            screen: Screen::Projects,
            runtimes: Vec::new(),
            catalog: None,
            catalog_loading: false,
            catalog_error: None,
            catalog_fetched_at: None,
            busy_runtime: None,
            nginx_on: HashSet::new(),
            php_on: HashSet::new(),
            serve_status: serve::Status::default(),
            serve_error: None,
            serve_busy: false,
            trust_busy: false,
            mode: ControlMode::Uac,
            elevation_busy: false,
            hosts_written: Vec::new(),
            pending_root: None,
            log_kind: LogKind::Php,
            log_lines: Vec::new(),
            project_filter,
            project_tab: ProjectTab::Overview,
            log_filter,
            log_case: false,
            log_follow: true,
            log_match: 0,
            log_scroll: ScrollHandle::default(),
            tray_sites: Vec::new(),
            metrics: VecDeque::new(),
            palette,
            palette_open: false,
            runtime_tab: RuntimeTab::Php,
            _subscriptions,
        };
        this.reload(cx);
        // Show the first project on launch rather than an empty pane.
        if this.selected.is_none() {
            this.selected = this.projects.first().map(|project| project.id);
        }
        this.reload_runtimes(cx);
        this.load_catalog(cx);
        Self::spawn_status_poll(cx);
        Self::spawn_log_poll(cx);
        Self::spawn_metrics_poll(cx);
        Self::spawn_mode_probe(cx);

        Self::spawn_auto_update_check(cx);

        // The tray's menu reads this, and its actions come back to `handle_tray`.
        cx.set_global(tray::TrayMenuData::default());
        let weak = cx.weak_entity();
        cx.set_global(tray::TrayHooks(Some(Box::new(move |request, cx| {
            let _ = weak.update(cx, |this, cx| this.handle_tray(request, cx));
        }))));

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
        // A new project starts back on Overview, and any in-flight edit is lost.
        if self.selected != Some(id) {
            self.project_tab = ProjectTab::Overview;
            self.editor = None;
        }
        self.selected = Some(id);
        self.screen = Screen::Projects;
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
                php_ini_overrides: String::new(),
                ..Default::default()
            });

        let (domain_label, domain_suffix) = db::split_domain(&draft.domain);

        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("My App")
                .default_value(draft.name.clone())
        });
        let app_name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("my-app")
                .default_value(draft.app_name.clone())
        });
        let root_path = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(r"C:\Sites\my-app")
                .default_value(draft.root_path.clone())
        });
        let domain = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("my-app")
                .default_value(domain_label)
        });
        let overrides = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("upload_max_filesize = 128M\nmax_execution_time = 300")
                .default_value(draft.php_ini_overrides.clone())
        });

        // The domain mirrors the app name until the user edits it directly.
        let app_sub = cx.subscribe_in(
            &app_name,
            window,
            |this, _state, event, window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.sync_domain(window, cx);
                }
            },
        );
        let domain_sub = cx.subscribe_in(
            &domain,
            window,
            |this, _state, event, _window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.note_domain_edit(cx);
                }
            },
        );

        self.editor = Some(EditorState {
            id: existing.as_ref().map(|p| p.id),
            name,
            app_name,
            root_path,
            domain,
            domain_suffix,
            // An existing project keeps its own domain; a new one follows along.
            domain_auto: existing.is_none(),
            php_version: draft.php_version,
            ssl_enabled: draft.ssl_enabled,
            overrides,
            error: None,
            _subscriptions: vec![app_sub, domain_sub],
        });

        if existing.is_some() {
            // Editing an existing project renders in place on its Settings tab.
            self.project_tab = ProjectTab::Settings;
            cx.notify();
            return;
        }

        let entity = cx.entity();
        window.open_dialog(cx, move |dialog, window, cx| {
            let php_versions = entity.read(cx).installed_php_versions();
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
                .child(editor_form(editor, &entity, error, &php_versions, window, cx))
                .footer(editor_footer(entity.clone(), is_new))
        });
    }

    /// Mirror the app name into the domain while the domain is still automatic.
    fn sync_domain(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.as_ref() else {
            return;
        };
        if !editor.domain_auto {
            return;
        }
        let slug = db::slugify(&editor.app_name.read(cx).value());
        editor
            .domain
            .update(cx, |state, cx| state.set_value(slug, window, cx));
    }

    /// Stop auto-filling the domain once the user types their own.
    fn note_domain_edit(&mut self, cx: &mut Context<Self>) {
        let (typed, expected) = {
            let Some(editor) = self.editor.as_ref() else {
                return;
            };
            (
                editor.domain.read(cx).value().to_string(),
                db::slugify(&editor.app_name.read(cx).value()),
            )
        };
        if domain_is_hand_edited(&typed, &expected) {
            if let Some(editor) = self.editor.as_mut() {
                editor.domain_auto = false;
            }
        }
    }

    /// Open the native folder picker. The result arrives asynchronously, so it
    /// is stashed and applied on the next render, where a `Window` is available.
    fn browse_root(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.editor.is_none() {
            return;
        }
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let picked = rfd::FileDialog::new()
                .set_title("Choose the project's root folder")
                .pick_folder();
            let _ = sender.send(picked);
        });

        cx.spawn(async move |entity, cx| {
            let picked = cx
                .background_executor()
                .spawn(async move { receiver.recv().ok().flatten() })
                .await;
            if let Some(folder) = picked {
                entity
                    .update(cx, |this, cx| {
                        this.pending_root = Some(folder.to_string_lossy().to_string());
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();
    }

    /// Apply a folder chosen in the picker to the editor's root input.
    fn apply_pending_root(&mut self, window: &mut Window, cx: &mut Context<Self>) {        let Some(path) = self.pending_root.take() else {
            return;
        };
        if let Some(editor) = self.editor.as_ref() {
            editor
                .root_path
                .update(cx, |state, cx| state.set_value(path, window, cx));
        }
    }

    /// Reveal a folder in File Explorer, warning if it is not there.
    fn open_folder(&mut self, path: &str, cx: &mut Context<Self>) {
        if !std::path::Path::new(path).is_dir() {
            self.push_toast(
                ToastKind::Warning,
                format!("That folder is not there: {path}"),
            );
        } else if let Err(error) = shell::open_folder(path) {
            self.push_toast(ToastKind::Error, error.to_string());
        }
        cx.notify();
    }

    /// Open a project's domain in the default browser.
    fn open_domain(&mut self, url: String, cx: &mut Context<Self>) {
        if let Err(error) = shell::open_url(&url) {
            self.push_toast(ToastKind::Error, error.to_string());
        }
        cx.notify();
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
            domain: format!(
                "{}{}",
                editor.domain.read(cx).value().trim(),
                editor.domain_suffix
            ),
            php_version: editor.php_version.clone(),
            ssl_enabled: editor.ssl_enabled,
            php_ini_overrides: editor.overrides.read(cx).value().to_string(),
        };
        let id = editor.id;
        let before = id.and_then(|id| self.projects.iter().find(|p| p.id == id).cloned());

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
                self.screen = Screen::Projects;
                self.reload(cx);

                // If the project is running and something that affects how it is
                // served changed, apply it now. A rename alone changes nothing.
                if self.is_on(saved_id) {
                    let after = self.projects.iter().find(|p| p.id == saved_id).cloned();
                    if let (Some(before), Some(after)) = (before, after) {
                        if service_config_changed(&before, &after) {
                            self.sync_server(cx);
                        }
                    }
                }

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
                self.sync_hosts(cx);
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

    // ---- runtimes --------------------------------------------------------

    fn goto(&mut self, screen: Screen, cx: &mut Context<Self>) {
        if self.screen != screen {
            self.screen = screen;
            // Opening Services with a stale catalog refreshes it in the
            // background, so new PHP and nginx releases show up without a click.
            if screen == Screen::Services && self.catalog_is_stale() {
                self.load_catalog(cx);
            }
            cx.notify();
        }
    }

    /// Whether the catalog should be refetched.
    fn catalog_is_stale(&self) -> bool {
        match self.catalog_fetched_at {
            Some(at) => now_unix().saturating_sub(at) >= CATALOG_TTL_SECS,
            None => true,
        }
    }

    fn reload_runtimes(&mut self, cx: &mut Context<Self>) {
        if let Some(store) = &self.store {
            match store.list_runtimes() {
                Ok(runtimes) => self.runtimes = runtimes,
                Err(error) => self.error = Some(error.to_string()),
            }
        }
        cx.notify();
    }

    fn load_catalog(&mut self, cx: &mut Context<Self>) {
        if self.catalog_loading {
            return;
        }
        self.catalog_loading = true;
        self.catalog_error = None;
        cx.notify();

        cx.spawn(async move |entity, cx| {
            let result = cx
                .background_executor()
                .spawn(async { Catalog::fetch() })
                .await;
            entity
                .update(cx, |this, cx| {
                    this.catalog_loading = false;
                    match result {
                        Ok(catalog) => {
                            this.catalog = Some(catalog);
                            this.catalog_error = None;
                            this.catalog_fetched_at = Some(now_unix());
                        }
                        Err(error) => this.catalog_error = Some(error.to_string()),
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    /// Installed versions of a kind, newest first.
    fn installed(&self, kind: RuntimeKind) -> Vec<InstalledRuntime> {
        let mut list: Vec<InstalledRuntime> = self
            .runtimes
            .iter()
            .filter(|runtime| runtime.kind == kind.as_str())
            .cloned()
            .collect();
        list.sort_by_key(|runtime| std::cmp::Reverse(version_key(&runtime.version)));
        list
    }

    /// Installed PHP versions, for the editor's dropdown.
    fn installed_php_versions(&self) -> Vec<String> {
        self.installed(RuntimeKind::Php)
            .into_iter()
            .map(|runtime| runtime.version)
            .collect()
    }

    /// The highest VC++ runtime an installed PHP build needs but is missing.
    fn php_vcrt_gap(&self) -> Option<u16> {
        let catalog = self.catalog.as_ref()?;
        let mut needed: Option<u16> = None;
        for runtime in &self.runtimes {
            if runtime.kind != RuntimeKind::Php.as_str() {
                continue;
            }
            let Some(release) = catalog.php.iter().find(|r| r.version == runtime.version) else {
                continue;
            };
            if let Some(vc) = release.vc.filter(|vc| !vcrt::satisfies(*vc)) {
                needed = Some(needed.map_or(vc, |current: u16| current.max(vc)));
            }
        }
        needed
    }

    fn install_release(&mut self, kind: RuntimeKind, release: Release, cx: &mut Context<Self>) {
        if self.busy_runtime.is_some() {
            return;
        }
        let key = format!("{}/{}", kind.as_str(), release.version);
        self.busy_runtime = Some(key);
        cx.notify();

        let version = release.version.clone();
        cx.spawn(async move |entity, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    runtime::install(kind, &release).map(|path| path.to_string_lossy().to_string())
                })
                .await;

            entity
                .update(cx, |this, cx| {
                    this.busy_runtime = None;
                    match outcome {
                        Ok(path) => {
                            if let Some(store) = &this.store {
                                let _ = store.add_runtime(kind.as_str(), &version, &path);
                            }
                            // A freshly installed nginx becomes the active one.
                            if kind == RuntimeKind::Nginx {
                                this.settings.active_nginx = Some(version.clone());
                                this.settings.save();
                            }
                            this.reload_runtimes(cx);
                            this.push_toast(
                                ToastKind::Success,
                                format!("Installed {} {version}.", kind.label()),
                            );
                        }
                        Err(error) => {
                            this.push_toast(ToastKind::Error, error.to_string());
                        }
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    /// Replace an installed PHP patch with a newer one, repointing projects.
    fn update_php(&mut self, current: String, release: Release, cx: &mut Context<Self>) {
        if self.busy_runtime.is_some() {
            return;
        }
        let key = format!("php/{}", release.version);
        self.busy_runtime = Some(key);
        cx.notify();

        let version = release.version.clone();
        let previous = current.clone();
        cx.spawn(async move |entity, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    let path = runtime::install(RuntimeKind::Php, &release)?;
                    // Only drop the old patch once the new one is in place.
                    let _ = runtime::remove(RuntimeKind::Php, &previous);
                    Ok::<_, runtime::RuntimeError>(path.to_string_lossy().to_string())
                })
                .await;

            entity
                .update(cx, |this, cx| {
                    this.busy_runtime = None;
                    match outcome {
                        Ok(path) => {
                            if let Some(store) = &this.store {
                                let _ = store.remove_runtime("php", &current);
                                let _ = store.add_runtime("php", &version, &path);
                                let _ = store.repoint_php(&current, &version);
                            }
                            this.reload_runtimes(cx);
                            this.reload(cx);
                            this.push_toast(
                                ToastKind::Success,
                                format!("Updated PHP to {version}."),
                            );
                        }
                        Err(error) => {
                            this.push_toast(ToastKind::Error, error.to_string());
                        }
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    fn uninstall_runtime(
        &mut self,
        kind: RuntimeKind,
        version: String,
        cx: &mut Context<Self>,
    ) {
        if self.busy_runtime.is_some() {
            return;
        }
        let key = format!("{}/{version}", kind.as_str());
        self.busy_runtime = Some(key);
        cx.notify();

        let removing = version.clone();
        cx.spawn(async move |entity, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move { runtime::remove(kind, &removing) })
                .await;

            entity
                .update(cx, |this, cx| {
                    this.busy_runtime = None;
                    match outcome {
                        Ok(()) => {
                            if let Some(store) = &this.store {
                                let _ = store.remove_runtime(kind.as_str(), &version);
                            }
                            if kind == RuntimeKind::Nginx
                                && this.settings.active_nginx.as_deref() == Some(version.as_str())
                            {
                                this.settings.active_nginx = None;
                                this.settings.save();
                            }
                            this.reload_runtimes(cx);
                            this.push_toast(
                                ToastKind::Success,
                                format!("Removed {} {version}.", kind.label()),
                            );
                        }
                        Err(error) => {
                            this.push_toast(ToastKind::Error, error.to_string());
                        }
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    fn set_active_nginx(&mut self, version: String, cx: &mut Context<Self>) {
        if self.settings.active_nginx.as_deref() == Some(version.as_str()) {
            return;
        }
        self.settings.active_nginx = Some(version.clone());
        self.settings.save();
        self.push_toast(
            ToastKind::Success,
            format!("nginx {version} is now the active version."),
        );
        cx.notify();
    }

    fn install_vcrt(&mut self, cx: &mut Context<Self>) {        match vcrt::launch_installer() {
            Ok(()) => self.push_toast(
                ToastKind::Success,
                "The Visual C++ installer is opening. Restart Wisp when it finishes.".to_string(),
            ),
            Err(error) => self.push_toast(ToastKind::Error, error),
        }
        cx.notify();
    }

    // ---- serving ---------------------------------------------------------

    /// Periodically reconcile the reported process status with reality.
    fn spawn_status_poll(cx: &mut Context<Self>) {
        cx.spawn(async move |entity, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(2000))
                    .await;
                let applied = entity.update(cx, |this, cx| {
                    // While a sync is in flight the supervisor is checked out to
                    // the background task, so a status read would be misleading.
                    if this.serve_busy {
                        return;
                    }
                    let status = cx.global_mut::<ServerGlobal>().0.status();
                    if status.nginx_running != this.serve_status.nginx_running
                        || status.running_projects != this.serve_status.running_projects
                    {
                        this.serve_status = status;
                        cx.notify();
                    }
                });
                if applied.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Whether a project is served or has a worker.
    fn is_on(&self, id: i64) -> bool {
        self.nginx_on.contains(&id) || self.php_on.contains(&id)
    }

    /// The specs for the projects the user asked to serve or run.
    fn site_specs(&self) -> std::result::Result<Vec<SiteSpec>, String> {
        let mut specs = Vec::new();
        for project in &self.projects {
            let vhost = self.nginx_on.contains(&project.id);
            let worker = self.php_on.contains(&project.id);
            if !vhost && !worker {
                continue;
            }

            let php = project.php_version.clone().filter(|value| !value.is_empty());
            if worker {
                let Some(php) = php.clone() else {
                    return Err(format!(
                        "\"{}\" has no PHP version set. Choose one in Edit.",
                        project.name
                    ));
                };
                if !runtime::php_cgi(&php).is_file() {
                    return Err(format!(
                        "PHP {php} is not installed (needed by \"{}\").",
                        project.name
                    ));
                }
            }

            specs.push(SiteSpec {
                id: project.id,
                slug: project.slug(),
                domain: project.domain.clone(),
                root: PathBuf::from(&project.root_path),
                php_version: php.unwrap_or_default(),
                ssl_enabled: project.ssl_enabled,
                php_ini_overrides: project.php_ini_overrides.clone(),
                vhost,
                worker,
            });
        }
        Ok(specs)
    }

    /// Turn this project's server block on or off.
    fn toggle_nginx(&mut self, id: i64, cx: &mut Context<Self>) {
        if !self.nginx_on.remove(&id) {
            self.nginx_on.insert(id);
        }
        self.sync_server(cx);
    }

    /// Turn this project's PHP worker on or off.
    fn toggle_php(&mut self, id: i64, cx: &mut Context<Self>) {
        if !self.php_on.remove(&id) {
            self.php_on.insert(id);
        }
        self.sync_server(cx);
    }

    /// Reconcile nginx and the PHP workers with the requested set.
    fn sync_server(&mut self, cx: &mut Context<Self>) {
        if self.serve_busy {
            return;
        }
        let specs = match self.site_specs() {
            Ok(specs) => specs,
            Err(message) => {
                self.serve_error = Some(message);
                cx.notify();
                return;
            }
        };
        let version = self.settings.active_nginx.clone();
        let mut supervisor = std::mem::take(&mut cx.global_mut::<ServerGlobal>().0);
        self.serve_busy = true;
        cx.notify();

        cx.spawn(async move |entity, cx| {
            let (supervisor, outcome) = cx
                .background_executor()
                .spawn(async move {
                    let outcome = supervisor
                        .sync(version.as_deref(), &specs)
                        .map(|()| supervisor.status());
                    (supervisor, outcome)
                })
                .await;

            entity
                .update(cx, |this, cx| {
                    cx.global_mut::<ServerGlobal>().0 = supervisor;
                    this.serve_busy = false;
                    match outcome {
                        Ok(status) => {
                            this.serve_status = status;
                            this.serve_error = None;
                        }
                        Err(error) => this.serve_error = Some(error.to_string()),
                    }
                    this.sync_hosts(cx);
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    /// Trust Wisp's local CA in the current user's root store, off the UI thread
    /// because Windows shows a confirmation dialog.
    fn trust_ca(&mut self, cx: &mut Context<Self>) {
        if self.trust_busy {
            return;
        }
        self.trust_busy = true;
        cx.notify();

        cx.spawn(async move |entity, cx| {
            let result = cx
                .background_executor()
                .spawn(async { tls::trust_ca() })
                .await;
            entity
                .update(cx, |this, cx| {
                    this.trust_busy = false;
                    match result {
                        Ok(()) => {
                            this.settings.ca_trusted = true;
                            this.settings.save();
                            this.push_toast(
                                ToastKind::Success,
                                "Wisp's certificate is trusted \u{2014} https:// will work in your browser."
                                    .to_string(),
                            );
                        }
                        Err(error) => this.push_toast(ToastKind::Error, error.to_string()),
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    /// Probe how privileged writes will be performed.
    fn spawn_mode_probe(cx: &mut Context<Self>) {
        cx.spawn(async move |entity, cx| {
            let mode = cx
                .background_executor()
                .spawn(async { elevation::detect_mode() })
                .await;
            entity
                .update(cx, |this, cx| {
                    this.mode = mode;
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    /// Keep the hosts file in step with the running projects.
    fn sync_hosts(&mut self, cx: &mut Context<Self>) {
        let mut domains: Vec<String> = self
            .projects
            .iter()
            .filter(|project| self.nginx_on.contains(&project.id))
            .map(|project| project.domain.clone())
            .collect();
        domains.sort();

        // Nothing to do when the file already matches.
        if domains == self.hosts_written {
            return;
        }
        // A previous run, or another tool, may already have written the entries.
        // Reading the file itself avoids an unnecessary elevation prompt.
        if !hosts::needs_update(&domains) {
            self.hosts_written = domains;
            return;
        }

        let mode = self.mode;
        let to_write = domains.clone();
        cx.spawn(async move |entity, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move { elevation::perform_hosts(mode, &to_write) })
                .await;
            entity
                .update(cx, |this, cx| {
                    match outcome {
                        ActionOutcome::Done => {
                            let should_report = this.hosts_written.is_empty() && !domains.is_empty();
                            this.hosts_written = domains;
                            // The first write may have installed no-prompt control.
                            if this.mode == ControlMode::Uac {
                                this.mode = elevation::detect_mode();
                            }
                            if should_report {
                                this.push_toast(
                                    ToastKind::Success,
                                    "Hosts updated \u{2014} the .test domains now resolve."
                                        .to_string(),
                                );
                            }
                        }
                        ActionOutcome::Cancelled => this.push_toast(
                            ToastKind::Warning,
                            "The hosts update was cancelled \u{2014} domains will not resolve."
                                .to_string(),
                        ),
                        ActionOutcome::Failed(message) => this.push_toast(ToastKind::Error, message),
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    fn setup_elevated(&mut self, cx: &mut Context<Self>) {
        if self.elevation_busy {
            return;
        }
        self.elevation_busy = true;
        cx.notify();

        cx.spawn(async move |entity, cx| {
            let result = cx
                .background_executor()
                .spawn(async {
                    let exe = std::env::current_exe().map_err(|error| {
                        TaskError::Setup(format!("could not locate the application: {error}"))
                    })?;
                    helper_task::setup(&exe)
                })
                .await;

            entity
                .update(cx, |this, cx| {
                    this.elevation_busy = false;
                    match result {
                        Ok(()) => {
                            this.mode = ControlMode::Task;
                            this.push_toast(
                                ToastKind::Success,
                                "No-prompt control is on \u{2014} hosting changes will not prompt."
                                    .to_string(),
                            );
                        }
                        Err(TaskError::Cancelled) => this.push_toast(
                            ToastKind::Warning,
                            "Setup cancelled \u{2014} administrator rights are needed once."
                                .to_string(),
                        ),
                        Err(error) => this.push_toast(ToastKind::Error, error.to_string()),
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    fn remove_elevated(&mut self, cx: &mut Context<Self>) {
        if self.elevation_busy {
            return;
        }
        self.elevation_busy = true;
        cx.notify();

        cx.spawn(async move |entity, cx| {
            let result = cx
                .background_executor()
                .spawn(async { helper_task::remove() })
                .await;
            entity
                .update(cx, |this, cx| {
                    this.elevation_busy = false;
                    match result {
                        Ok(()) => {
                            this.mode = ControlMode::Uac;
                            this.push_toast(
                                ToastKind::Success,
                                "No-prompt control is off \u{2014} hosting changes will prompt again."
                                    .to_string(),
                            );
                        }
                        Err(TaskError::RemoveCancelled) => this.push_toast(
                            ToastKind::Warning,
                            "Removal cancelled \u{2014} administrator rights are needed to remove it."
                                .to_string(),
                        ),
                        Err(error) => this.push_toast(ToastKind::Error, error.to_string()),
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    // ---- logs ------------------------------------------------------------

    fn selected_log_path(&self) -> Option<PathBuf> {
        let project = self.selected_project()?;
        Some(serve::config::site_logs(&project.slug()).join(self.log_kind.file()))
    }

    /// Re-read the selected log's tail.
    fn refresh_logs(&mut self, cx: &mut Context<Self>) {
        let lines = match self.selected_log_path() {
            Some(path) => logs::tail(&path, 256 * 1024, 400),
            None => Vec::new(),
        };
        if lines != self.log_lines {
            self.log_lines = lines;
            // Follow keeps the newest line in view; the exact bottom is known
            // after the next layout, so this lands a frame later.
            if self.log_follow {
                let max = self.log_scroll.max_offset();
                self.log_scroll.set_offset(point(px(0.), max.y));
            }
            cx.notify();
        }
    }

    /// Poll the selected project's log while the Projects screen is open.
    fn spawn_log_poll(cx: &mut Context<Self>) {
        cx.spawn(async move |entity, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(2000))
                    .await;
                let applied = entity.update(cx, |this, cx| {
                    if this.screen == Screen::Projects {
                        this.refresh_logs(cx);
                    }
                });
                if applied.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Sample CPU and memory every other second, keeping five minutes of history.
    fn spawn_metrics_poll(cx: &mut Context<Self>) {
        cx.spawn(async move |entity, cx| {
            let mut system = sysinfo::System::new();
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(2000))
                    .await;
                system.refresh_cpu_usage();
                system.refresh_memory();
                let cpu = system.global_cpu_usage() as f64;
                let total = system.total_memory();
                let used = system.used_memory();
                let ram = if total == 0 {
                    0.0
                } else {
                    used as f64 / total as f64 * 100.0
                };
                let applied = entity.update(cx, |this, cx| {
                    this.push_metrics(cpu, ram);
                    cx.notify();
                });
                if applied.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Add one sample, dropping the oldest once the window is full.
    fn push_metrics(&mut self, cpu: f64, ram: f64) {
        if self.metrics.len() == METRIC_SAMPLES {
            self.metrics.pop_front();
        }
        self.metrics.push_back((cpu, ram));
    }

    /// Move between search hits, scrolling the log towards the new one.
    fn step_log_match(&mut self, forward: bool, cx: &mut Context<Self>) {
        let raw = self.log_filter.read(cx).value().to_string();
        let needle = if self.log_case { raw } else { raw.to_lowercase() };
        if needle.is_empty() {
            return;
        }

        let hits: Vec<usize> = self
            .log_lines
            .iter()
            .enumerate()
            .filter(|(_, line)| {
                let hay = if self.log_case {
                    (*line).clone()
                } else {
                    line.to_lowercase()
                };
                hay.contains(&needle)
            })
            .map(|(index, _)| index)
            .collect();

        if hits.is_empty() {
            self.log_match = 0;
            cx.notify();
            return;
        }

        let count = hits.len();
        self.log_match = if forward {
            (self.log_match + 1) % count
        } else {
            (self.log_match + count - 1) % count
        };

        // Line offsets are not tracked, so map the hit's position onto the
        // scrollable range as an approximation.
        let fraction = self.log_match as f32 / count as f32;
        let max = self.log_scroll.max_offset();
        self.log_scroll
            .set_offset(point(px(0.), px(f32::from(max.y) * fraction)));
        cx.notify();
    }

    fn clear_log(&mut self, cx: &mut Context<Self>) {        if let Some(path) = self.selected_log_path() {
            let _ = std::fs::write(&path, "");
        }
        self.refresh_logs(cx);
    }

    fn open_log_folder(&mut self, cx: &mut Context<Self>) {
        if let Some(slug) = self.selected_project().map(|project| project.slug()) {
            let path = serve::config::site_logs(&slug).to_string_lossy().to_string();
            self.open_folder(&path, cx);
        }
    }

    // ---- service controls ------------------------------------------------

    fn refresh_serve_status(&mut self, cx: &mut Context<Self>) {
        self.serve_status = cx.global_mut::<ServerGlobal>().0.status();
        cx.notify();
    }

    /// Serve every project that has a PHP version set.
    fn start_all(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<i64> = self
            .projects
            .iter()
            .filter(|project| project.php_version.is_some())
            .map(|project| project.id)
            .collect();
        self.nginx_on = ids.iter().copied().collect();
        self.php_on = ids.into_iter().collect();
        self.sync_server(cx);
        self.sync_hosts(cx);
    }

    /// Stop every site: all workers and nginx.
    fn stop_all(&mut self, cx: &mut Context<Self>) {
        if self.serve_busy {
            return;
        }
        self.nginx_on.clear();
        self.php_on.clear();
        {
            let server = cx.global_mut::<ServerGlobal>();
            server.0.stop_workers();
            server.0.stop_nginx_only();
        }
        self.hosts_written.clear();
        self.sync_hosts(cx);
        self.refresh_serve_status(cx);
    }

    /// Restart every running site's worker, or start them if none are running.
    fn restart_all(&mut self, cx: &mut Context<Self>) {
        if self.serve_busy {
            return;
        }
        if self.nginx_on.is_empty() && self.php_on.is_empty() {
            self.start_all(cx);
            return;
        }
        self.force_worker_restart();
        self.sync_server(cx);
    }

    /// Restart one of a project's two services.
    fn restart_service(&mut self, id: i64, php: bool, cx: &mut Context<Self>) {
        if self.serve_busy {
            return;
        }
        // Removing the ini makes the next sync restart just this worker.
        if php {
            if let Some(project) = self.projects.iter().find(|project| project.id == id) {
                let _ = std::fs::remove_file(serve::config::site_dir(&project.slug()).join("php.ini"));
            }
        }
        self.sync_server(cx);
    }

    /// Act on a request from the tray. The window owns the running state, so all
    /// tray commands funnel through here rather than the supervisor directly.
    fn handle_tray(&mut self, request: tray::TrayRequest, cx: &mut Context<Self>) {
        match request {
            tray::TrayRequest::Site { id, command } => match command {
                tray::SiteCommand::Start => {
                    if !self.php_on.contains(&id) {
                        self.toggle_php(id, cx);
                    }
                    if !self.nginx_on.contains(&id) {
                        self.toggle_nginx(id, cx);
                    }
                }
                tray::SiteCommand::Stop => {
                    if self.nginx_on.contains(&id) {
                        self.toggle_nginx(id, cx);
                    }
                    if self.php_on.contains(&id) {
                        self.toggle_php(id, cx);
                    }
                }
                tray::SiteCommand::Restart => self.restart_service(id, true, cx),
                tray::SiteCommand::Open => {
                    let url = self
                        .projects
                        .iter()
                        .find(|project| project.id == id)
                        .map(|project| {
                            format!("{}{}", scheme_prefix(project.ssl_enabled), project.domain)
                        });
                    if let Some(url) = url {
                        self.open_domain(url, cx);
                    }
                }
            },
            tray::TrayRequest::All(command) => match command {
                tray::AllCommand::Start => self.start_all(cx),
                tray::AllCommand::Stop => self.stop_all(cx),
                tray::AllCommand::Restart => self.restart_all(cx),
            },
        }
        cx.notify();
    }

    /// Push the current site list to the tray, rebuilding the menu when it moved.
    fn publish_tray(&mut self, cx: &mut Context<Self>) {
        let sites: Vec<tray::TraySite> = self
            .projects
            .iter()
            .map(|project| {
                let id = project.id;
                let nginx = self.nginx_on.contains(&id);
                let php = self.php_on.contains(&id);
                tray::TraySite {
                    id,
                    name: project.name.clone(),
                    url: format!(
                        "{}{}",
                        scheme_prefix(project.ssl_enabled),
                        project.domain
                    ),
                    running: nginx && php,
                    partial: nginx || php,
                }
            })
            .collect();

        if sites != self.tray_sites {
            self.tray_sites = sites;
            cx.set_global(tray::TrayMenuData {
                sites: self.tray_sites.clone(),
            });
            tray::refresh(cx);
        }
    }

    /// Delete every project's `php.ini` so the next sync restarts each worker.
    fn force_worker_restart(&mut self) {
        for project in &self.projects {
            let _ = std::fs::remove_file(serve::config::site_dir(&project.slug()).join("php.ini"));
        }
    }

    fn set_hosting_control(&mut self, wanted: bool, cx: &mut Context<Self>) {        if wanted {
            self.setup_elevated(cx);
        } else {
            self.remove_elevated(cx);
        }
    }

    // ---- settings page ---------------------------------------------------

    fn settings_content(&self, cx: &mut Context<Self>) -> AnyElement {
        let entity = cx.entity();
        let settings = SettingsView::new("wisp-settings").pages([
            general_settings_page(&entity),
            hosting_settings_page(&entity, self),
            updates_settings_page(&entity, self),
            about_settings_page(),
        ]);

        div()
            .id("settings")
            .flex()
            .flex_col()
            .size_full()
            .min_w(px(0.))
            .p(px(28.))
            .child(settings)
            .into_any_element()
    }

    // ---- settings toggles ------------------------------------------------

    fn toggle_background(&mut self, cx: &mut Context<Self>) {
        self.set_background(!self.settings.background, cx);
    }

    fn set_background(&mut self, wanted: bool, cx: &mut Context<Self>) {
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
        self.set_autostart(!self.autostart, cx);
    }

    fn set_autostart(&mut self, wanted: bool, cx: &mut Context<Self>) {
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

    /// The same, from the Settings page, where there is no window to hand.
    fn set_theme_pref(&mut self, pref: ThemePreference, cx: &mut Context<Self>) {
        if self.theme == pref {
            return;
        }
        self.theme = pref;
        self.settings.theme = pref;
        self.settings.save();
        apply_theme(pref, None, cx);
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

    /// The window status bar: ambient state on the left, the current project in
    /// the middle, ambient notices and the global site controls on the right.
    ///
    /// The bar never wraps, so what it includes is chosen from the window width
    /// rather than left to clip.
    fn status_bar(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let width = window.bounds().size.width;
        let wide = width >= px(1120.);
        let compact = width >= px(940.);

        let nginx_up = self.serve_status.nginx_running;
        let workers = self.serve_status.running_projects.len();
        let desired = self.php_on.len();

        let state_dot = |on: bool, cx: &App| {
            ui::dot(if on {
                cx.theme().success
            } else {
                cx.theme().muted_foreground
            })
        };

        // Left: the shared server and the workers.
        let nginx_label = match (&self.settings.active_nginx, nginx_up) {
            (Some(version), true) if wide => format!("nginx {version}"),
            (Some(_), false) if wide => "nginx stopped".to_string(),
            _ => "nginx".to_string(),
        };
        let php_label = if wide {
            format!("PHP {workers} of {desired}")
        } else {
            format!("PHP {workers}/{desired}")
        };

        let mut bar = StatusBar::new()
            .left(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .child(state_dot(nginx_up, cx))
                    .child(nginx_label),
            )
            .left(Separator::vertical())
            .left(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .child(state_dot(workers > 0, cx))
                    .child(php_label),
            );

        // Center: the selected project, ellipsised rather than clipped.
        if let Some(project) = self.selected_project() {
            let serving = self.nginx_on.contains(&project.id);
            let state = if serving { "serving" } else { "stopped" };
            let text = if wide {
                format!(
                    "{} \u{00b7} {} {}{}",
                    project.name,
                    state,
                    scheme_prefix(project.ssl_enabled),
                    project.domain
                )
            } else {
                format!("{} \u{00b7} {state}", project.name)
            };
            bar = bar.child(div().text_ellipsis().overflow_hidden().child(text));
        }

        // Right: ambient notices.
        let hosts = match self.mode {
            ControlMode::Elevated => "elevated",
            ControlMode::Task => "no-prompt",
            ControlMode::Uac => "asks",
        };
        let hosts_label = if wide {
            format!("Hosts: {hosts}")
        } else {
            hosts.to_string()
        };
        bar = bar.right(
            Button::new("status-hosts")
                .ghost()
                .xsmall()
                .icon(icon(IconName::ShieldCheck))
                .label(hosts_label)
                .tooltip("How Wisp writes the hosts file")
                .on_click(cx.listener(|this, _, _window, cx| this.goto(Screen::Settings, cx))),
        );

        if let Some(version) = self.settings.available_version.clone() {
            bar = bar.right(
                Button::new("status-update")
                    .ghost()
                    .xsmall()
                    .icon(icon(IconName::Download))
                    .label(format!("v{version}"))
                    .tooltip("An update is available")
                    .on_click(cx.listener(|this, _, _window, cx| this.goto(Screen::Settings, cx))),
            );
        }

        // Global site controls, condensed rather than hidden.
        let any_running = !self.nginx_on.is_empty() || !self.php_on.is_empty();
        let any_stopped = self
            .projects
            .iter()
            .any(|project| project.php_version.is_some() && !self.is_on(project.id));
        let busy = self.serve_busy;

        let (start, restart, stop) = if wide {
            ("Start all", "Restart all", "Stop all")
        } else {
            ("Start", "Restart", "Stop")
        };
        let labels = if compact {
            Some((start, restart, stop))
        } else {
            None
        };

        let site_button = |id: &str,
                           icon_name: IconName,
                           label: &str,
                           tip: &str,
                           enabled: bool,
                           action: fn(&mut Wisp, &mut Context<Wisp>)| {
            let mut button = Button::new(id.to_string())
                .ghost()
                .xsmall()
                .icon(icon(icon_name))
                .tooltip(tip.to_string())
                .disabled(busy || !enabled);
            if let Some((_, _, _)) = labels {
                button = button.label(label.to_string());
            }
            let action: fn(&mut Wisp, &mut Context<Wisp>) = action;
            button.on_click(cx.listener(move |this, _, _window, cx| action(this, cx)))
        };

        bar.right(site_button(
            "status-start-all",
            IconName::Play,
            labels.map_or("", |l| l.0),
            "Start every project and nginx",
            any_stopped,
            Wisp::start_all,
        ))
        .right(site_button(
            "status-restart-all",
            IconName::RotateCcw,
            labels.map_or("", |l| l.1),
            "Restart every project's worker",
            any_running,
            Wisp::restart_all,
        ))
        .right(site_button(
            "status-stop-all",
            IconName::Square,
            labels.map_or("", |l| l.2),
            "Stop every project and nginx",
            any_running,
            Wisp::stop_all,
        ))
        .into_any_element()
    }

    fn top_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .flex_wrap()
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
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Wisp"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("open-palette")
                            .ghost()
                            .small()
                            .icon(icon(IconName::Search))
                            .label("Search")
                            .tooltip("Search or run a command (Ctrl-K)")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_palette(window, cx)
                            })),
                    )
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
        let on_runtimes = self.screen == Screen::Services;
        let on_settings = self.screen == Screen::Settings;
        let filter = self.project_filter.read(cx).value().to_lowercase();

        let visible: Vec<&Project> = self
            .projects
            .iter()
            .filter(|project| {
                filter.is_empty()
                    || project.name.to_lowercase().contains(&filter)
                    || project.domain.to_lowercase().contains(&filter)
            })
            .collect();

        let mut menu = SidebarMenu::new();
        if visible.is_empty() {
            menu = menu.child(
                SidebarMenuItem::new(if self.projects.is_empty() {
                    "No projects yet"
                } else {
                    "No matches"
                })
                .icon(IconName::FolderOpen)
                .disable(true),
            );
        } else {
            for project in &visible {
                let active = selected == Some(project.id) && !on_runtimes && !on_settings;
                let serving = self.nginx_on.contains(&project.id);
                let worker = self.php_on.contains(&project.id);
                let color = if serving && worker {
                    cx.theme().success
                } else if serving || worker {
                    cx.theme().warning
                } else {
                    cx.theme().muted_foreground
                };
                menu = menu.child(project_menu_item(project, active, color, cx));
            }
        }

        let footer = SidebarFooter::new()
            .child(nav_item(
                "Runtimes",
                IconName::Server,
                on_runtimes,
                Screen::Services,
                self.runtimes.len(),
                cx,
            ))
            .child(nav_item(
                "Settings",
                IconName::Settings,
                on_settings,
                Screen::Settings,
                0,
                cx,
            ));

        let header = SidebarHeader::new().child(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .w_full()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(div().size(px(22.)).child(img(APP_ICON).size(px(22.))))
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("Wisp"),
                        ),
                )
                .child(Input::new(&self.project_filter)),
        );

        Sidebar::new("wisp-sidebar")
            .header(header)
            .child(SidebarGroup::new("Projects").child(menu))
            .footer(footer)
    }

    fn content(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        match self.screen {
            Screen::Projects => self.projects_content(window, cx),
            Screen::Services => self.services_content(cx),
            Screen::Settings => self.settings_content(cx),
        }
    }

    fn projects_content(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
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
        let url = format!("{}{}", scheme_prefix(ssl), domain);
        let config_dir = crate::config::sites().join(&slug);
        let config_display = config_dir.to_string_lossy().to_string();

        let header = div()
            .flex()
            .flex_row()
            .flex_wrap()
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
                            .child(ui::dot(cx.theme().primary))
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
                            .danger()
                            .small()
                            .icon(icon(IconName::Trash))
                            .label("Remove")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.confirm_delete(id, window, cx)
                            })),
                    ),
            );

        let details = ui::section(
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
                .child(detail_row_action(
                    "Root folder",
                    &root,
                    Button::new("open-root")
                        .ghost()
                        .small()
                        .icon(icon(IconName::FolderOpen))
                        .tooltip("Open in Explorer")
                        .on_click(cx.listener({
                            let path = root.clone();
                            move |this, _, _window, cx| this.open_folder(&path, cx)
                        })),
                    cx,
                ))
                .child(detail_row_action(
                    "Domain",
                    &url,
                    Button::new("open-url")
                        .ghost()
                        .small()
                        .icon(icon(IconName::ExternalLink))
                        .tooltip("Open in browser")
                        .on_click(cx.listener({
                            let url = url.clone();
                            move |this, _, _window, cx| this.open_domain(url.clone(), cx)
                        })),
                    cx,
                ))
                .child(detail_row("HTTPS", if ssl { "Trusted certificate" } else { "Off" }, cx))
                .child(detail_row("PHP", &php, cx))
                .child(detail_row_action(
                    "Config folder",
                    &config_display,
                    Button::new("open-config")
                        .ghost()
                        .small()
                        .icon(icon(IconName::FolderTree))
                        .tooltip("Open in Explorer")
                        .on_click(cx.listener({
                            let path = config_display.clone();
                            move |this, _, _window, cx| this.open_folder(&path, cx)
                        })),
                    cx,
                )),
        );

        let worker_up = self.serve_status.running_projects.contains(&id);
        let nginx_up = self.serve_status.nginx_running;
        let nginx_on = self.nginx_on.contains(&id);
        let php_on = self.php_on.contains(&id);
        let busy = self.serve_busy;

        let services_header = section_header(
            "Services",
            "This project's nginx server block and its PHP worker.",
            cx,
        );
        let nginx_state = if !nginx_on {
            "Not serving".to_string()
        } else if nginx_up {
            format!("Serving {url}")
        } else {
            "Starting\u{2026}".to_string()
        };
        let php_state = if !php_on {
            "Stopped".to_string()
        } else if worker_up {
            format!("php-cgi {php}")
        } else {
            "Starting\u{2026}".to_string()
        };
        let rows = div()
            .flex()
            .flex_col()
            .p(px(8.))
            .child(project_service_row(
                "nginx",
                &nginx_state,
                nginx_on,
                ProjectService::Nginx,
                id,
                busy,
                cx,
            ))
            .child(project_service_row(
                "PHP",
                &php_state,
                php_on,
                ProjectService::Php,
                id,
                busy,
                cx,
            ));
        let services = ui::section(
            cx,
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(services_header)
                .child(rows),
        );

        let logs = self.logs_card(cx);

        let tabs = TabBar::new("project-tabs")
            .segmented()
            .selected_index(self.project_tab.index())
            .children([
                Tab::new().label("Overview"),
                Tab::new().label("Services"),
                Tab::new().label("Logs"),
                Tab::new().label("Settings"),
            ])
            .on_click(cx.listener(|this, index, _window, cx| {
                let tab = ProjectTab::from_index(*index);
                this.project_tab = tab;
                if tab == ProjectTab::Logs {
                    this.refresh_logs(cx);
                }
                cx.notify();
            }));

        let body: AnyElement = match self.project_tab {
            ProjectTab::Overview => div()
                .flex()
                .flex_col()
                .gap_4()
                .child(details)
                .child(self.resources_card(cx))
                .into_any_element(),
            ProjectTab::Services => services.into_any_element(),
            ProjectTab::Logs => logs.into_any_element(),
            ProjectTab::Settings => self.project_settings(project, window, cx),
        };

        div()
            .id("content")
            .flex()
            .flex_col()
            .size_full()
            .gap_4()
            .p(px(28.))
            .overflow_y_scrollbar()
            .child(header)
            .child(tabs)
            .child(body)
            .into_any_element()
    }

    /// A live CPU/RAM readout with five-minute sparklines for the Overview tab.
    fn resources_card(&self, cx: &mut Context<Self>) -> AnyElement {
        let (cpu, ram) = self.metrics.back().copied().unwrap_or((0.0, 0.0));
        let cpu_values: Vec<f64> = self.metrics.iter().map(|(cpu, _)| *cpu).collect();
        let ram_values: Vec<f64> = self.metrics.iter().map(|(_, ram)| *ram).collect();

        let header = section_header(
            "Resources",
            "This machine, sampled every two seconds for the last five minutes.",
            cx,
        );

        let mut body = div().flex().flex_col().gap_3().p(px(18.));
        if self.metrics.is_empty() {
            body = body.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("Measuring\u{2026}"),
            );
        }
        body = body
            .child(resource_row("CPU", cpu, "cpu-spark", cpu_values, cx))
            .child(resource_row("RAM", ram, "ram-spark", ram_values, cx));

        ui::section(
            cx,
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(header)
                .child(body),
        )
        .into_any_element()
    }

    /// Show or hide the command palette, focusing its field when it appears.
    fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette_open = !self.palette_open;
        if self.palette_open {
            self.palette.update(cx, |state, cx| state.focus(window, cx));
        }
        cx.notify();
    }

    /// The Ctrl-K palette: destinations, project shortcuts and bulk actions.
    fn palette_overlay(&self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut items: Vec<CommandItem> = vec![
            CommandItem::new()
                .label("Open Runtimes")
                .icon(IconName::Server)
                .action(Box::new(GoRuntimes)),
            CommandItem::new()
                .label("Open Settings")
                .icon(IconName::Settings)
                .action(Box::new(GoSettings)),
            CommandItem::new()
                .label("New project")
                .icon(IconName::FolderPlus)
                .action(Box::new(NewProject)),
            CommandItem::new()
                .label("Start all sites")
                .icon(IconName::Play)
                .action(Box::new(tray::StartAll)),
            CommandItem::new()
                .label("Stop all sites")
                .icon(IconName::CircleStop)
                .action(Box::new(tray::StopAll)),
            CommandItem::new()
                .label("Restart all sites")
                .icon(IconName::RefreshCw)
                .action(Box::new(tray::RestartAll)),
        ];
        for project in &self.projects {
            items.push(
                CommandItem::new()
                    .label(format!("Open {}", project.name))
                    .icon(IconName::Globe)
                    .keywords([project.domain.clone()])
                    .action(Box::new(tray::SiteAction {
                        id: project.id,
                        command: tray::SiteCommand::Open,
                    })),
            );
        }

        let on_cancel = {
            let entity = cx.entity();
            move |_window: &mut Window, cx: &mut App| {
                entity.update(cx, |this, cx| {
                    this.palette_open = false;
                    cx.notify();
                });
            }
        };
        let on_confirm = {
            let entity = cx.entity();
            move |_path, _window: &mut Window, cx: &mut App| {
                entity.update(cx, |this, cx| {
                    this.palette_open = false;
                    cx.notify();
                });
            }
        };

        let command = Command::new(&self.palette)
            .placeholder("Search Wisp\u{2026}")
            .items(items)
            .on_cancel(on_cancel)
            .on_confirm(on_confirm);

        div()
            .id("palette-backdrop")
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(88.))
            .bg(hsla(0.0, 0.0, 0.0, 0.35))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _window, cx| {
                    this.palette_open = false;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .id("palette-card")
                    .w(px(560.))
                    .on_mouse_down(MouseButton::Left, |_, _, _| {})
                    .child(command),
            )
            .into_any_element()
    }

    /// The Settings tab: the project form in place, or a summary that opens it.
    fn project_settings(
        &self,
        project: &Project,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if let Some(editor) = self
            .editor
            .as_ref()
            .filter(|editor| editor.id == Some(project.id))
        {
            let entity = cx.entity();
            let error = editor.error.clone();
            let php_versions = self.installed_php_versions();
            return div()
                .flex()
                .flex_col()
                .gap_4()
                .child(editor_form(
                    editor,
                    &entity,
                    error,
                    &php_versions,
                    window,
                    cx,
                ))
                .child(inline_editor_footer(entity))
                .into_any_element();
        }

        let id = project.id;
        let php = project
            .php_version
            .clone()
            .unwrap_or_else(|| "Not set".to_string());
        let overrides = project.php_ini_overrides.clone();

        let summary = ui::section(
            cx,
            div()
                .flex()
                .flex_col()
                .p(px(18.))
                .gap_3()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child("Project settings"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child("Name, folder, domain, PHP version and HTTPS are edited together."),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .child(
                            Button::new("settings-edit")
                                .primary()
                                .small()
                                .icon(icon(IconName::SquarePen))
                                .label("Edit project")
                                .on_click({
                                    let entity = cx.entity();
                                    move |_, window, cx| {
                                        let existing = entity
                                            .read(cx)
                                            .projects
                                            .iter()
                                            .find(|p| p.id == id)
                                            .cloned();
                                        entity.update(cx, |this, cx| {
                                            this.open_editor(existing, window, cx);
                                        });
                                    }
                                }),
                        )
                        .child(
                            Button::new("settings-remove")
                                .danger()
                                .small()
                                .icon(icon(IconName::Trash))
                                .label("Remove")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.confirm_delete(id, window, cx)
                                })),
                        ),
                )
                .child(detail_row("PHP", &php, cx)),
        );

        let ini = ui::section(
            cx,
            div()
                .flex()
                .flex_col()
                .p(px(18.))
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child("php.ini overrides"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child("Appended last, so these win over the generated values."),
                )
                .child(
                    div()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_xs()
                        .text_color(if overrides.trim().is_empty() {
                            cx.theme().muted_foreground
                        } else {
                            cx.theme().foreground
                        })
                        .child(if overrides.trim().is_empty() {
                            "None yet.".to_string()
                        } else {
                            overrides
                        }),
                ),
        );

        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(summary)
            .child(ini)
            .into_any_element()
    }

    // ---- services --------------------------------------------------------

    fn services_content(&self, cx: &mut Context<Self>) -> AnyElement {
        let header = div()
            .flex()
            .flex_row()
            .flex_wrap()
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
                            .child("Services"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("Install and manage the PHP and nginx builds Wisp runs."),
                    ),
            )
            .child(
                Button::new("refresh-catalog")
                    .outline()
                    .small()
                    .icon(icon(IconName::RefreshCw))
                    .label(if self.catalog_loading {
                        "Refreshing\u{2026}"
                    } else {
                        "Refresh"
                    })
                    .disabled(self.catalog_loading)
                    .on_click(cx.listener(|this, _, _window, cx| this.load_catalog(cx))),
            );

        let mut column = div().flex().flex_col().gap_6().child(header);

        if let Some(message) = self.catalog_error.clone() {
            column = column.child(
                Alert::error("catalog-error", message)
                    .title("Could not load the version list")
                    .banner(),
            );
        }

        if let Some(vc) = self.php_vcrt_gap() {
            column = column.child(self.vcrt_banner(vc, cx));
        }

        let tabs = TabBar::new("runtime-tabs")
            .segmented()
            .selected_index(self.runtime_tab.index())
            .children([
                Tab::new().label("PHP"),
                Tab::new().label("nginx"),
                Tab::new().label("HTTPS"),
                Tab::new().label("Hosts"),
                Tab::new().label("Resources"),
            ])
            .on_click(cx.listener(|this, index, _window, cx| {
                this.runtime_tab = RuntimeTab::from_index(*index);
                cx.notify();
            }));

        let body: AnyElement = match self.runtime_tab {
            RuntimeTab::Php => self.php_section(cx).into_any_element(),
            RuntimeTab::Nginx => self.nginx_section(cx).into_any_element(),
            RuntimeTab::Https => self.https_card(cx).into_any_element(),
            RuntimeTab::Hosts => self.hosting_card(cx).into_any_element(),
            RuntimeTab::Resources => self.resources_card(cx),
        };

        div()
            .id("services")
            .flex()
            .flex_col()
            .size_full()
            .gap_4()
            .p(px(28.))
            .overflow_y_scrollbar()
            .child(column)
            .child(tabs)
            .child(body)
            .into_any_element()
    }

    /// The selected project's logs: source, search, follow and a bounded tail.
    fn logs_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.log_kind;
        let raw = self.log_filter.read(cx).value().to_string();
        let needle = if self.log_case {
            raw.clone()
        } else {
            raw.to_lowercase()
        };

        let visible: Vec<(usize, &String)> = self
            .log_lines
            .iter()
            .enumerate()
            .filter(|(_, line)| {
                if needle.is_empty() {
                    return true;
                }
                let hay = if self.log_case {
                    (*line).clone()
                } else {
                    line.to_lowercase()
                };
                hay.contains(&needle)
            })
            .collect();
        let shown = visible.len();
        let total = self.log_lines.len();
        let matching = !needle.is_empty();

        let sources: Vec<AnyElement> = [
            LogKind::Access,
            LogKind::Errors,
            LogKind::Php,
            LogKind::Worker,
        ]
        .into_iter()
        .map(|kind| {
            let button = Button::new(format!("log-{}", kind.label()))
                .small()
                .label(kind.label());
            let button = if kind == current {
                button.primary()
            } else {
                button.outline()
            };
            button
                .on_click(cx.listener(move |this, _, _window, cx| {
                    if this.log_kind != kind {
                        this.log_kind = kind;
                        this.log_lines.clear();
                        this.log_match = 0;
                        this.refresh_logs(cx);
                    }
                    cx.notify();
                }))
                .into_any_element()
        })
        .collect();

        let count = if matching {
            format!("{} of {} lines", self.log_match.saturating_add(1).min(shown), shown)
        } else {
            format!("{total} lines")
        };

        let header = section_header("Logs", "The tail of this project's logs.", cx);
        let toolbar = Toolbar::new("logs-toolbar")
            .contents(sources)
            .content(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(220.)).child(Input::new(&self.log_filter)))
                    .child({
                        let button = Button::new("log-case")
                            .small()
                            .label("Aa")
                            .tooltip("Match case");
                        let button = if self.log_case {
                            button.primary()
                        } else {
                            button.outline()
                        };
                        button.on_click(cx.listener(|this, _, _window, cx| {
                            this.log_case = !this.log_case;
                            cx.notify();
                        }))
                    })
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child(count.clone()))
                    .child(
                        Button::new("log-prev")
                            .ghost()
                            .xsmall()
                            .icon(icon(IconName::ChevronRight))
                            .tooltip("Previous match")
                            .disabled(!matching)
                            .on_click(cx.listener(|this, _, _window, cx| this.step_log_match(false, cx))),
                    )
                    .child(
                        Button::new("log-next")
                            .ghost()
                            .xsmall()
                            .icon(icon(IconName::ChevronRight))
                            .tooltip("Next match")
                            .disabled(!matching)
                            .on_click(cx.listener(|this, _, _window, cx| this.step_log_match(true, cx))),
                    )
                    .child({
                        let button = Button::new("log-follow")
                            .small()
                            .label("Follow")
                            .tooltip("Keep the newest line in view");
                        let button = if self.log_follow {
                            button.primary()
                        } else {
                            button.outline()
                        };
                        button.on_click(cx.listener(|this, _, _window, cx| {
                            this.log_follow = !this.log_follow;
                            if this.log_follow {
                                let max = this.log_scroll.max_offset();
                                this.log_scroll.set_offset(point(px(0.), max.y));
                            }
                            cx.notify();
                        }))
                    })
                    .child(
                        Button::new("log-folder")
                            .ghost()
                            .small()
                            .icon(icon(IconName::FolderOpen))
                            .tooltip("Open the log folder")
                            .on_click(cx.listener(|this, _, _window, cx| this.open_log_folder(cx))),
                    )
                    .child(
                        Button::new("log-clear")
                            .ghost()
                            .small()
                            .label("Clear")
                            .tooltip("Empty this log file")
                            .on_click(cx.listener(|this, _, _window, cx| this.clear_log(cx))),
                    ),
            );

        let body = if visible.is_empty() {
            div()
                .px(px(18.))
                .pb(px(16.))
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(if self.log_lines.is_empty() {
                    "Nothing logged yet. Start the project and open its domain."
                } else {
                    "No lines match that search."
                })
                .into_any_element()
        } else {
            let lines: Vec<AnyElement> = visible
                .iter()
                .map(|(_, line)| {
                    div()
                        .whitespace_nowrap()
                        .child((*line).clone())
                        .into_any_element()
                })
                .collect();

            div()
                .relative()
                .w_full()
                .h(px(240.))
                .child(
                    div()
                        .id("log-lines")
                        .track_scroll(&self.log_scroll)
                        .overflow_y_scroll()
                        .size_full()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_xs()
                        .children(lines),
                )
                .vertical_scrollbar(&self.log_scroll)
                .into_any_element()
        };

        ui::section(
            cx,
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(header)
                .child(div().px(px(18.)).pb(px(6.)).child(toolbar))
                .child(body),
        )
    }

    /// The state of the hosts-file elevated control.
    fn hosting_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mode = self.mode;
        let busy = self.elevation_busy;

        let badge_label = match mode {
            ControlMode::Elevated => "Elevated",
            ControlMode::Task => "No-prompt",
            ControlMode::Uac => "Asks once",
        };
        let state = match mode {
            ControlMode::Elevated => "Wisp is running elevated, so hosts changes never prompt.",
            ControlMode::Task => "Set up once \u{2014} hosts changes never prompt.",
            ControlMode::Uac => {
                "Wisp prompts once on first use, then sets up no-prompt control."
            }
        };
        let action: AnyElement = match mode {
            ControlMode::Elevated => div().into_any_element(),
            ControlMode::Task => Button::new("hosting-remove")
                .outline()
                .small()
                .label("Remove")
                .disabled(busy)
                .on_click(cx.listener(|this, _, _window, cx| this.remove_elevated(cx)))
                .into_any_element(),
            ControlMode::Uac => Button::new("hosting-setup")
                .outline()
                .small()
                .label("Set up now")
                .disabled(busy)
                .on_click(cx.listener(|this, _, _window, cx| this.setup_elevated(cx)))
                .into_any_element(),
        };
        let badge_label = SharedString::from(badge_label);
        let state = SharedString::from(state);

        ui::section(
            cx,
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_4()
                .p(px(18.))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_3()
                        .child(icon(IconName::ShieldCheck).text_color(cx.theme().primary))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .text_sm()
                                                .font_weight(FontWeight::MEDIUM)
                                                .child("Hosts file"),
                                        )
                                        .child(badge(&badge_label, cx)),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(state.clone()),
                                ),
                        ),
                )
                .child(action),
        )
    }
    fn https_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let trusted = self.settings.ca_trusted;

        let action: AnyElement = if self.trust_busy {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(Spinner::new().small())
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("Trusting\u{2026}"),
                )
                .into_any_element()
        } else if trusted {
            badge("Trusted", cx).into_any_element()
        } else {
            Button::new("trust-ca")
                .primary()
                .small()
                .label("Trust certificate")
                .on_click(cx.listener(|this, _, _window, cx| this.trust_ca(cx)))
                .into_any_element()
        };

        let subtitle = if trusted {
            "Wisp's certificate is trusted on this account."
        } else {
            "Trust Wisp's certificate so https:// works in your browser."
        };

        ui::section(
            cx,
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_4()
                .p(px(18.))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_3()
                        .child(icon(IconName::ShieldCheck).text_color(cx.theme().primary))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child("Local HTTPS"),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(subtitle),
                                ),
                        ),
                )
                .child(action),
        )
    }

    fn vcrt_banner(&self, vc: u16, cx: &mut Context<Self>) -> impl IntoElement {        let family = if vc >= 17 {
            "Visual C++ 2015\u{2013}2022 x64"
        } else {
            "Visual C++ 2015\u{2013}2019 x64"
        };
        ui::section(
            cx,
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_4()
                .p(px(18.))
                .child(
                    icon(IconName::TriangleAlert)
                        .text_color(cx.theme().primary),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .gap_1()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .child(format!("The {family} runtime is required")),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!(
                                    "PHP will not start without it. {}",
                                    vcrt::description()
                                )),
                        ),
                )
                .child(
                    Button::new("install-vcrt")
                        .primary()
                        .small()
                        .label("Install runtime")
                        .on_click(cx.listener(|this, _, _window, cx| this.install_vcrt(cx))),
                ),
        )
    }

    fn php_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let installed = self.installed(RuntimeKind::Php);

        // One row per minor line, using the newest patch available in it.
        let mut lines: Vec<(String, Release)> = Vec::new();
        if let Some(catalog) = &self.catalog {
            for release in &catalog.php {
                let line = db::minor_line(&release.version);
                if lines.iter().any(|(existing, _)| *existing == line) {
                    continue;
                }
                lines.push((line, release.clone()));
            }
        }

        let rows: Vec<AnyElement> = lines
            .iter()
            .map(|(line, latest)| {
                let current = installed
                    .iter()
                    .find(|runtime| runtime.line() == *line)
                    .cloned();
                php_row(line, current.as_ref(), latest, self.busy_runtime.as_deref(), cx)
            })
            .collect();

        let body = if rows.is_empty() {
            div()
                .p(px(18.))
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(if self.catalog_loading {
                    "Loading versions\u{2026}"
                } else {
                    "No PHP versions were found."
                })
                .into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .p(px(8.))
                .children(rows)
                .into_any_element()
        };

        ui::section(
            cx,
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(section_header("PHP", "Choose a version per project. Updates replace the old patch.", cx))
                .child(body),
        )
    }

    fn nginx_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let installed = self.installed(RuntimeKind::Nginx);
        let active = self.settings.active_nginx.clone();
        let busy = self.busy_runtime.as_deref();

        let rows: Vec<AnyElement> = installed
            .iter()
            .map(|runtime| {
                let is_active = active.as_deref() == Some(runtime.version.as_str());
                nginx_row(runtime, is_active, busy, cx)
            })
            .collect();

        let body = if rows.is_empty() {
            div()
                .p(px(18.))
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("No nginx installed yet.")
                .into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .p(px(8.))
                .children(rows)
                .into_any_element()
        };

        // Versions offered for install: available, not already installed.
        let installable: Vec<Release> = self
            .catalog
            .as_ref()
            .map(|catalog| {
                catalog
                    .nginx
                    .iter()
                    .filter(|release| {
                        !installed
                            .iter()
                            .any(|runtime| runtime.version == release.version)
                    })
                    .take(12)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();

        let install_menu = if installable.is_empty() {
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("Up to date with the catalog.")
                .into_any_element()
        } else {
            Button::new("install-nginx")
                .outline()
                .small()
                .icon(icon(IconName::Download))
                .label("Install a version")
                .dropdown_menu_with_anchor(Anchor::BottomRight, {
                    let entity = cx.entity();
                    move |menu, _, _| {
                        let mut menu = menu
                            .check_side(Side::Right)
                            .item(PopupMenuItem::new("nginx Version").disabled(true))
                            .separator();
                        for release in &installable {
                            let release = release.clone();
                            let entity = entity.clone();
                            menu = menu.item(
                                PopupMenuItem::new(release.version.clone()).on_click(move |_, _, cx| {
                                    entity.update(cx, |this, cx| {
                                        this.install_release(RuntimeKind::Nginx, release.clone(), cx)
                                    });
                                }),
                            );
                        }
                        menu
                    }
                })
                .into_any_element()
        };

        ui::section(
            cx,
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(section_header(
                    "nginx",
                    "One active version serves every project.",
                    cx,
                ))
                .child(body)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_end()
                        .px(px(18.))
                        .pb(px(14.))
                        .child(install_menu),
                ),
        )
    }
}

impl Render for Wisp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        flush_toasts(&mut self.toasts, window, cx);
        self.apply_pending_root(window, cx);
        self.publish_tray(cx);

        let mut root = div()
            .id("wisp-root")
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(|this, _: &TogglePalette, window, cx| {
                this.toggle_palette(window, cx);
            }))
            .on_action(cx.listener(|this, _: &GoRuntimes, _window, cx| {
                this.goto(Screen::Services, cx);
            }))
            .on_action(cx.listener(|this, _: &GoSettings, _window, cx| {
                this.goto(Screen::Settings, cx);
            }))
            .on_action(cx.listener(|this, _: &NewProject, window, cx| {
                this.open_editor(None, window, cx);
            }))
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

        if let Some(message) = self.serve_error.clone() {
            root = root.child(
                div().px(px(20.)).pb(px(12.)).child(
                    Alert::error("serve-error", message)
                        .title("Could not start the server")
                        .banner(),
                ),
            );
        }

        if let Some(banner) = self.update_banner(cx) {
            root = root.child(banner);
        }

        let page = root
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_hidden()
                    .child(self.sidebar(cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .h_full()
                            .overflow_hidden()
                            .child(self.content(window, cx)),
                    ),
            )
            .child(self.status_bar(window, cx));

        if self.palette_open {
            page.child(self.palette_overlay(window, cx))
        } else {
            page
        }
    }
}

/// The shortest name for a theme preference, as stored in settings.
fn theme_name(pref: ThemePreference) -> &'static str {
    match pref {
        ThemePreference::System => "system",
        ThemePreference::Light => "light",
        ThemePreference::Dark => "dark",
    }
}

fn theme_from_name(name: &str) -> ThemePreference {
    match name {
        "light" => ThemePreference::Light,
        "dark" => ThemePreference::Dark,
        _ => ThemePreference::System,
    }
}

/// Appearance and startup.
fn general_settings_page(entity: &Entity<Wisp>) -> SettingPage {
    let theme_read = entity.clone();
    let theme_write = entity.clone();
    let background_read = entity.clone();
    let background_write = entity.clone();
    let autostart_read = entity.clone();
    let autostart_write = entity.clone();

    SettingPage::new("General")
        .icon(icon(IconName::Settings))
        .description("Appearance and how Wisp starts.")
        .group(
            SettingGroup::new()
                .title("Appearance")
                .item(
                    SettingItem::new(
                        "Theme",
                        SettingField::dropdown(
                            vec![
                                (SharedString::from("system"), SharedString::from("System")),
                                (SharedString::from("light"), SharedString::from("Light")),
                                (SharedString::from("dark"), SharedString::from("Dark")),
                            ],
                            move |cx: &App| {
                                SharedString::from(theme_name(theme_read.read(cx).settings.theme))
                            },
                            move |value: SharedString, cx: &mut App| {
                                let pref = theme_from_name(value.as_ref());
                                theme_write.update(cx, |this, cx| this.set_theme_pref(pref, cx));
                            },
                        ),
                    )
                    .description("System follows Windows."),
                )
                .item(
                    SettingItem::new(
                        "Run in background",
                        SettingField::switch(
                            move |cx: &App| background_read.read(cx).settings.background,
                            move |value: bool, cx: &mut App| {
                                background_write.update(cx, |this, cx| this.set_background(value, cx));
                            },
                        ),
                    )
                    .description("Closing the window keeps Wisp in the tray."),
                ),
        )
        .group(
            SettingGroup::new()
                .title("Startup")
                .item(
                    SettingItem::new(
                        "Start with Windows",
                        SettingField::switch(
                            move |cx: &App| autostart_read.read(cx).autostart,
                            move |value: bool, cx: &mut App| {
                                autostart_write.update(cx, |this, cx| this.set_autostart(value, cx));
                            },
                        ),
                    )
                    .description("Opens Wisp when you sign in."),
                ),
        )
}

/// The hosts-file elevated control.
fn hosting_settings_page(entity: &Entity<Wisp>, wisp: &Wisp) -> SettingPage {
    let mode = wisp.mode;
    let read = entity.clone();
    let write = entity.clone();
    let folder = entity.clone();

    SettingPage::new("Hosting")
        .icon(icon(IconName::ShieldCheck))
        .description("How Wisp writes the Windows hosts file for local domains.")
        .group(
            SettingGroup::new()
                .title("Hosts file")
                .item(
                    SettingItem::new(
                        "No-prompt control",
                        SettingField::switch(
                            move |cx: &App| read.read(cx).mode == ControlMode::Task,
                            move |value: bool, cx: &mut App| {
                                write.update(cx, |this, cx| this.set_hosting_control(value, cx));
                            },
                        ),
                    )
                    .description(match mode {
                        ControlMode::Elevated => "Wisp is running elevated, so nothing prompts.",
                        ControlMode::Task => "Set up once \u{2014} hosts changes never prompt.",
                        ControlMode::Uac => "Wisp asks once, then sets this up automatically.",
                    })
                    .disabled(mode == ControlMode::Elevated),
                )
                .item(SettingItem::render({
                    let entity = folder.clone();
                    move |_options, _window, _cx| {
                        Button::new("settings-open-config")
                            .outline()
                            .small()
                            .icon(icon(IconName::FolderOpen))
                            .label("Open config folder")
                            .on_click({
                                let entity = entity.clone();
                                move |_, _, cx| {
                                    entity.update(cx, |this, cx| {
                                        let path = crate::config::root().to_string_lossy().to_string();
                                        this.open_folder(&path, cx);
                                    });
                                }
                            })
                    }
                })),
        )
}

/// Version and updates.
fn updates_settings_page(entity: &Entity<Wisp>, wisp: &Wisp) -> SettingPage {
    let version = env!("CARGO_PKG_VERSION").to_string();
    let available = wisp.settings.available_version.clone();
    let check = entity.clone();
    let install = entity.clone();

    let mut items = vec![
        SettingItem::render(move |_options, _window, _cx| {
            div().text_sm().child(format!("Wisp v{version}"))
        }),
        SettingItem::render(move |_options, _window, _cx| {
            Button::new("settings-check-updates")
                .outline()
                .small()
                .icon(icon(IconName::RefreshCw))
                .label("Check for updates")
                .on_click({
                    let entity = check.clone();
                    move |_, _, cx| {
                        entity.update(cx, |this, cx| this.check_updates(cx, true));
                    }
                })
        }),
    ];
    if let Some(version) = available {
        items.push(SettingItem::render({
            let entity = install.clone();
            move |_options, _window, _cx| {
                Button::new("settings-install-update")
                    .primary()
                    .small()
                    .icon(icon(IconName::Download))
                    .label(format!("Install v{version}"))
                    .on_click({
                        let entity = entity.clone();
                        move |_, _, cx| {
                            entity.update(cx, |this, cx| this.install_update(cx));
                        }
                    })
            }
        }));
    }

    SettingPage::new("Updates")
        .icon(icon(IconName::Download))
        .description("Wisp updates itself from GitHub Releases.")
        .group(SettingGroup::new().title("Version").items(items))
}

/// Version, source and license.
fn about_settings_page() -> SettingPage {
    SettingPage::new("About")
        .icon(icon(IconName::Info))
        .description(env!("CARGO_PKG_DESCRIPTION"))
        .group(
            SettingGroup::new().title("Wisp").item(SettingItem::render(
                |_options, _window, cx| {
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_sm()
                                .child(format!("Version {}", env!("CARGO_PKG_VERSION"))),
                        )
                        .child(
                            Link::new("about-repository")
                                .href(REPOSITORY_URL)
                                .text_sm()
                                .child(repository_label()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!("{} licensed", env!("CARGO_PKG_LICENSE"))),
                        )
                },
            )),
        )
}

/// One project row in the sidebar: name, a status dot, and selection.
fn project_menu_item(
    project: &Project,
    active: bool,
    color: Hsla,
    cx: &mut Context<Wisp>,
) -> SidebarMenuItem {
    let id = project.id;

    SidebarMenuItem::new(project.name.clone())
        .icon(IconName::Globe)
        .active(active)
        .suffix(move |_window, _cx| ui::dot(color))
        .on_click(cx.listener(move |this, _, _window, cx| this.select(id, cx)))
}

/// A top-level sidebar destination.
fn nav_item(
    label: &'static str,
    icon_name: IconName,
    active: bool,
    screen: Screen,
    count: usize,
    cx: &mut Context<Wisp>,
) -> impl IntoElement {
    let base = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .px(px(12.))
        .py(px(9.))
        .rounded_lg();

    let styled = if active {
        base.bg(cx.theme().accent)
            .border_1()
            .border_color(cx.theme().accent)
    } else {
        base
    };

    styled
        .id(label)
        .hover(|d| d.bg(cx.theme().list_hover))
        .on_click(cx.listener(move |this, _, _window, cx| this.goto(screen, cx)))
        .child(icon(icon_name).small().text_color(if active {
            cx.theme().primary
        } else {
            cx.theme().muted_foreground
        }))
        .child(
            div()
                .flex_1()
                .text_sm()
                .font_weight(if active {
                    FontWeight::MEDIUM
                } else {
                    FontWeight::NORMAL
                })
                .child(label),
        )
        .child(if count > 0 {
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(count.to_string())
                .into_any_element()
        } else {
            div().into_any_element()
        })
}

/// A section title and its one-line description inside a plate.
fn section_header(title: &str, subtitle: &str, cx: &App) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .px(px(18.))
        .pt(px(16.))
        .pb(px(10.))
        .child(
            div()
                .text_base()
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.to_string()),
        )
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(subtitle.to_string()),
        )
}

/// A small accent badge, e.g. "Up to date" or "Active".
fn badge(label: &str, cx: &App) -> impl IntoElement {
    let color = cx.theme().primary;
    let _ = cx;
    div()
        .px(px(8.))
        .py(px(3.))
        .rounded_full()
        .bg(cx.theme().accent)
        .border_1()
        .border_color(cx.theme().accent)
        .text_xs()
        .text_color(color)
        .child(label.to_string())
}

/// One PHP minor line, with its install/update/uninstall controls.
fn php_row(
    line: &str,
    current: Option<&InstalledRuntime>,
    latest: &Release,
    busy: Option<&str>,
    cx: &mut Context<Wisp>,
) -> AnyElement {
    let latest = latest.clone();
    let busy_here = busy.is_some_and(|key| {
        key == format!("php/{}", latest.version)
            || current.is_some_and(|runtime| key == format!("php/{}", runtime.version))
    });

    let status = match current {
        None => "Not installed".to_string(),
        Some(runtime) if runtime.version == latest.version => {
            format!("Installed {}", runtime.version)
        }
        Some(runtime) => format!(
            "{} installed \u{00b7} {} available",
            runtime.version, latest.version
        ),
    };

    let actions: Vec<AnyElement> = if busy_here {
        vec![div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(Spinner::new().small())
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("Working\u{2026}"),
            )
            .into_any_element()]
    } else {
        match current {
            None => {
                let release = latest.clone();
                vec![Button::new(format!("php-install-{line}"))
                    .small()
                    .primary()
                    .label(format!("Install {}", release.version))
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.install_release(RuntimeKind::Php, release.clone(), cx)
                    }))
                    .into_any_element()]
            }
            Some(runtime) if runtime.version == latest.version => {
                let version = runtime.version.clone();
                vec![
                    badge("Up to date", cx).into_any_element(),
                    Button::new(format!("php-remove-{line}"))
                        .small()
                        .danger()
                        .label("Uninstall")
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.uninstall_runtime(RuntimeKind::Php, version.clone(), cx)
                        }))
                        .into_any_element(),
                ]
            }
            Some(runtime) => {
                let current_version = runtime.version.clone();
                let release = latest.clone();
                let remove_version = runtime.version.clone();
                vec![
                    Button::new(format!("php-update-{line}"))
                        .small()
                        .primary()
                        .label(format!("Update to {}", release.version))
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.update_php(current_version.clone(), release.clone(), cx)
                        }))
                        .into_any_element(),
                    Button::new(format!("php-remove2-{line}"))
                        .small()
                        .danger()
                        .label("Uninstall")
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.uninstall_runtime(RuntimeKind::Php, remove_version.clone(), cx)
                        }))
                        .into_any_element(),
                ]
            }
        }
    };

    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_4()
        .px(px(12.))
        .py(px(10.))
        .rounded_lg()
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child(format!("PHP {line}")),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(status),
                ),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .children(actions),
        )
        .into_any_element()
}

/// One installed nginx version, with active selection and uninstall.
fn nginx_row(
    runtime: &InstalledRuntime,
    active: bool,
    busy: Option<&str>,
    cx: &mut Context<Wisp>,
) -> AnyElement {
    let version = runtime.version.clone();
    let busy_here = busy.is_some_and(|key| key == format!("nginx/{version}"));

    let actions: Vec<AnyElement> = if busy_here {
        vec![div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(Spinner::new().small())
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("Working\u{2026}"),
            )
            .into_any_element()]
    } else {
        let use_version = version.clone();
        let remove_version = version.clone();
        let active_or_use = if active {
            badge("Active", cx).into_any_element()
        } else {
            Button::new(format!("nginx-use-{version}"))
                .small()
                .outline()
                .label("Use")
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.set_active_nginx(use_version.clone(), cx)
                }))
                .into_any_element()
        };
        vec![
            active_or_use,
            Button::new(format!("nginx-remove-{version}"))
                .small()
                .danger()
                .label("Uninstall")
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.uninstall_runtime(RuntimeKind::Nginx, remove_version.clone(), cx)
                }))
                .into_any_element(),
        ]
    };

    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_4()
        .px(px(12.))
        .py(px(10.))
        .rounded_lg()
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .child(format!("nginx {version}")),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .children(actions),
        )
        .into_any_element()
}

fn version_key(version: &str) -> Vec<u32> {
    version
        .split('.')
        .filter_map(|part| part.parse::<u32>().ok())
        .collect()
}

/// Which of a project's two services a row (or its buttons) refers to.
#[derive(Clone, Copy)]
enum ProjectService {
    Nginx,
    Php,
}

/// A project service row: status dot, name, status, and Start/Stop/Restart.
fn project_service_row(
    label: &str,
    status: &str,
    on: bool,
    service: ProjectService,
    id: i64,
    busy: bool,
    cx: &mut Context<Wisp>,
) -> impl IntoElement {
    let dot_color = if on {
        cx.theme().success
    } else {
        cx.theme().muted_foreground
    };

    let mut actions: Vec<AnyElement> = Vec::new();
    if on {
        actions.push(
            Button::new(format!("{label}-stop-{id}"))
                .danger()
                .small()
                .icon(icon(IconName::Square))
                .label("Stop")
                .disabled(busy)
                .on_click(cx.listener(move |this, _, _window, cx| match service {
                    ProjectService::Nginx => this.toggle_nginx(id, cx),
                    ProjectService::Php => this.toggle_php(id, cx),
                }))
                .into_any_element(),
        );
        actions.push(
            Button::new(format!("{label}-restart-{id}"))
                .outline()
                .small()
                .icon(icon(IconName::RotateCcw))
                .label("Restart")
                .disabled(busy)
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.restart_service(id, matches!(service, ProjectService::Php), cx)
                }))
                .into_any_element(),
        );
    } else {
        actions.push(
            Button::new(format!("{label}-start-{id}"))
                .primary()
                .small()
                .icon(icon(IconName::Play))
                .label("Start")
                .disabled(busy)
                .on_click(cx.listener(move |this, _, _window, cx| match service {
                    ProjectService::Nginx => this.toggle_nginx(id, cx),
                    ProjectService::Php => this.toggle_php(id, cx),
                }))
                .into_any_element(),
        );
    }

    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap_3()
        .px(px(12.))
        .py(px(10.))
        .rounded_lg()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .child(ui::dot(dot_color))
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child(label.to_string()),
                ),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(status.to_string()),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .children(actions),
                ),
        )
}

/// A `label  value` row with a trailing action (e.g. open in Explorer).
fn detail_row_action(
    label: &str,
    value: &str,
    action: impl IntoElement,
    cx: &App,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
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
        .child(action)
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

/// A metric row: name, current value, and a sparkline of its recent history.
fn resource_row(
    label: &str,
    value: f64,
    id: &'static str,
    values: Vec<f64>,
    cx: &App,
) -> impl IntoElement {
    let color = cx.theme().primary;
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_4()
        .child(
            div()
                .w(px(48.))
                .flex_shrink(0.0)
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .child(label.to_string()),
        )
        .child(
            div()
                .w(px(64.))
                .flex_shrink(0.0)
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(format!("{value:.1}%")),
        )
        .child(
            div().flex_1().h(px(40.)).child(
                AreaChart::new(values.into_iter().enumerate())
                    .id(id)
                    .x(|(index, _)| index.to_string())
                    .y(|(_, value)| *value)
                    .stroke(color)
                    .x_axis(false)
                    .y_axis(false)
                    .grid(false)
                    .y_domain(0.0, 100.0),
            ),
        )
}

/// The body of the create/edit dialog.
fn editor_form(
    editor: &EditorState,
    entity: &Entity<Wisp>,
    error: Option<String>,
    php_versions: &[String],
    window: &mut Window,
    cx: &App,
) -> impl IntoElement {
    // Keep the dialog inside the window; the form scrolls when it is taller.
    let cap = window.bounds().size.height - px(200.);
    let cap = if cap < px(220.) { px(220.) } else { cap };

    let php_label = editor
        .php_version
        .clone()
        .map(|version| format!("PHP {version}"))
        .unwrap_or_else(|| "Choose a version".to_string());

    let php_menu: AnyElement = if php_versions.is_empty() {
        div()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child("Install a PHP version on the Services page first.")
            .into_any_element()
    } else {
        let versions: Vec<String> = php_versions.to_vec();
        let selected = editor.php_version.clone();
        Button::new("php-version")
            .outline()
            .label(php_label)
            .dropdown_menu_with_anchor(Anchor::BottomLeft, {
                let entity = entity.clone();
                move |menu, _, _| {
                    let entity = entity.clone();
                    let mut menu = menu
                        .check_side(Side::Right)
                        .item(PopupMenuItem::new("PHP Version").disabled(true))
                        .separator();
                    for version in &versions {
                        let version = version.clone();
                        let is_current = selected.as_deref() == Some(version.as_str());
                        menu = menu.item(
                            PopupMenuItem::new(version.clone())
                                .checked(is_current)
                                .on_click({
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
            })
            .into_any_element()
    };

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
                .description("Public/ is served when present.")
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(div().flex_1().child(Input::new(&editor.root_path)))
                        .child({
                            let entity = entity.clone();
                            Button::new("browse-root")
                                .outline()
                                .small()
                                .icon(icon(IconName::FolderOpen))
                                .label("Browse")
                                .on_click(move |_, window, cx| {
                                    entity.update(cx, |this, cx| this.browse_root(window, cx));
                                })
                                .into_any_element()
                        }),
                ),
        )
        .child(
            Field::new()
                .label("Domain")
                .description("HTTPS uses a locally-trusted certificate.")
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(div().flex_1().child(Input::new(&editor.domain)))
                        .child({
                            let entity = entity.clone();
                            let current = editor.domain_suffix.clone();
                            Button::new("domain-suffix")
                                .outline()
                                .label(current.clone())
                                .dropdown_menu_with_anchor(Anchor::BottomRight, move |menu, _, _| {
                                    let entity = entity.clone();
                                    let mut menu = menu
                                        .check_side(Side::Right)
                                        .item(PopupMenuItem::new("Suffix").disabled(true))
                                        .separator();
                                    for suffix in db::DOMAIN_SUFFIXES {
                                        let suffix = (*suffix).to_string();
                                        let is_current = suffix == current;
                                        menu = menu.item(
                                            PopupMenuItem::new(suffix.clone())
                                                .checked(is_current)
                                                .on_click({
                                                    let entity = entity.clone();
                                                    move |_, _, cx| {
                                                        let suffix = suffix.clone();
                                                        entity.update(cx, |this, cx| {
                                                            if let Some(editor) =
                                                                this.editor.as_mut()
                                                            {
                                                                editor.domain_suffix =
                                                                    suffix.clone();
                                                            }
                                                            cx.notify();
                                                        });
                                                    }
                                                }),
                                        );
                                    }
                                    menu
                                })
                                .into_any_element()
                        }),
                ),
        )
        .child(Field::new().label("PHP Version").child(php_menu))
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
        )
        .child(
            Field::new()
                .label("php.ini overrides")
                .description("One directive per line, applied last.")
                .child(Textarea::new(&editor.overrides).h(px(96.))),
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

    div()
        .id("editor-scroll")
        .max_h(cap)
        .overflow_y_scrollbar()
        .child(form)
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

/// The Save/Cancel row when the project form renders inline on Settings.
fn inline_editor_footer(entity: Entity<Wisp>) -> impl IntoElement {
    let cancel = Button::new("editor-cancel-inline")
        .label("Cancel")
        .outline()
        .on_click({
            let entity = entity.clone();
            move |_, _, cx| {
                entity.update(cx, |this, cx| this.cancel_editor(cx));
            }
        });

    let save = Button::new("editor-save-inline")
        .label("Save changes")
        .primary()
        .on_click(move |_, _, cx| {
            entity.update(cx, |this, cx| {
                this.save_editor(cx);
            });
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

/// Whether an edit changes how a project is served, as opposed to a rename.
fn service_config_changed(before: &Project, after: &Project) -> bool {
    before.app_name != after.app_name
        || before.root_path != after.root_path
        || before.domain != after.domain
        || before.php_version != after.php_version
        || before.ssl_enabled != after.ssl_enabled
}

/// Whether a domain value counts as hand-edited rather than still auto-filled.
///
/// The auto-fill writes the app-name slug, so anything else (once non-empty) is
/// the user's own choice and stops the mirroring.
fn domain_is_hand_edited(typed: &str, expected: &str) -> bool {
    !typed.trim().is_empty() && typed != expected
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
    crate::ui::theme::apply(cx);
}

#[cfg(test)]
mod tests {
    use super::{domain_is_hand_edited, is_newer, service_config_changed, should_offer_update, REPOSITORY_URL};
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

    #[test]
    fn the_auto_filled_domain_does_not_count_as_hand_edited() {
        // Matching the slug, or still empty, keeps the mirroring on.
        assert!(!domain_is_hand_edited("my-app", "my-app"));
        assert!(!domain_is_hand_edited("", "my-app"));
    }

    #[test]
    fn a_typed_domain_counts_as_hand_edited() {
        assert!(domain_is_hand_edited("shop", "my-app"));
        assert!(domain_is_hand_edited("my-apps", "my-app"));
    }

    fn project(name: &str) -> crate::db::Project {
        crate::db::Project {
            id: 1,
            name: name.to_string(),
            app_name: "my-app".to_string(),
            root_path: r"C:\Sites\my-app".to_string(),
            domain: "my-app.test".to_string(),
            php_version: Some("8.3.35".to_string()),
            ssl_enabled: true,
            php_ini_overrides: "{}".to_string(),
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn a_rename_does_not_change_the_serving_config() {
        let before = project("My App");
        let after = project("Renamed");
        assert!(!service_config_changed(&before, &after));
    }

    #[test]
    fn changing_the_php_version_or_domain_needs_a_resync() {
        let before = project("My App");

        let mut after = before.clone();
        after.php_version = Some("8.4.26".to_string());
        assert!(service_config_changed(&before, &after));

        let mut after = before.clone();
        after.domain = "shop.test".to_string();
        assert!(service_config_changed(&before, &after));

        let mut after = before.clone();
        after.ssl_enabled = false;
        assert!(service_config_changed(&before, &after));

        let mut after = before.clone();
        after.root_path = r"C:\Sites\other".to_string();
        assert!(service_config_changed(&before, &after));
    }
}
