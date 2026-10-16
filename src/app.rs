//! The Wisp window.
//!
//! Layout: a slim sidebar of projects beside a detail pane. Every card is the
//! shared "tray and plate" primitive, the brand accent marks the active project
//! and the primary action, and nothing else carries colour.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};
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
use gpui_kit::component::skeleton::Skeleton;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::setting::{
    SettingField, SettingGroup, SettingItem, SettingPage, Settings as SettingsView,
};
use gpui_kit::component::separator::Separator;
use gpui_kit::component::progress::Progress;
use gpui_kit::component::sidebar::{
    Sidebar, SidebarCollapsible, SidebarGroup, SidebarHeader, SidebarItem as _, SidebarMenu,
    SidebarMenuItem, SidebarToggleButton,
};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::tab::{Tab, TabBar};
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
    /// Starting a site is the common case, so Services leads.
    Services,
    Overview,
    Logs,
    Settings,
}

impl ProjectTab {
    fn index(self) -> usize {
        match self {
            ProjectTab::Services => 0,
            ProjectTab::Overview => 1,
            ProjectTab::Logs => 2,
            ProjectTab::Settings => 3,
        }
    }

    fn from_index(index: usize) -> Self {
        match index {
            1 => ProjectTab::Overview,
            2 => ProjectTab::Logs,
            3 => ProjectTab::Settings,
            _ => ProjectTab::Services,
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

/// The processes Wisp started, published by the status poll.
///
/// The metrics sampler runs on the background executor, so it cannot reach into
/// the supervisor; it reads this instead. PIDs change on every worker restart,
/// which is why this is republished rather than cached.
#[derive(Clone, Default)]
pub struct TrackedProcesses(pub Arc<Mutex<Vec<serve::Tracked>>>);

impl Global for TrackedProcesses {}

/// One service's latest resource use, with the history behind its sparkline.
struct ServiceSample {
    /// The project this belongs to, or `None` for the shared nginx.
    project: Option<i64>,
    cpu: f64,
    ram_bytes: u64,
    cpu_history: VecDeque<f64>,
    ram_history: VecDeque<f64>,
}

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
    id: Option<i64>,    name: Entity<InputState>,
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
    /// The project's chosen icon, or `None` for the generated one.
    icon: Option<String>,
    /// A file picked from disk this session, copied into place on save.
    icon_source: Option<std::path::PathBuf>,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

/// Which pane of the php.ini editor is showing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum IniView {
    /// The overrides the user owns, which are applied last.
    Overrides,
    /// The whole file Wisp would write, read-only.
    Generated,
    /// What the overrides change.
    Diff,
}

impl IniView {
    fn index(self) -> usize {
        match self {
            IniView::Overrides => 0,
            IniView::Generated => 1,
            IniView::Diff => 2,
        }
    }

    fn from_index(index: usize) -> Self {
        match index {
            1 => IniView::Generated,
            2 => IniView::Diff,
            _ => IniView::Overrides,
        }
    }
}

/// The php.ini editor: whose file it is, what the user typed, and which pane.
struct IniEditor {
    project: i64,
    view: IniView,
    overrides: Entity<TextareaState>,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

/// One row of the php.ini diff.
enum DiffRow {
    /// A line both sides share.
    Same(String),
    /// Lines both sides share, collapsed because nobody reads them.
    Skipped(usize),
    /// Only the generated file has this line.
    Removed(String),
    /// Only the effective file has this line.
    Added(String),
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
    /// Whether the sidebar is collapsed to its icons.
    sidebar_collapsed: bool,
    /// The php.ini editor, when it is open.
    ini_editor: Option<IniEditor>,
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
    /// How far the current download has got, when one is running.
    busy_progress: Option<Arc<Mutex<f32>>>,

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
    log_wrap: bool,
    log_match: usize,
    /// Which level the log is filtered to.
    log_level: LogLevel,
    log_scroll: ScrollHandle,
    /// The last set of sites published to the tray, to avoid needless rebuilds.
    tray_sites: Vec<tray::TraySite>,
    /// The last sample per running service, and the machine totals, with the
    /// history behind their sparklines.
    service_metrics: Vec<ServiceSample>,
    machine_metrics: VecDeque<(f64, f64)>,
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

        // Read before `settings` moves into the struct.
        let log_follow = settings.log_follow.unwrap_or(true);
        let log_wrap = settings.log_wrap.unwrap_or(false);
        let log_case = settings.log_case.unwrap_or(false);
        let sidebar_collapsed = settings.sidebar_collapsed.unwrap_or(false);

        let mut this = Self {
            theme: settings.theme,            settings,
            toasts: Vec::new(),
            update: UpdateState::Idle,
            autostart: autostart::is_enabled(),
            store,
            projects: Vec::new(),
            selected: None,
            editor: None,
            ini_editor: None,
            sidebar_collapsed,
            error,
            screen: Screen::Projects,
            runtimes: Vec::new(),
            catalog: None,
            catalog_loading: false,
            catalog_error: None,
            catalog_fetched_at: None,
            busy_runtime: None,
            busy_progress: None,
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
            project_tab: ProjectTab::Services,
            log_filter,
            log_case,
            log_follow,
            log_wrap,
            log_match: 0,
            log_level: LogLevel::All,
            log_scroll: ScrollHandle::default(),
            tray_sites: Vec::new(),
            service_metrics: Vec::new(),
            machine_metrics: VecDeque::new(),
            palette,
            palette_open: false,
            runtime_tab: RuntimeTab::Https,
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
            icon: existing.as_ref().and_then(|project| project.icon_path.clone()),
            icon_source: None,
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

    /// Open the php.ini editor for a project.
    fn open_ini_editor(
        &mut self,
        project: Project,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let overrides = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("upload_max_filesize = 128M\nmax_execution_time = 300")
                .default_value(project.php_ini_overrides.clone())
        });
        self.ini_editor = Some(IniEditor {
            project: project.id,
            view: IniView::Overrides,
            overrides,
            error: None,
            _subscriptions: Vec::new(),
        });

        let entity = cx.entity();
        window.open_dialog(cx, move |dialog, _window, cx| {
            let read = entity.read(cx);
            let Some(editor) = read.ini_editor.as_ref() else {
                return dialog;
            };
            let Some(project) = read.projects.iter().find(|p| p.id == editor.project) else {
                return dialog;
            };
            let title = format!("php.ini \u{00b7} {}", project.name);
            let error = editor.error.clone();
            let body = ini_editor_body(editor, &entity, read, project, cx);
            dialog
                .title(title)
                .w(px(760.))
                .close_button(true)
                .child(body)
                .footer(ini_editor_footer(entity.clone(), error))
        });
    }

    /// The whole php.ini Wisp would write, with `overrides` applied last.
    ///
    /// Passing an empty string for `overrides` gives the file without the
    /// user's additions, which is what the diff needs as its base.
    fn generated_ini(&self, project: &Project, overrides: &str) -> String {
        let Some(version) = project.php_version.as_deref() else {
            return String::new();
        };
        let runtime_dir = runtime::install_dir(RuntimeKind::Php, version);
        let template =
            std::fs::read_to_string(runtime_dir.join("php.ini-development")).unwrap_or_default();
        let extension_dir = runtime_dir.join("ext");
        let dir = serve::config::site_dir(&project.slug());
        let error_log = dir.join("logs").join("php-error.log");
        serve::config::php_ini(&template, &extension_dir, &error_log, overrides)
    }

    /// Save the overrides and restart PHP so they take effect.
    fn save_ini_editor(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(editor) = self.ini_editor.as_ref() else {
            return false;
        };
        let id = editor.project;
        let text = editor.overrides.read(cx).value().to_string();
        let Some(project) = self.projects.iter().find(|p| p.id == id).cloned() else {
            return false;
        };

        let mut draft = ProjectDraft::from_project(&project);
        draft.php_ini_overrides = text;

        let Some(store) = &self.store else {
            self.set_ini_error("The project database is unavailable.".to_string(), cx);
            return false;
        };

        match store.update_project(id, &draft) {
            Ok(()) => {
                self.ini_editor = None;
                self.reload(cx);
                // Dropping the generated file is what makes the worker restart.
                self.restart_service(id, true, cx);
                self.push_toast(
                    ToastKind::Success,
                    format!("Saved php.ini for {}.", project.name),
                );
                cx.notify();
                true
            }
            Err(error) => {
                self.set_ini_error(error.to_string(), cx);
                false
            }
        }
    }

    /// Empty the overrides, which hands the file back to the generated values.
    fn reset_ini_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.ini_editor.as_ref() else {
            return;
        };
        editor
            .overrides
            .update(cx, |state, cx| state.set_value(String::new(), window, cx));
        cx.notify();
    }

    fn set_ini_error(&mut self, message: String, cx: &mut Context<Self>) {
        if let Some(editor) = self.ini_editor.as_mut() {
            editor.error = Some(message);
        }
        cx.notify();
    }

    fn close_ini_editor(&mut self, cx: &mut Context<Self>) {
        self.ini_editor = None;
        cx.notify();
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

    /// Open the native file picker for the project's icon.
    fn choose_icon(&mut self, cx: &mut Context<Self>) {
        let Some(picked) = rfd::FileDialog::new()
            .add_filter("Image", &["png", "jpg", "jpeg", "gif", "webp", "bmp", "ico"])
            .set_title("Choose a project icon")
            .pick_file()
        else {
            return;
        };
        if let Some(editor) = self.editor.as_mut() {
            editor.icon = Some(picked.to_string_lossy().to_string());
            editor.icon_source = Some(picked);
        }
        cx.notify();
    }

    /// Drop the project's icon, falling back to the generated squircle.
    fn remove_icon(&mut self, cx: &mut Context<Self>) {
        if let Some(editor) = self.editor.as_mut() {
            editor.icon = None;
            editor.icon_source = None;
        }
        cx.notify();
    }

    /// Copy a picked image next to the project's generated config under a token
    /// name, returning where it landed.
    fn adopt_icon(
        source: &std::path::Path,
        slug: &str,
    ) -> std::io::Result<String> {
        let extension = source
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("png")
            .to_ascii_lowercase();
        let destination = project_icon_path(slug, &random_token(), &extension);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(source, &destination)?;
        Ok(destination.to_string_lossy().to_string())
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
        let id = editor.id;
        let previous = id.and_then(|id| self.projects.iter().find(|p| p.id == id).cloned());
        let icon_source = editor.icon_source.clone();
        let chosen_icon = editor.icon.clone();

        let mut draft = ProjectDraft {
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
            icon_path: chosen_icon,
        };

        // A file picked this session is copied in under a token name; anything
        // it replaces is deleted once the record points at the new one, so
        // replacing an icon does not leave the old file behind.
        if let Some(source) = icon_source {
            match Self::adopt_icon(&source, &db::slugify(&draft.app_name)) {
                Ok(path) => draft.icon_path = Some(path),
                Err(error) => {
                    self.set_editor_error(
                        format!("Could not save that image: {error}"),
                        cx,
                    );
                    return false;
                }
            }
        }

        let before = previous;

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
                // The icon the record no longer points at is no longer wanted.
                if let Some(previous) = before.as_ref().and_then(|p| p.icon_path.clone()) {
                    if Some(&previous) != draft.icon_path.as_ref() {
                        let _ = std::fs::remove_file(previous);
                    }
                }
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
        let progress = Arc::new(Mutex::new(0.0_f32));
        self.busy_progress = Some(progress.clone());
        cx.notify();

        let version = release.version.clone();
        cx.spawn(async move |entity, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    let report = |fraction: f32| {
                        if let Ok(mut slot) = progress.lock() {
                            *slot = fraction;
                        }
                    };
                    runtime::install(kind, &release, &report)
                        .map(|path| path.to_string_lossy().to_string())
                })
                .await;

            entity
                .update(cx, |this, cx| {
                    this.busy_runtime = None;
                    this.busy_progress = None;
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
        let progress = Arc::new(Mutex::new(0.0_f32));
        self.busy_progress = Some(progress.clone());
        cx.notify();

        let version = release.version.clone();
        let previous = current.clone();
        cx.spawn(async move |entity, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    let report = |fraction: f32| {
                        if let Ok(mut slot) = progress.lock() {
                            *slot = fraction;
                        }
                    };
                    let path = runtime::install(RuntimeKind::Php, &release, &report)?;
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
                    let (status, tracked) = {
                        let supervisor = &mut cx.global_mut::<ServerGlobal>().0;
                        (supervisor.status(), supervisor.tracked())
                    };
                    if let Ok(mut slot) = cx.global::<TrackedProcesses>().0.lock() {
                        *slot = tracked;
                    }
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

    /// Sample what Wisp is running, every other second, plus the machine.
    fn spawn_metrics_poll(cx: &mut Context<Self>) {
        let tracked = cx.global::<TrackedProcesses>().0.clone();
        cx.spawn(async move |entity, cx| {
            let mut system = sysinfo::System::new();
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(2000))
                    .await;

                let wanted = tracked
                    .lock()
                    .map(|slot| slot.clone())
                    .unwrap_or_default();

                system.refresh_cpu_usage();
                system.refresh_memory();
                system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

                let cpu = system.global_cpu_usage() as f64;
                let total = system.total_memory();
                let ram = if total == 0 {
                    0.0
                } else {
                    system.used_memory() as f64 / total as f64 * 100.0
                };
                let samples = measure_services(&system, &wanted);

                let applied = entity.update(cx, |this, cx| {
                    this.push_metrics(samples, cpu, ram);
                    cx.notify();
                });
                if applied.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Fold one round of samples into the histories the sparklines draw.
    fn push_metrics(
        &mut self,
        samples: Vec<(Option<i64>, f64, u64)>,
        machine_cpu: f64,
        machine_ram: f64,
    ) {
        merge_samples(&mut self.service_metrics, samples);

        if self.machine_metrics.len() == METRIC_SAMPLES {
            self.machine_metrics.pop_front();
        }
        self.machine_metrics.push_back((machine_cpu, machine_ram));
    }

    /// The lines the log view is currently showing, in order.
    fn visible_log_lines(&self, cx: &App) -> Vec<String> {
        let raw = self.log_filter.read(cx).value().to_string();
        let needle = if self.log_case { raw } else { raw.to_lowercase() };
        self.log_lines
            .iter()
            .filter(|line| line_has_level(line, self.log_level))
            .filter(|line| {
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
            .cloned()
            .collect()
    }

    /// Remember the log viewer's toggles, so they survive a restart.
    fn save_log_prefs(&mut self) {
        self.settings.log_follow = Some(self.log_follow);
        self.settings.log_wrap = Some(self.log_wrap);
        self.settings.log_case = Some(self.log_case);
        self.settings.save();
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

    /// Start or stop both of a project's services together.
    fn set_project(&mut self, id: i64, wanted: bool, cx: &mut Context<Self>) {
        let changed = if wanted {
            let nginx = self.nginx_on.insert(id);
            let php = self.php_on.insert(id);
            nginx || php
        } else {
            let nginx = self.nginx_on.remove(&id);
            let php = self.php_on.remove(&id);
            nginx || php
        };
        if changed {
            self.sync_server(cx);
        }
    }

    /// Restart a project's PHP worker, which is what its nginx entry shares.
    fn restart_project(&mut self, id: i64, cx: &mut Context<Self>) {
        self.restart_service(id, true, cx);
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
                tray::SiteCommand::OpenFiles => {
                    let dir = self
                        .projects
                        .iter()
                        .find(|project| project.id == id)
                        .map(|project| serve::config::site_dir(&project.slug()));
                    if let Some(dir) = dir {
                        self.open_folder(&dir.to_string_lossy(), cx);
                    }
                }
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

        // Center: the selected project, ellipsised rather than clipped, with its
        // address as a link.
        if let Some(project) = self.selected_project() {
            let serving = self.nginx_on.contains(&project.id);
            let state = if serving { "serving" } else { "stopped" };
            let url = format!("{}{}", scheme_prefix(project.ssl_enabled), project.domain);
            let name = project.name.clone();
            let wide_here = wide;
            let mut row = div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .overflow_hidden()
                .child(div().flex_shrink_0().child(if wide_here {
                    format!("{name} \u{00b7} {state}")
                } else {
                    name
                }));
            if wide_here {
                row = row.child(Link::new("status-domain").href(url.clone()).child(url));
            }
            bar = bar.child(row);
        }

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

        let controls = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .child(site_button(
                "status-start-all",
                IconName::Play,
                labels.map_or("", |l| l.0),
                "Start every project and nginx",
                any_stopped,
                Wisp::start_all,
            ))
            .child(site_button(
                "status-restart-all",
                IconName::RotateCcw,
                labels.map_or("", |l| l.1),
                "Restart every project's worker",
                any_running,
                Wisp::restart_all,
            ))
            .child(site_button(
                "status-stop-all",
                IconName::Square,
                labels.map_or("", |l| l.2),
                "Stop every project and nginx",
                any_running,
                Wisp::stop_all,
            ));

        bar.right(Separator::vertical())
            .right(controls)
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

    fn sidebar(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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

        // Runtimes and Settings are destinations of their own, so they sit below
        // the projects as full-width menu rows rather than as a pair of buttons.
        let runtime_count = self.runtimes.len();
        let nav = SidebarMenu::new()
            .w_full()
            .child(
                SidebarMenuItem::new("Runtimes")
                    .icon(IconName::Server)
                    .active(on_runtimes)
                    .suffix(move |_window, cx| runtime_count_suffix(runtime_count, cx))
                    .on_click(cx.listener(|this, _, _window, cx| this.goto(Screen::Services, cx))),
            )
            .child(
                SidebarMenuItem::new("Settings")
                    .icon(IconName::Settings)
                    .active(on_settings)
                    .on_click(cx.listener(|this, _, _window, cx| this.goto(Screen::Settings, cx))),
            );

        // The header is the search field with the collapse toggle beside it, the
        // way a standard sidebar carries its own toggle. Collapsed, there is no
        // room for a field, so it is dropped rather than squashed into the icon
        // strip and the toggle is left centred on its own.
        let mut header_row = div().flex().flex_row().items_center().gap_2().w_full();
        if self.sidebar_collapsed {
            header_row = header_row.justify_center();
        } else {
            header_row = header_row.child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .child(Input::new(&self.project_filter)),
            );
        }
        let header = SidebarHeader::new().child(
            header_row.child(
                SidebarToggleButton::new()
                    .collapsed(self.sidebar_collapsed)
                    .on_click(cx.listener(|this, _, _window, cx| this.toggle_sidebar(cx))),
            ),
        );

        Sidebar::new("wisp-sidebar")
            .collapsible(SidebarCollapsible::Icon)
            .collapsed(self.sidebar_collapsed)
            .header(header)
            .child(SidebarGroup::new("Projects").child(menu))
            .footer(nav.render("sidebar-nav", window, cx))
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

        let name = project.name.clone();
        let domain = project.domain.clone();
        let ssl = project.ssl_enabled;
        // The domain is the one thing here worth clicking, and it is a link
        // rather than a button beside it. Edit and Remove live on the Settings
        // tab, where they are needed once rather than always.
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .child(project_icon(project, px(44.)))
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
                                Link::new("header-domain")
                                    .href(format!("{}{}", scheme_prefix(ssl), domain))
                                    .text_sm()
                                    .child(format!("{}{}", scheme_prefix(ssl), domain)),
                            ),
                    ),
            );

        let tabs = TabBar::new("project-tabs")
            .segmented()
            .selected_index(self.project_tab.index())
            .children([
                Tab::new().label("Services"),
                Tab::new().label("Overview"),
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

        // Only the tab that is showing is built: a full project page is deep
        // enough that materialising all four would be wasteful every frame.
        let body: AnyElement = match self.project_tab {
            ProjectTab::Overview => div()
                .flex()
                .flex_col()
                .gap_4()
                .child(self.project_details(project, cx))
                .child(self.resources_card(Some(project.id), cx))
                .into_any_element(),
            ProjectTab::Services => self.project_services(project, cx),
            ProjectTab::Logs => self.logs_card(cx).into_any_element(),
            ProjectTab::Settings => self.project_settings(project, window, cx),
        };

        let page = div()
            .id("content")
            .flex()
            .flex_col()
            .size_full()
            .gap_4()
            .p(px(28.))
            .child(header)
            .child(tabs)
            .child(body);

        // Logs sizes its own pane to the window and scrolls inside it. Wrapping
        // that in the page's scroll region made the wheel move both, so the log
        // tab is the one tab that does not scroll as a page.
        if self.project_tab == ProjectTab::Logs {
            page.into_any_element()
        } else {
            page.overflow_y_scrollbar().into_any_element()
        }
    }

    /// The Overview tab: the project's configuration at a glance.
    fn project_details(&self, project: &Project, cx: &mut Context<Self>) -> impl IntoElement {
        let root = project.root_path.clone();
        let url = format!("{}{}", scheme_prefix(project.ssl_enabled), project.domain);
        let php = project
            .php_version
            .clone()
            .unwrap_or_else(|| "Not set".to_string());
        let config_display = crate::config::sites()
            .join(project.slug())
            .to_string_lossy()
            .to_string();

        ui::section(
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
                .child(detail_row_link(
                    "open-root",
                    "Root folder",
                    &root,
                    folder_url(&root),
                    cx,
                ))
                .child(detail_row_link("open-url", "Domain", &url, url.clone(), cx))
                .child(detail_row(
                    "HTTPS",
                    if project.ssl_enabled {
                        "Trusted certificate"
                    } else {
                        "Off"
                    },
                    cx,
                ))
                .child(detail_row("PHP", &php, cx))
                .child(detail_row_link(
                    "open-config",
                    "Config folder",
                    &config_display,
                    folder_url(&config_display),
                    cx,
                )),
        )
    }

    /// The Services tab: this project's nginx server block and its PHP worker.
    fn project_services(&self, project: &Project, cx: &mut Context<Self>) -> AnyElement {
        let id = project.id;
        let url = format!("{}{}", scheme_prefix(project.ssl_enabled), project.domain);
        let php = project
            .php_version
            .clone()
            .unwrap_or_else(|| "Not set".to_string());
        let worker_up = self.serve_status.running_projects.contains(&id);
        let nginx_up = self.serve_status.nginx_running;
        let nginx_on = self.nginx_on.contains(&id);
        let php_on = self.php_on.contains(&id);
        let busy = self.serve_busy;

        let header = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_start()
            .justify_between()
            .gap_4()
            .child(div().flex_1().child(section_header(
                "Services",
                "This project's nginx server block and its PHP worker.",
                cx,
            )))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .pt(px(14.))
                    .pr(px(18.))
                    .child(
                        Button::new("project-start")
                            .primary()
                            .small()
                            .icon(icon(IconName::Play))
                            .label("Start")
                            .tooltip("Start this project's nginx entry and PHP worker")
                            .disabled(busy || (nginx_on && php_on))
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.set_project(id, true, cx)
                            })),
                    )
                    .child(
                        Button::new("project-restart")
                            .outline()
                            .small()
                            .icon(icon(IconName::RotateCcw))
                            .label("Restart")
                            .tooltip("Restart this project's PHP worker")
                            .disabled(busy || !(nginx_on || php_on))
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.restart_project(id, cx)
                            })),
                    )
                    .child(
                        Button::new("project-stop")
                            .danger()
                            .small()
                            .icon(icon(IconName::Square))
                            .label("Stop")
                            .tooltip("Stop this project's nginx entry and PHP worker")
                            .disabled(busy || !(nginx_on || php_on))
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.set_project(id, false, cx)
                            })),
                    ),
            );
        let nginx_state: AnyElement = if !nginx_on {
            div().child("Not serving").into_any_element()
        } else if nginx_up {
            Link::new("nginx-serving")
                .href(url.clone())
                .child(format!("Serving {url}"))
                .into_any_element()
        } else {
            div().child("Starting\u{2026}").into_any_element()
        };
        let php_state: AnyElement = div()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(if !php_on {
                "Stopped".to_string()
            } else if worker_up {
                format!("php-cgi {php}")
            } else {
                "Starting\u{2026}".to_string()
            })
            .into_any_element();
        let rows = div()
            .flex()
            .flex_col()
            .p(px(8.))
            .child(project_service_row(
                "nginx",
                nginx_state,
                nginx_on,
                ProjectService::Nginx,
                id,
                busy,
                cx,
            ))
            .child(project_service_row(
                "PHP",
                php_state,
                php_on,
                ProjectService::Php,
                id,
                busy,
                cx,
            ));

        ui::section(
            cx,
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(header)
                .child(rows),
        )
        .into_any_element()
    }

    /// What Wisp is running, with the machine underneath it as context.
    ///
    /// `only` narrows the card to one project, which is what the Overview tab
    /// wants; `None` lists every service Wisp started.
    fn resources_card(&self, only: Option<i64>, cx: &mut Context<Self>) -> AnyElement {
        let header = section_header(
            "Resources",
            "The processes Wisp started, sampled every two seconds.",
            cx,
        );

        let shown: Vec<&ServiceSample> = self
            .service_metrics
            .iter()
            .filter(|sample| only.is_none() || sample.project == only)
            .collect();

        let mut body = div().flex().flex_col().gap_4().p(px(18.));
        if shown.is_empty() {
            body = body.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(match only {
                        Some(_) => "This project is not running, so it is using nothing.",
                        None => "Nothing is running yet. Start a site to see what it costs.",
                    }),
            );
        } else {
            for sample in shown {
                let (name, detail) = self.service_name(sample.project);
                body = body.child(service_row(&name, &detail, sample, cx));
            }
        }

        // Machine context, so a heavy site can be told apart from a busy PC.
        let (cpu, ram) = self.machine_metrics.back().copied().unwrap_or((0.0, 0.0));
        body = body.child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap_2()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(format!(
                    "This machine: CPU {cpu:.1}% \u{00b7} RAM {ram:.1}%"
                )),
        );

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

    /// What a tracked service should be called: nginx, or the project it serves.
    fn service_name(&self, project: Option<i64>) -> (String, String) {
        let Some(id) = project else {
            return ("nginx".to_string(), "shared server".to_string());
        };
        match self.projects.iter().find(|project| project.id == id) {
            Some(project) => (
                project.name.clone(),
                match &project.php_version {
                    Some(version) => format!("php-cgi {version}"),
                    None => "php-cgi".to_string(),
                },
            ),
            None => ("php-cgi".to_string(), String::new()),
        }
    }

    /// Collapse or expand the sidebar, and remember the choice.
    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {        self.sidebar_collapsed = !self.sidebar_collapsed;
        self.settings.sidebar_collapsed = Some(self.sidebar_collapsed);
        self.settings.save();
        cx.notify();
    }

    /// How far the running download has got, if one is running.
    fn current_progress(&self) -> Option<f32> {
        self.busy_progress
            .as_ref()
            .and_then(|progress| progress.lock().ok().map(|slot| *slot))
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
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .items_center()
                        .justify_between()
                        .gap_3()
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child("php.ini"),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child("Overrides are appended last, so they win."),
                                ),
                        )
                        .child(
                            Button::new("edit-ini")
                                .outline()
                                .small()
                                .icon(icon(IconName::SquarePen))
                                .label("Edit php.ini")
                                .on_click({
                                    let entity = cx.entity();
                                    move |_, window, cx| {
                                        let project = entity
                                            .read(cx)
                                            .projects
                                            .iter()
                                            .find(|p| p.id == id)
                                            .cloned();
                                        if let Some(project) = project {
                                            entity.update(cx, |this, cx| {
                                                this.open_ini_editor(project, window, cx)
                                            });
                                        }
                                    }
                                }),
                        ),
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
                            "None yet \u{2014} the generated file is used as-is.".to_string()
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
            RuntimeTab::Https => div()
                .flex()
                .flex_col()
                .gap_4()
                .child(self.https_card(cx))
                .child(self.certificates_card(cx))
                .into_any_element(),
            RuntimeTab::Hosts => div()
                .flex()
                .flex_col()
                .gap_4()
                .child(self.hosting_card(cx))
                .child(self.hosts_block_card(cx))
                .into_any_element(),
            RuntimeTab::Resources => self.resources_card(None, cx),
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
                if !line_has_level(line, self.log_level) {
                    return false;
                }
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
                        // The filters are in the source's own terms, so they
                        // start again when the source changes.
                        this.log_level = LogLevel::All;
                        this.refresh_logs(cx);
                    }
                    cx.notify();
                }))
                .into_any_element()
        })
        .collect();

        let leading: Vec<AnyElement> = sources;

        let count = if matching {
            format!(
                "{} of {} lines",
                self.log_match.saturating_add(1).min(shown),
                shown
            )
        } else {
            format!("{total} lines")
        };
        // One button rather than a chip per level: the toolbar has to hold the
        // search, the navigation and the toggles too, and they were being
        // pushed off the edge.
        let level_button = {
            let current_level = self.log_level;
            let entity = cx.entity();
            Button::new("log-level")
                .outline()
                .small()
                .label(current_level.label())
                .tooltip("Filter by level or status")
                .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, _| {
                    let mut menu = menu.check_side(Side::Right);
                    for level in LogLevel::for_kind(current).iter().copied() {
                        let entity = entity.clone();
                        let is_current = level == current_level;
                        menu = menu.item(
                            PopupMenuItem::new(level.label())
                                .checked(is_current)
                                .on_click(move |_, _, cx| {
                                    entity.update(cx, |this, cx| {
                                        this.log_level = level;
                                        this.log_match = 0;
                                        cx.notify();
                                    });
                                }),
                        );
                    }
                    menu
                })
        };

        let header = section_header("Logs", "The tail of this project's logs.", cx);
        // Laid out here rather than through Toolbar: the log has more controls
        // than fit on one line, and wrapping them onto a second is better than
        // pushing the search box off the edge.
        let toolbar = div()
            .flex()
            .flex_col()
            .gap_2()
            .px(px(18.))
            .pb(px(10.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .children(leading)
                    .child(level_button),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(180.)).child(Input::new(&self.log_filter)))
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
                            this.save_log_prefs();
                            cx.notify();
                        }))
                    })
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child(count.clone()))
                    .child(
                        Button::new("log-prev")
                            .ghost()
                            .xsmall()
                            .icon(icon(IconName::ChevronLeft))
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
                            this.save_log_prefs();
                            cx.notify();
                        }))
                    })
                    .child({
                        let button = Button::new("log-wrap")
                            .small()
                            .label("Wrap")
                            .tooltip("Wrap long lines instead of scrolling sideways");
                        let button = if self.log_wrap {
                            button.primary()
                        } else {
                            button.outline()
                        };
                        button.on_click(cx.listener(|this, _, _window, cx| {
                            this.log_wrap = !this.log_wrap;
                            this.save_log_prefs();
                            cx.notify();
                        }))
                    })
                    .child(
                        Button::new("log-copy")
                            .ghost()
                            .small()
                            .icon(icon(IconName::Copy))
                            .tooltip("Copy the lines shown")
                            .disabled(visible.is_empty())
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                let text = this.visible_log_lines(cx).join("\r\n");
                                cx.write_to_clipboard(ClipboardItem::new_string(text));
                                this.push_toast(
                                    ToastKind::Success,
                                    "Copied the lines shown.".to_string(),
                                );
                                cx.notify();
                            })),
                    )
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
            // A stripe on alternating rows, so a dense log is easier to follow
            // across. It is the card's own colour lightened, rather than another
            // surface token: `muted`, `accent`, `background` and a translucent
            // overlay all sat too close to `secondary` to read as a row, which a
            // diagnostic fill in pure red proved was the colour and not the
            // layout.
            let mut stripe = cx.theme().secondary;
            stripe.l = (stripe.l + 0.13).min(1.0);

            let lines: Vec<AnyElement> = visible
                .iter()
                .map(|(index, line)| {
                    let mut row = div()
                        .w_full()
                        .px(px(2.))
                        .child(log_line(line, &needle, self.log_case, self.log_wrap, cx));
                    if index % 2 == 1 {
                        row = row.bg(stripe);
                    }
                    row.into_any_element()
                })
                .collect();

            // Grows into whatever the card has left after its header and
            // toolbar. This used to be a height derived from the window, which
            // overshot and let the log run past the card and off the window.
            div()
                .relative()
                .flex_1()
                .min_h(px(0.))
                .w_full()
                .px(px(18.))
                .pb(px(18.))
                .child(
                    div()
                        .id("log-lines")
                        .track_scroll(&self.log_scroll)
                        .overflow_y_scroll()
                        .size_full()
                        .px(px(12.))
                        .py(px(10.))
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_xs()
                        .children(lines),
                )
                .vertical_scrollbar(&self.log_scroll)
                .into_any_element()
        };

        ui::section_grow(
            cx,
            div()
                .flex()
                .flex_col()
                .h_full()
                .gap_1()
                .child(header)
                .child(toolbar)
                .child(body),
        )
    }

    /// The hosts block Wisp manages, as it is on disk.
    fn hosts_block_card(&self, cx: &mut Context<Self>) -> AnyElement {
        let header = section_header(
            "Managed entries",
            "The block Wisp owns in the hosts file. Everything else is left alone.",
            cx,
        );
        let body: AnyElement = match managed_hosts_block() {
            Some(text) => code_pane("hosts-code", text, cx),
            None => div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("Wisp has not written its block yet. Starting a site adds it.")
                .into_any_element(),
        };

        ui::section(
            cx,
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(header)
                .child(div().px(px(18.)).pb(px(18.)).child(body)),
        )
        .into_any_element()
    }

    /// The certificates Wisp has issued, and how long ago.
    fn certificates_card(&self, cx: &mut Context<Self>) -> AnyElement {
        let header = section_header(
            "Certificates",
            "One per domain, issued by Wisp's own authority.",
            cx,
        );

        let rows: Vec<AnyElement> = self
            .projects
            .iter()
            .map(|project| {
                let state = match tls::expiry(&project.domain) {
                    Some(expires) => human_expiry(expires),
                    None => "not issued yet".to_string(),
                };
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
                            .child("Domain"),
                    )
                    .child(
                        Link::new(SharedString::from(format!("cert-{}", project.id)))
                            .href(format!("https://{}", project.domain))
                            .text_sm()
                            .child(project.domain.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(state),
                    )
                    .into_any_element()
            })
            .collect();

        let body: AnyElement = if rows.is_empty() {
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("No projects yet, so nothing has been issued.")
                .into_any_element()
        } else {
            div().flex().flex_col().gap_3().children(rows).into_any_element()
        };

        ui::section(
            cx,
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(header)
                .child(div().px(px(18.)).pb(px(18.)).child(body)),
        )
        .into_any_element()
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
                php_row(
                    line,
                    current.as_ref(),
                    latest,
                    self.busy_runtime.as_deref(),
                    self.current_progress(),
                    cx,
                )
            })
            .collect();

        let body = if rows.is_empty() {
            if self.catalog_loading {
                version_skeletons()
            } else {
                div()
                    .p(px(18.))
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("No PHP versions were found.")
                    .into_any_element()
            }
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
        let progress = self.current_progress();

        let rows: Vec<AnyElement> = installed
            .iter()
            .map(|runtime| {
                let is_active = active.as_deref() == Some(runtime.version.as_str());
                nginx_row(runtime, is_active, busy, progress, cx)
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

        let sidebar = self.sidebar(window, cx);
        let content = self.content(window, cx);
        let status = self.status_bar(window, cx);

        let page = root
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_hidden()
                    .child(
                        // Fixed width, not resizable: `Sidebar` sizes itself and
                        // offers no width builder, so a draggable panel either
                        // left a dead band beside it when widened or let its rows
                        // paint over the page when squeezed. It still collapses
                        // to icons from the toggle in the top bar.
                        div()
                            .flex_shrink_0()
                            .h_full()
                            .overflow_hidden()
                            .child(sidebar),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .h_full()
                            .overflow_hidden()
                            .child(content),
                    ),
            )
            .child(status);

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
/// Placeholder rows while the version list is on its way.
fn version_skeletons() -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .p(px(18.))
        .child(Skeleton::new().h(px(24.)).w_full())
        .child(Skeleton::new().h(px(24.)).w_full())
        .child(Skeleton::new().h(px(24.)).w(px(320.)))
        .into_any_element()
}

/// A download in flight, as a bar with its percentage beside it.
fn download_progress(id: &'static str, progress: Option<f32>, cx: &App) -> AnyElement {
    let fraction = progress.unwrap_or(0.0);
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .child(div().w(px(120.)).child(Progress::new(id).value(fraction)))
        .child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(format!("{}%", (fraction * 100.0).round() as i32)),
        )
        .into_any_element()
}

fn php_row(
    line: &str,
    current: Option<&InstalledRuntime>,
    latest: &Release,
    busy: Option<&str>,
    progress: Option<f32>,
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
        vec![download_progress("php-progress", progress, cx)]
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
    progress: Option<f32>,
    cx: &mut Context<Wisp>,
) -> AnyElement {
    let version = runtime.version.clone();
    let busy_here = busy.is_some_and(|key| key == format!("nginx/{version}"));

    let actions: Vec<AnyElement> = if busy_here {
        vec![download_progress("nginx-progress", progress, cx)]
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
    status: impl IntoElement,
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
                .child(status)
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


/// A `file:///` URL for a Windows path, so the shell opens it in Explorer.
fn folder_url(path: &str) -> String {
    format!("file:///{}", path.replace('\\', "/"))
}

/// A `label  link` row: the value is the thing you click, with no button beside
/// it repeating what the text already says.
fn detail_row_link(
    id: &'static str,
    label: &str,
    text: &str,
    href: String,
    cx: &App,
) -> AnyElement {
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
                .flex_shrink(1.0)
                .min_w(px(0.))
                .overflow_hidden()
                .child(Link::new(id).href(href).text_sm().child(text.to_string())),
        )
        .into_any_element()
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

/// One service: what it is, and what it is costing right now.
fn service_row(name: &str, detail: &str, sample: &ServiceSample, cx: &App) -> AnyElement {
    let key = sample
        .project
        .map_or_else(|| "nginx".to_string(), |id| id.to_string());
    let cpu_max = sample.cpu_history.iter().copied().fold(100.0_f64, f64::max);
    let ram_max = sample.ram_history.iter().copied().fold(1.0_f64, f64::max);
    let megabytes = sample.ram_bytes as f64 / (1024.0 * 1024.0);

    div()
        .flex()
        .flex_col()
        .gap_2()
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
                        .font_weight(FontWeight::MEDIUM)
                        .child(name.to_string()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(detail.to_string()),
                ),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap_4()
                .child(service_meter(
                    "CPU",
                    &format!("{:.1}%", sample.cpu),
                    format!("cpu-{key}"),
                    sample.cpu_history.iter().copied().collect(),
                    cpu_max,
                    "CPU",
                    format_percent,
                    cx.theme().primary,
                    cx,
                ))
                .child(service_meter(
                    "RAM",
                    &format!("{megabytes:.1} MB"),
                    format!("ram-{key}"),
                    sample.ram_history.iter().copied().collect(),
                    ram_max,
                    "RAM",
                    format_megabytes,
                    cx.theme().warning,
                    cx,
                )),
        )
        .into_any_element()
}

/// `label  value  sparkline`, one metric of one service.
#[allow(clippy::too_many_arguments)]
fn service_meter(
    label: &str,
    value: &str,
    id: String,
    values: Vec<f64>,
    max: f64,
    name: &'static str,
    format: fn(f64) -> String,
    color: Hsla,
    cx: &App,
) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .flex_1()
        .min_w(px(240.))
        .child(
            div()
                .w(px(34.))
                .flex_shrink(0.0)
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(label.to_string()),
        )
        .child(
            div()
                .w(px(74.))
                .flex_shrink(0.0)
                .text_xs()
                .child(value.to_string()),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(80.))
                .h(px(28.))
                .child(metric_sparkline(id, values, max, name, format, color)),
        )
        .child(
            // What the top of the chart is, since there is no axis to read it off.
            div()
                .flex_shrink_0()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(format!("peak {}", format(max))),
        )
        .into_any_element()
}

/// A sparkline over a `0..max` domain, whose tooltip says what the numbers are.
fn metric_sparkline(
    id: String,
    values: Vec<f64>,
    max: f64,
    name: &'static str,
    format: fn(f64) -> String,
    color: Hsla,
) -> impl IntoElement {
    let total = values.len();
    AreaChart::new(values.into_iter().enumerate())
        .id(id)
        .name(name)
        .x(move |(index, _)| sample_age(total, *index))
        .y(|(_, value)| *value)
        .stroke(color)
        .tooltip_title(move |_| SharedString::from(name))
        .tooltip_value(move |_, _, value| SharedString::from(format(value)))
        .x_axis(false)
        .y_axis(false)
        .grid(false)
        .y_domain(0.0, if max <= 0.0 { 1.0 } else { max })
}

/// The php.ini editor's body: the overrides, the generated file, or the diff.
fn ini_editor_body(
    editor: &IniEditor,
    entity: &Entity<Wisp>,
    wisp: &Wisp,
    project: &Project,
    cx: &App,
) -> AnyElement {
    let tabs = TabBar::new("ini-tabs")
        .segmented()
        .selected_index(editor.view.index())
        .children([
            Tab::new().label("Overrides"),
            Tab::new().label("Generated"),
            Tab::new().label("Diff"),
        ])
        .on_click({
            let entity = entity.clone();
            move |index, _window, cx| {
                let view = IniView::from_index(*index);
                entity.update(cx, |this, cx| {
                    if let Some(editor) = this.ini_editor.as_mut() {
                        editor.view = view;
                    }
                    cx.notify();
                });
            }
        });

    let pane: AnyElement = match editor.view {
        IniView::Overrides => Textarea::new(&editor.overrides)
            .h(px(360.))
            .w_full()
            .into_any_element(),
        IniView::Generated => code_pane(
            "ini-code",
            wisp.generated_ini(project, &project.php_ini_overrides),
            cx,
        ),
        IniView::Diff => diff_pane(wisp, project, cx),
    };

    div()
        .flex()
        .flex_col()
        .gap_3()
        .child(tabs)
        .child(pane)
        .into_any_element()
}

/// A read-only block of monospace text in a bordered, scrolling pane.
fn code_pane(id: &'static str, text: String, cx: &App) -> AnyElement {
    div()
        .id(id)
        .w_full()
        .h(px(360.))
        .rounded_md()
        .border_1()
        .border_color(cx.theme().border)
        .overflow_y_scrollbar()
        .px(px(12.))
        .py(px(10.))
        .font_family(cx.theme().mono_font_family.clone())
        .text_xs()
        .child(text)
        .into_any_element()
}

/// What the project's overrides change about the generated file.
fn diff_pane(wisp: &Wisp, project: &Project, cx: &App) -> AnyElement {
    let base = wisp.generated_ini(project, "");
    let effective = wisp.generated_ini(project, &project.php_ini_overrides);
    let rows = diff_lines(&base, &effective, 3);

    if rows.is_empty() {
        return div()
            .id("ini-diff")
            .w_full()
            .h(px(360.))
            .rounded_md()
            .border_1()
            .border_color(cx.theme().border)
            .p(px(18.))
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child("No overrides yet, so the effective file is the generated one.")
            .into_any_element();
    }

    let mut body = div().flex().flex_col().py(px(6.));
    for row in rows {
        body = body.child(match row {
            DiffRow::Same(text) => diff_row(" ", &text, cx.theme().muted_foreground, cx),
            DiffRow::Skipped(count) => div()
                .px(px(10.))
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(format!("\u{2026} {count} unchanged lines"))
                .into_any_element(),
            DiffRow::Removed(text) => diff_row("-", &text, cx_error(), cx),
            DiffRow::Added(text) => diff_row("+", &text, cx.theme().success, cx),
        });
    }

    div()
        .id("ini-diff")
        .w_full()
        .h(px(360.))
        .rounded_md()
        .border_1()
        .border_color(cx.theme().border)
        .overflow_y_scrollbar()
        .font_family(cx.theme().mono_font_family.clone())
        .text_xs()
        .child(body)
        .into_any_element()
}

/// One diff row: its marker, then the line.
fn diff_row(marker: &str, text: &str, color: Hsla, cx: &App) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .gap_2()
        .px(px(10.))
        .whitespace_nowrap()
        .child(div().w(px(8.)).flex_shrink(0.0).text_color(color).child(marker.to_string()))
        .child(div().text_color(cx.theme().foreground).child(text.to_string()))
        .into_any_element()
}

/// How an axis label should read for a sample `index` of `total`.
///
/// Samples are two seconds apart, so the x axis is an age rather than a sample
/// number: "-2m" says more than "31" ever did.
fn sample_age(total: usize, index: usize) -> String {
    let seconds = (total.saturating_sub(1).saturating_sub(index)) * 2;
    if seconds < 60 {
        format!("-{seconds}s")
    } else {
        format!("-{}m", seconds / 60)
    }
}

/// A percentage, as a chart value and a tooltip both read it.
fn format_percent(value: f64) -> String {
    format!("{value:.1}%")
}

/// Megabytes, as a chart value and a tooltip both read it.
fn format_megabytes(value: f64) -> String {
    format!("{value:.1} MB")
}

/// A line diff for the change Wisp makes, which is an append.
///
/// The generated file and the effective file share a long prefix and the
/// overrides tail on to the end, so there is no need for a full LCS: strip the
/// shared prefix and suffix and report what is left. Runs of unchanged lines
/// collapse to a count, because the rest of the file is thousands of defaults
/// nobody is looking at.
fn diff_lines(base: &str, other: &str, context: usize) -> Vec<DiffRow> {
    let base: Vec<&str> = base.lines().collect();
    let other: Vec<&str> = other.lines().collect();

    let mut prefix = 0;
    while prefix < base.len() && prefix < other.len() && base[prefix] == other[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix + prefix < base.len()
        && suffix + prefix < other.len()
        && base[base.len() - 1 - suffix] == other[other.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let mut rows = Vec::new();

    // Nothing differs anywhere: report nothing rather than reprinting the file.
    if prefix + suffix >= base.len() && prefix + suffix >= other.len() {
        return rows;
    }

    let head_from = prefix.saturating_sub(context);
    if head_from > 0 {
        rows.push(DiffRow::Skipped(head_from));
    }
    for line in &base[head_from..prefix] {
        rows.push(DiffRow::Same((*line).to_string()));
    }

    for line in &base[prefix..base.len() - suffix] {
        rows.push(DiffRow::Removed((*line).to_string()));
    }
    for line in &other[prefix..other.len() - suffix] {
        rows.push(DiffRow::Added((*line).to_string()));
    }

    let tail_len = suffix.min(context);
    for line in &base[base.len() - suffix..base.len() - suffix + tail_len] {
        rows.push(DiffRow::Same((*line).to_string()));
    }
    if suffix > tail_len {
        rows.push(DiffRow::Skipped(suffix - tail_len));
    }

    rows
}

fn ini_editor_footer(entity: Entity<Wisp>, error: Option<String>) -> impl IntoElement {
    let reset = Button::new("ini-reset")
        .label("Reset to generated")
        .outline()
        .on_click({
            let entity = entity.clone();
            move |_, window, cx| {
                entity.update(cx, |this, cx| this.reset_ini_editor(window, cx));
            }
        });

    let cancel = Button::new("ini-cancel")
        .label("Cancel")
        .outline()
        .on_click({
            let entity = entity.clone();
            move |_, window, cx| {
                entity.update(cx, |this, cx| this.close_ini_editor(cx));
                window.close_dialog(cx);
            }
        });

    let save = Button::new("ini-save")
        .label("Save & restart PHP")
        .primary()
        .on_click({
            let entity = entity.clone();
            move |_, window, cx| {
                let saved = entity.update(cx, |this, cx| this.save_ini_editor(cx));
                if saved {
                    window.close_dialog(cx);
                }
            }
        });

    let mut footer = div().flex().flex_col().gap_2();
    if let Some(message) = error {
        footer = footer.child(div().text_xs().text_color(cx_error()).child(message));
    }

    footer
        .child(
            div()
                .flex()
                .flex_row()
                .justify_between()
                .gap_2()
                .child(reset)
                .child(div().flex().flex_row().gap_2().child(cancel).child(save)),
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
                .label("Icon")
                .description("A squircle built from the name is used until you pick an image.")
                .child({
                    let entity = entity.clone();
                    let current = editor.icon.clone();
                    let name = editor.name.read(cx).value().to_string();
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_3()
                        .child(match current.as_deref() {
                            Some(path) if std::path::Path::new(path).is_file() => {
                                let path = SharedString::from(path.to_string());
                                div()
                                    .size(px(44.))
                                    .flex_shrink_0()
                                    .rounded(px(13.))
                                    .overflow_hidden()
                                    .child(img(path).size(px(44.)))
                                    .into_any_element()
                            }
                            _ => generated_icon(&name, px(44.)),
                        })
                        .child(
                            Button::new("choose-icon")
                                .outline()
                                .small()
                                .icon(icon(IconName::FolderOpen))
                                .label(if current.is_some() {
                                    "Replace image"
                                } else {
                                    "Choose image"
                                })
                                .on_click({
                                    let entity = entity.clone();
                                    move |_, _window, cx| {
                                        entity.update(cx, |this, cx| this.choose_icon(cx));
                                    }
                                }),
                        )
                        .child(
                            Button::new("remove-icon")
                                .ghost()
                                .small()
                                .label("Use generated")
                                .disabled(current.is_none())
                                .on_click({
                                    let entity = entity.clone();
                                    move |_, _window, cx| {
                                        entity.update(cx, |this, cx| this.remove_icon(cx));
                                    }
                                }),
                        )
                }),
        )
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

/// The muted count shown at the end of the Runtimes row.
fn runtime_count_suffix(count: usize, cx: &App) -> AnyElement {
    if count == 0 {
        return div().into_any_element();
    }
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(count.to_string())
        .into_any_element()
}

/// Push onto a capped history, dropping the oldest sample once it is full.
fn push_capped(history: &mut VecDeque<f64>, value: f64) {
    if history.len() == METRIC_SAMPLES {
        history.pop_front();
    }
    history.push_back(value);
}

/// CPU (as a percentage of one core) and memory for each tracked process tree.
///
/// A tree is measured whole, because nginx forks its workers and their cost
/// belongs to the server the user turned on, not to a stray child process.
fn measure_services(
    system: &sysinfo::System,
    wanted: &[serve::Tracked],
) -> Vec<(Option<i64>, f64, u64)> {
    let processes = system.processes();
    let parent_of = |pid: sysinfo::Pid| {
        processes
            .get(&pid)
            .and_then(sysinfo::Process::parent)
    };

    wanted
        .iter()
        .map(|tracked| {
            let root = sysinfo::Pid::from_u32(tracked.pid);
            let mut cpu = 0.0;
            let mut ram = 0_u64;
            for (pid, process) in processes {
                if !descends_from(&parent_of, *pid, root) {
                    continue;
                }
                cpu += process.cpu_usage() as f64;
                ram += process.memory();
            }
            (tracked.project, cpu, ram)
        })
        .collect()
}

/// Fold one round of samples into the histories the sparklines draw.
///
/// A service that is still running keeps its history; a restarted one starts
/// again, so the line does not jump across the restart.
fn merge_samples(existing: &mut Vec<ServiceSample>, samples: Vec<(Option<i64>, f64, u64)>) {
    let mut fresh = Vec::with_capacity(samples.len());
    for (project, cpu, ram_bytes) in samples {
        let (mut cpu_history, mut ram_history) = existing
            .iter()
            .find(|sample| sample.project == project)
            .map(|sample| (sample.cpu_history.clone(), sample.ram_history.clone()))
            .unwrap_or_default();
        push_capped(&mut cpu_history, cpu);
        // Stored in megabytes, not bytes: the chart, its axis and its tooltip all
        // read from this history, and raw byte counts meant nothing on screen.
        push_capped(&mut ram_history, ram_bytes as f64 / (1024.0 * 1024.0));
        fresh.push(ServiceSample {
            project,
            cpu,
            ram_bytes,
            cpu_history,
            ram_history,
        });
    }
    *existing = fresh;
}

/// Whether `pid` is `root` or one of its descendants.
///
/// The walk is bounded so a cycle in a corrupt process table cannot hang the
/// sampler.
fn descends_from(
    parent_of: &dyn Fn(sysinfo::Pid) -> Option<sysinfo::Pid>,
    pid: sysinfo::Pid,
    root: sysinfo::Pid,
) -> bool {
    let mut current = Some(pid);
    for _ in 0..64 {
        let Some(pid) = current else {
            return false;
        };
        if pid == root {
            return true;
        }
        current = parent_of(pid);
    }
    false
}

/// A coarse filter over the log, in the terms its source speaks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LogLevel {
    All,
    Fatal,
    Warning,
    Notice,
    /// Access lines that answered 4xx.
    Client,
    /// Access lines that answered 5xx.
    Server,
}

impl LogLevel {
    fn label(self) -> &'static str {
        match self {
            LogLevel::All => "All",
            LogLevel::Fatal => "Fatal",
            LogLevel::Warning => "Warning",
            LogLevel::Notice => "Notice",
            LogLevel::Client => "4xx",
            LogLevel::Server => "5xx",
        }
    }

    /// Which filters are worth offering for a given source.
    fn for_kind(kind: LogKind) -> &'static [LogLevel] {
        match kind {
            LogKind::Access => &[
                LogLevel::All,
                LogLevel::Client,
                LogLevel::Server,
            ],
            _ => &[
                LogLevel::All,
                LogLevel::Fatal,
                LogLevel::Warning,
                LogLevel::Notice,
            ],
        }
    }
}

/// Whether a line belongs to the level being filtered for.
fn line_has_level(line: &str, level: LogLevel) -> bool {
    match level {
        LogLevel::All => true,
        LogLevel::Fatal => line.contains("Fatal error") || line.contains("PHP Fatal"),
        LogLevel::Warning => line.contains("Warning"),
        LogLevel::Notice => line.contains("Notice") || line.contains("Deprecated"),
        LogLevel::Client => access_status(line).is_some_and(|status| (400..500).contains(&status)),
        LogLevel::Server => access_status(line).is_some_and(|status| (500..600).contains(&status)),
    }
}

/// The status code of an nginx access line, if it carries one.
///
/// The combined format is `host - - [date] "METHOD /path HTTP/1.1" 200 1234`,
/// so the code is the first field after the closing quote of the request.
fn access_status(line: &str) -> Option<u16> {
    let mut quotes = line.split('"');
    quotes.next()?;
    quotes.next()?;
    quotes
        .next()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// One log line, with every hit on the search term emphasised.
fn log_line(text: &str, needle: &str, case_sensitive: bool, wrap: bool, cx: &App) -> AnyElement {
    let line = div().flex().flex_row().flex_wrap();
    let line = if wrap { line } else { line.whitespace_nowrap() };

    if needle.is_empty() {
        return line.child(text.to_string()).into_any_element();
    }

    let characters: Vec<char> = text.chars().collect();
    let mut segments: Vec<AnyElement> = Vec::new();
    let mut cursor = 0;
    for (start, end) in find_ranges(text, needle, case_sensitive) {
        if start > cursor {
            segments.push(
                div()
                    .child(characters[cursor..start].iter().collect::<String>())
                    .into_any_element(),
            );
        }
        segments.push(
            div()
                .rounded_sm()
                .bg(cx.theme().accent)
                .text_color(cx.theme().primary)
                .child(characters[start..end].iter().collect::<String>())
                .into_any_element(),
        );
        cursor = end;
    }
    if cursor < characters.len() {
        segments.push(
            div()
                .child(characters[cursor..].iter().collect::<String>())
                .into_any_element(),
        );
    }

    line.children(segments).into_any_element()
}

/// Every occurrence of `needle` in `text`, as character ranges.
///
/// Matching is on characters rather than bytes, because lowercasing can change
/// a string's byte length and the ranges have to slice the original text back
/// out for display.
fn find_ranges(text: &str, needle: &str, case_sensitive: bool) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let haystack: Vec<char> = text.chars().collect();
    let pattern: Vec<char> = needle.chars().collect();
    if pattern.is_empty() || pattern.len() > haystack.len() {
        return ranges;
    }

    let mut at = 0;
    while at + pattern.len() <= haystack.len() {
        let hit = (0..pattern.len()).all(|offset| {
            let left = haystack[at + offset];
            let right = pattern[offset];
            if case_sensitive {
                left == right
            } else {
                left.to_lowercase().eq(right.to_lowercase())
            }
        });
        if hit {
            ranges.push((at, at + pattern.len()));
            at += pattern.len();
        } else {
            at += 1;
        }
    }
    ranges
}

/// A certificate's expiry as a person would state it, with the date itself.
fn human_expiry(expires: time::OffsetDateTime) -> String {
    let now = time::OffsetDateTime::now_utc();
    let days = (expires - now).whole_days();
    let date = expires
        .format(time::macros::format_description!("[year]-[month]-[day]"))
        .unwrap_or_default();

    match days {
        d if d < 0 => format!("expired {} days ago ({date})", -d),
        0 => format!("expires today ({date})"),
        1 => format!("expires tomorrow ({date})"),
        d => format!("expires in {d} days ({date})"),
    }
}

/// The `# wisp:start … # wisp:end` block as it is on disk, if there is one.
fn managed_hosts_block() -> Option<String> {
    let text = hosts::read().ok()?;
    let start = text.find(hosts::START)?;
    let rest = &text[start..];
    let end = rest.find(hosts::END)?;
    Some(rest[..end + hosts::END.len()].to_string())
}

/// A short token for a saved icon's filename, so replacing an icon never
/// collides with a cached image under the same name.
fn random_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos() as u64);
    let tick = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}{tick:x}")
}

/// Where a project's chosen icon is kept: with its generated config, under a
/// token name.
fn project_icon_path(slug: &str, token: &str, extension: &str) -> std::path::PathBuf {
    serve::config::site_dir(slug)
        .join(format!("icon-{token}.{extension}"))
}

/// A stable hue for a name, so a project's generated icon keeps its colour.
fn stable_hue(name: &str) -> f32 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (hash % 360) as f32 / 360.0
}

/// The generated icon: a squircle in a colour derived from the name, carrying
/// the name's first letter.
///
/// Used whenever a project has not chosen an image, which is why adding the
/// feature needs nothing written for projects that already exist: their icon is
/// drawn from what is already known about them.
fn generated_icon(name: &str, size: Pixels) -> AnyElement {
    let side = f32::from(size);
    let letter = name
        .trim()
        .chars()
        .next()
        .map(|first| first.to_uppercase().to_string())
        .unwrap_or_else(|| "?".to_string());
    let hue = stable_hue(name);

    div()
        .size(size)
        .flex_shrink_0()
        .rounded(px(side * 0.3))
        .bg(hsla(hue, 0.45, 0.42, 1.0))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(side * 0.5))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(hsla(hue, 0.25, 0.97, 1.0))
        .child(letter)
        .into_any_element()
}

/// A project's icon: the image it chose, when that file is still there, and the
/// generated squircle otherwise.
fn project_icon(project: &Project, size: Pixels) -> AnyElement {
    let chosen = project
        .icon_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .filter(|path| std::path::Path::new(path).is_file());

    match chosen {
        Some(path) => {
            let path = SharedString::from(path.to_string());
            div()
                .size(size)
                .flex_shrink_0()
                .rounded(px(f32::from(size) * 0.3))
                .overflow_hidden()
                .child(img(path).size(size))
                .into_any_element()
        }
        None => generated_icon(&project.name, size),
    }
}

/// The error colour, for the few places a raw token is right.
fn cx_error() -> Hsla {    rgb(0xEF4444).into()
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
    use super::{
        access_status, descends_from, diff_lines, domain_is_hand_edited, find_ranges, is_newer,
        line_has_level, measure_services, merge_samples, push_capped, service_config_changed,
        should_offer_update, DiffRow, LogKind, LogLevel, ServiceSample, REPOSITORY_URL,
    };
    use crate::update::{REPO_NAME, REPO_OWNER};
    use std::collections::{HashMap, VecDeque};

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
            icon_path: None,
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

    // ---- the resource sampler -------------------------------------------

    #[test]
    fn a_capped_history_drops_the_oldest_sample() {
        let mut history = VecDeque::new();
        for value in 0..super::METRIC_SAMPLES + 10 {
            push_capped(&mut history, value as f64);
        }
        assert_eq!(history.len(), super::METRIC_SAMPLES);
        // The first ten were pushed out, so the window now starts at 10.
        assert_eq!(history.front().copied(), Some(10.0));
    }

    #[test]
    fn a_process_descends_from_itself_and_its_ancestors() {
        // 3 -> 2 -> 1, and 9 standing on its own.
        let table: HashMap<u32, u32> = [(2_u32, 1_u32), (3, 2)].into_iter().collect();
        let parent_of = |pid: sysinfo::Pid| {
            table.get(&pid.as_u32()).copied().map(sysinfo::Pid::from_u32)
        };

        assert!(descends_from(&parent_of, pid(1), pid(1)));
        assert!(descends_from(&parent_of, pid(3), pid(1)));
        assert!(descends_from(&parent_of, pid(2), pid(1)));
        assert!(!descends_from(&parent_of, pid(9), pid(1)));
        assert!(!descends_from(&parent_of, pid(1), pid(3)));
    }

    #[test]
    fn a_cycle_in_the_process_table_does_not_hang_the_walk() {
        let table: HashMap<u32, u32> = [(1_u32, 2_u32), (2, 1)].into_iter().collect();
        let parent_of = |pid: sysinfo::Pid| {
            table.get(&pid.as_u32()).copied().map(sysinfo::Pid::from_u32)
        };

        assert!(!descends_from(&parent_of, pid(1), pid(7)));
    }

    /// The measurement end to end: our own test process is a real one, so the
    /// sampler should find it and attribute memory to it.
    #[test]
    fn measuring_a_real_process_reports_its_memory() {
        let mut system = sysinfo::System::new();
        system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

        let tracked = vec![crate::serve::Tracked {
            project: Some(7),
            pid: std::process::id(),
        }];
        let measured = measure_services(&system, &tracked);

        assert_eq!(measured.len(), 1);
        let (project, cpu, ram) = measured[0];
        assert_eq!(project, Some(7));
        assert!(ram > 0, "our own process should have some memory");
        assert!(cpu >= 0.0);
    }

    /// A pid that is not running costs nothing, rather than panicking.
    #[test]
    fn measuring_a_dead_process_reports_nothing() {
        let mut system = sysinfo::System::new();
        system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

        // PIDs are 32-bit on Windows but a very high one is never live here.
        let tracked = vec![crate::serve::Tracked {
            project: None,
            pid: u32::MAX - 3,
        }];
        let measured = measure_services(&system, &tracked);

        assert_eq!(measured, vec![(None, 0.0, 0)]);
    }

    /// A service that keeps running keeps its sparkline history; one that went
    /// away does not.
    #[test]
    fn history_follows_the_service_that_is_still_running() {
        let mut history = VecDeque::new();
        push_capped(&mut history, 4.0);
        let existing = ServiceSample {
            project: Some(1),
            cpu: 4.0,
            ram_bytes: 1024,
            cpu_history: history.clone(),
            ram_history: history,
        };
        let mut kept = vec![existing];
        merge_samples(
            &mut kept,
            vec![(Some(1), 9.0, 2048), (None, 1.0, 512)],
        );

        assert_eq!(kept.len(), 2);
        let project = &kept[0];
        assert_eq!(project.cpu, 9.0);
        assert_eq!(project.cpu_history.len(), 2, "the old sample is kept");
        assert_eq!(project.cpu_history.front().copied(), Some(4.0));
        // nginx appeared for the first time, so it has a single point.
        assert_eq!(kept[1].cpu_history.len(), 1);
    }

    fn pid(value: u32) -> sysinfo::Pid {
        sysinfo::Pid::from_u32(value)
    }

    // ---- log search ------------------------------------------------------

    #[test]
    fn a_search_term_is_located_wherever_it_appears() {
        assert_eq!(find_ranges("nginx error nginx", "nginx", true), vec![(0, 5), (12, 17)]);
        assert_eq!(find_ranges("nothing here", "nginx", true), vec![]);
        assert_eq!(find_ranges("", "nginx", true), vec![]);
        assert_eq!(find_ranges("nginx", "", true), vec![]);
    }

    #[test]
    fn an_insensitive_search_matches_but_keeps_the_original_spelling() {
        let text = "PHP Fatal error: Uncaught Error";
        let ranges = find_ranges(text, "error", false);
        assert_eq!(ranges.len(), 2);

        // The ranges slice the original text, so the casing survives.
        let characters: Vec<char> = text.chars().collect();
        let first: String = characters[ranges[0].0..ranges[0].1].iter().collect();
        assert_eq!(first, "error");
        let second: String = characters[ranges[1].0..ranges[1].1].iter().collect();
        assert_eq!(second, "Error");
    }

    #[test]
    fn a_case_sensitive_search_skips_a_different_casing() {
        assert_eq!(find_ranges("Error error", "error", true), vec![(6, 11)]);
        assert_eq!(find_ranges("Error error", "error", false).len(), 2);
    }

    #[test]
    fn a_pattern_longer_than_the_line_is_not_a_match() {
        assert_eq!(find_ranges("err", "error", true), vec![]);
        assert_eq!(find_ranges("erro", "error", true), vec![]);
        // A partial tail must not match either.
        assert_eq!(find_ranges("xerr", "error", true), vec![]);
    }

    // ---- the php.ini diff ------------------------------------------------

    #[test]
    fn an_unchanged_file_has_nothing_to_show() {
        let text = "a\nb\nc";
        assert!(diff_lines(text, text, 3).is_empty());
    }

    #[test]
    fn appended_overrides_show_as_added_lines() {
        let rows = diff_lines("a\nb\nc", "a\nb\nc\nupload_max_filesize = 128M", 3);
        let added: Vec<&String> = rows
            .iter()
            .filter_map(|row| match row {
                DiffRow::Added(text) => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(added, vec!["upload_max_filesize = 128M"]);
        // Nothing was taken away, so nothing is reported as a removal.
        assert!(!rows.iter().any(|row| matches!(row, DiffRow::Removed(_))));
    }

    #[test]
    fn long_runs_are_collapsed_but_the_change_stays_visible() {
        let base: String = (0..100).map(|n| format!("line {n}\n")).collect();
        let other = format!("{base}max_execution_time = 300");

        let rows = diff_lines(&base, &other, 2);
        let skipped: usize = rows
            .iter()
            .filter_map(|row| match row {
                DiffRow::Skipped(count) => Some(*count),
                _ => None,
            })
            .sum();
        assert!(skipped >= 90, "the unchanged bulk should collapse: {skipped}");

        let context = rows
            .iter()
            .filter(|row| matches!(row, DiffRow::Same(_)))
            .count();
        assert_eq!(context, 2, "only the configured context is kept");
    }

    #[test]
    fn a_removed_tail_is_reported_as_removed() {
        let rows = diff_lines("a\nb\ngone", "a\nb", 3);
        let removed: Vec<&String> = rows
            .iter()
            .filter_map(|row| match row {
                DiffRow::Removed(text) => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(removed, vec!["gone"]);
    }

    // ---- log levels ------------------------------------------------------

    #[test]
    fn an_access_line_reports_the_status_it_answered() {
        let line = r#"127.0.0.1 - - [16/Oct/2026:04:05:25 +0000] "GET /dashboard HTTP/1.1" 404 153 "-" "curl/8.4.0""#;
        assert_eq!(access_status(line), Some(404));
        assert!(line_has_level(line, LogLevel::Client));
        assert!(!line_has_level(line, LogLevel::Server));

        let failure = line.replace(" 404 ", " 502 ");
        assert_eq!(access_status(&failure), Some(502));
        assert!(line_has_level(&failure, LogLevel::Server));
    }

    #[test]
    fn a_line_that_is_not_an_access_line_has_no_status() {
        assert_eq!(access_status("PHP Fatal error: something"), None);
        assert_eq!(access_status(""), None);
        // A quote with nothing after it must not be read as a code.
        assert_eq!(access_status(r#""GET / HTTP/1.1""#), None);
    }

    #[test]
    fn php_levels_are_recognised_from_the_text() {
        assert!(line_has_level(
            "[16-Oct-2026 04:05:25 UTC] PHP Fatal error:  Uncaught Error: Class \"NumberFormatter\" not found",
            LogLevel::Fatal
        ));
        assert!(line_has_level(
            "PHP Warning:  Undefined array key 1 in index.php",
            LogLevel::Warning
        ));
        assert!(line_has_level(
            "PHP Deprecated:  strlen(): Passing null is deprecated",
            LogLevel::Notice
        ));
        assert!(!line_has_level("PHP Notice: something", LogLevel::Fatal));
        assert!(line_has_level("anything at all", LogLevel::All));
    }

    #[test]
    fn each_source_offers_the_filters_that_make_sense_for_it() {
        assert_eq!(
            LogLevel::for_kind(LogKind::Access),
            &[LogLevel::All, LogLevel::Client, LogLevel::Server]
        );
        assert_eq!(
            LogLevel::for_kind(LogKind::Php),
            &[
                LogLevel::All,
                LogLevel::Fatal,
                LogLevel::Warning,
                LogLevel::Notice
            ]
        );
    }
}
