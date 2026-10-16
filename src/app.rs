//! The Wisp window.
//!
//! Layout: a slim sidebar of projects beside a detail pane. Every card is the
//! shared "tray and plate" primitive, the brand accent marks the active project
//! and the primary action, and nothing else carries colour.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::base::Disableable as _;
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
use crate::db::{self, InstalledRuntime, Project, ProjectDraft, Store};
use crate::elevation::{self, ActionOutcome, ControlMode};
use crate::helper_task::{self, TaskError};
use crate::hosts;
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
}

/// Process-wide owner of nginx and the PHP workers, so the app-quit hook can
/// stop them, and the background sync can move it in and out.
pub struct ServerGlobal(pub serve::Supervisor);

impl Global for ServerGlobal {}

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

    /// Projects the user has asked to serve.
    running: HashSet<i64>,
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
            screen: Screen::Projects,
            runtimes: Vec::new(),
            catalog: None,
            catalog_loading: false,
            catalog_error: None,
            catalog_fetched_at: None,
            busy_runtime: None,
            running: HashSet::new(),
            serve_status: serve::Status::default(),
            serve_error: None,
            serve_busy: false,
            trust_busy: false,
            mode: ControlMode::Uac,
            elevation_busy: false,
            hosts_written: Vec::new(),
            pending_root: None,
        };
        this.reload(cx);
        // Show the first project on launch rather than an empty pane.
        if this.selected.is_none() {
            this.selected = this.projects.first().map(|project| project.id);
        }
        this.reload_runtimes(cx);
        this.load_catalog(cx);
        Self::spawn_status_poll(cx);
        Self::spawn_mode_probe(cx);

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
            error: None,
            _subscriptions: vec![app_sub, domain_sub],
        });

        let entity = cx.entity();
        window.open_dialog(cx, move |dialog, _window, cx| {
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
                .child(editor_form(editor, &entity, error, &php_versions, cx))
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

    /// The specs for the projects the user asked to run.
    fn site_specs(&self) -> std::result::Result<Vec<SiteSpec>, String> {
        let mut specs = Vec::new();
        for project in &self.projects {
            if !self.running.contains(&project.id) {
                continue;
            }
            let Some(php) = project.php_version.clone().filter(|value| !value.is_empty()) else {
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
            specs.push(SiteSpec {
                id: project.id,
                slug: project.slug(),
                domain: project.domain.clone(),
                root: PathBuf::from(&project.root_path),
                php_version: php,
                ssl_enabled: project.ssl_enabled,
            });
        }
        Ok(specs)
    }

    fn toggle_project_running(&mut self, id: i64, cx: &mut Context<Self>) {
        if !self.running.remove(&id) {
            self.running.insert(id);
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
            .filter(|project| self.running.contains(&project.id))
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

    // ---- service controls ------------------------------------------------

    fn refresh_serve_status(&mut self, cx: &mut Context<Self>) {
        self.serve_status = cx.global_mut::<ServerGlobal>().0.status();
        cx.notify();
    }

    /// Start (or re-apply) the whole server: nginx plus the workers.
    fn start_nginx(&mut self, cx: &mut Context<Self>) {
        self.sync_server(cx);
    }

    fn stop_nginx(&mut self, cx: &mut Context<Self>) {
        if self.serve_busy {
            return;
        }
        cx.global_mut::<ServerGlobal>().0.stop_nginx_only();
        self.refresh_serve_status(cx);
    }

    fn restart_nginx(&mut self, cx: &mut Context<Self>) {
        if self.serve_busy {
            return;
        }
        cx.global_mut::<ServerGlobal>().0.stop_nginx_only();
        self.sync_server(cx);
    }

    /// Serve every project that has a PHP version set.
    fn start_php(&mut self, cx: &mut Context<Self>) {
        self.running = self
            .projects
            .iter()
            .filter(|project| project.php_version.is_some())
            .map(|project| project.id)
            .collect();
        self.sync_server(cx);
        self.sync_hosts(cx);
    }

    fn stop_php(&mut self, cx: &mut Context<Self>) {
        if self.serve_busy {
            return;
        }
        cx.global_mut::<ServerGlobal>().0.stop_workers();
        self.refresh_serve_status(cx);
    }

    fn restart_php(&mut self, cx: &mut Context<Self>) {
        if self.serve_busy {
            return;
        }
        self.force_worker_restart();
        self.sync_server(cx);
    }

    /// Start everything: nginx and a worker for every project with PHP.
    fn start_all(&mut self, cx: &mut Context<Self>) {
        self.start_php(cx);
    }

    /// Stop everything: every worker and nginx.
    fn stop_all(&mut self, cx: &mut Context<Self>) {
        if self.serve_busy {
            return;
        }
        self.running.clear();
        {
            let server = cx.global_mut::<ServerGlobal>();
            server.0.stop_workers();
            server.0.stop_nginx_only();
        }
        self.hosts_written.clear();
        self.sync_hosts(cx);
        self.refresh_serve_status(cx);
    }

    fn restart_all(&mut self, cx: &mut Context<Self>) {
        self.stop_all(cx);
        self.start_php(cx);
    }

    /// Restart one project's worker (used by its Restart button).
    fn restart_project(&mut self, id: i64, cx: &mut Context<Self>) {
        if self.serve_busy {
            return;
        }
        if let Some(project) = self.projects.iter().find(|project| project.id == id) {
            let _ = std::fs::remove_file(serve::config::site_dir(&project.slug()).join("php.ini"));
        }
        self.sync_server(cx);
    }

    /// Delete every project's `php.ini` so the next sync restarts each worker.
    fn force_worker_restart(&mut self) {
        for project in &self.projects {
            let _ = std::fs::remove_file(serve::config::site_dir(&project.slug()).join("php.ini"));
        }
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
                    .child(if self.running.is_empty() {
                        div().into_any_element()
                    } else {
                        Button::new("stop-all")
                            .danger()
                            .small()
                            .icon(icon(IconName::Square))
                            .label("Stop all")
                            .on_click(
                                cx.listener(|this, _, _window, cx| this.stop_all(cx)),
                            )
                            .into_any_element()
                    })
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
        let mode = self.mode;
        let elevation_busy = self.elevation_busy;

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
                    .check_side(Side::Right)
                    .item(
                        PopupMenuItem::new("No-prompt hosting control")
                            .icon(icon(IconName::ShieldCheck))
                            .checked(mode == ControlMode::Task)
                            .disabled(elevation_busy || mode == ControlMode::Elevated)
                            .on_click({
                                let entity = entity.clone();
                                move |_, _, cx| {
                                    entity.update(cx, |this, cx| {
                                        if mode == ControlMode::Task {
                                            this.remove_elevated(cx);
                                        } else {
                                            this.setup_elevated(cx);
                                        }
                                    });
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

        let nav = div()
            .flex()
            .flex_col()
            .gap_1()
            .p(px(10.))
            .child(nav_item(
                "Projects",
                IconName::FolderTree,
                self.screen == Screen::Projects,
                Screen::Projects,
                self.projects.len(),
                cx,
            ))
            .child(nav_item(
                "Services",
                IconName::Server,
                self.screen == Screen::Services,
                Screen::Services,
                self.runtimes.len(),
                cx,
            ));

        let body = if self.screen == Screen::Projects {
            if self.projects.is_empty() {
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
            }
        } else {
            div().into_any_element()
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
            .child(nav)
            .child(div().h(px(1.)).w_full().bg(cx.theme().border))
            .child(div().flex_1().overflow_hidden().child(body))
    }

    fn content(&self, cx: &mut Context<Self>) -> AnyElement {
        match self.screen {
            Screen::Projects => self.projects_content(cx),
            Screen::Services => self.services_content(cx),
        }
    }

    fn projects_content(&self, cx: &mut Context<Self>) -> AnyElement {
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
                            .danger()
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

        let is_running = self.serve_status.running_projects.contains(&id);
        let nginx_up = self.serve_status.nginx_running;
        let dot_color = if is_running {
            cx.theme().success
        } else {
            cx.theme().muted_foreground
        };
        let status_text = if is_running { "Running" } else { "Stopped" };
        let subtitle = if is_running {
            if nginx_up {
                "nginx is serving this domain over HTTP."
            } else {
                "Starting\u{2026}"
            }
        } else {
            "Start to serve this project on its domain."
        };

        let serve_actions: AnyElement = if self.serve_busy {
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
                        .child("Applying\u{2026}"),
                )
                .into_any_element()
        } else {
            let mut buttons: Vec<AnyElement> = Vec::new();
            if is_running {
                buttons.push(
                    Button::new("stop-project")
                        .danger()
                        .small()
                        .icon(icon(IconName::Square))
                        .label("Stop")
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.toggle_project_running(id, cx)
                        }))
                        .into_any_element(),
                );
                buttons.push(
                    Button::new("restart-project")
                        .outline()
                        .small()
                        .icon(icon(IconName::RotateCcw))
                        .label("Restart")
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.restart_project(id, cx)
                        }))
                        .into_any_element(),
                );
            } else {
                buttons.push(
                    Button::new("start-project")
                        .primary()
                        .small()
                        .icon(icon(IconName::Play))
                        .label("Start")
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.toggle_project_running(id, cx)
                        }))
                        .into_any_element(),
                );
            }
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .children(buttons)
                .into_any_element()
        };

        let services = ui::plate(
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
                        .child(ui::dot(dot_color))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(status_text),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(subtitle),
                                ),
                        ),
                )
                .child(serve_actions),
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

    // ---- services --------------------------------------------------------

    fn services_content(&self, cx: &mut Context<Self>) -> AnyElement {
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

        column = column.child(self.services_card(cx));
        column = column.child(self.https_card(cx));
        column = column.child(self.php_section(cx));
        column = column.child(self.nginx_section(cx));

        div()
            .id("services")
            .flex()
            .flex_col()
            .size_full()
            .p(px(28.))
            .overflow_y_scroll()
            .child(column)
            .into_any_element()
    }

    /// nginx, PHP and "all services" controls for the Services page.
    fn services_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let nginx_up = self.serve_status.nginx_running;
        let workers = self.serve_status.running_projects.len();
        let busy = self.serve_busy;

        let header = section_header(
            "Services",
            "The whole server. Each project also has its own Start, Stop and Restart.",
            cx,
        );
        let rows = div()
            .flex()
            .flex_col()
            .p(px(8.))
            .child(service_row(
                "nginx",
                if nginx_up { "Running" } else { "Stopped" },
                nginx_up,
                Service::Nginx,
                busy,
                cx,
            ))
            .child(service_row(
                "PHP",
                &format!("{workers} running"),
                workers > 0,
                Service::Php,
                busy,
                cx,
            ))
            .child(all_services_row(busy, cx));

        ui::plate(cx, div().flex().flex_col().gap_1().child(header).child(rows))
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

        ui::plate(
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
                        .child(icon(IconName::ShieldCheck).text_color(brand::accent()))
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
        ui::plate(
            cx,
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_4()
                .p(px(18.))
                .child(
                    icon(IconName::TriangleAlert)
                        .text_color(brand::accent()),
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

        ui::plate(
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

        ui::plate(
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

/// Which service a control row (or its buttons) refer to.
#[derive(Clone, Copy)]
enum Service {
    Nginx,
    Php,
}

/// A service row: status dot, name, and Start / Stop / Restart.
fn service_row(
    label: &str,
    status: &str,
    running: bool,
    service: Service,
    busy: bool,
    cx: &mut Context<Wisp>,
) -> impl IntoElement {
    let dot_color = if running {
        cx.theme().success
    } else {
        cx.theme().muted_foreground
    };

    let mut actions: Vec<AnyElement> = Vec::new();
    if running {
        actions.push(
            Button::new(format!("service-stop-{label}"))
                .danger()
                .small()
                .icon(icon(IconName::Square))
                .label("Stop")
                .disabled(busy)
                .on_click(cx.listener(move |this, _, _window, cx| match service {
                    Service::Nginx => this.stop_nginx(cx),
                    Service::Php => this.stop_php(cx),
                }))
                .into_any_element(),
        );
    } else {
        actions.push(
            Button::new(format!("service-start-{label}"))
                .primary()
                .small()
                .icon(icon(IconName::Play))
                .label("Start")
                .disabled(busy)
                .on_click(cx.listener(move |this, _, _window, cx| match service {
                    Service::Nginx => this.start_nginx(cx),
                    Service::Php => this.start_php(cx),
                }))
                .into_any_element(),
        );
    }
    actions.push(
        Button::new(format!("service-restart-{label}"))
            .outline()
            .small()
            .icon(icon(IconName::RotateCcw))
            .label("Restart")
            .disabled(busy)
            .on_click(cx.listener(move |this, _, _window, cx| match service {
                Service::Nginx => this.restart_nginx(cx),
                Service::Php => this.restart_php(cx),
            }))
            .into_any_element(),
    );

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
                .flex_row()
                .items_center()
                .gap_3()
                .child(ui::dot(dot_color))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .child(label.to_string()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(status.to_string()),
                        ),
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
}

///Start / Restart / Stop across nginx and every PHP worker.
fn all_services_row(busy: bool, cx: &mut Context<Wisp>) -> impl IntoElement {
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
                .child("All services"),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(
                    Button::new("all-start")
                        .primary()
                        .small()
                        .icon(icon(IconName::Play))
                        .label("Start all")
                        .disabled(busy)
                        .on_click(cx.listener(|this, _, _window, cx| this.start_all(cx))),
                )
                .child(
                    Button::new("all-restart")
                        .outline()
                        .small()
                        .icon(icon(IconName::RotateCcw))
                        .label("Restart all")
                        .disabled(busy)
                        .on_click(cx.listener(|this, _, _window, cx| this.restart_all(cx))),
                )
                .child(
                    Button::new("all-stop")
                        .danger()
                        .small()
                        .icon(icon(IconName::Square))
                        .label("Stop all")
                        .disabled(busy)
                        .on_click(cx.listener(|this, _, _window, cx| this.stop_all(cx))),
                ),
        )
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
        base.bg(brand::accent_wash())
            .border_1()
            .border_color(brand::accent_tint())
    } else {
        base
    };

    styled
        .id(label)
        .hover(|d| d.bg(cx.theme().list_hover))
        .on_click(cx.listener(move |this, _, _window, cx| this.goto(screen, cx)))
        .child(icon(icon_name).small().text_color(if active {
            brand::accent()
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
    let color = brand::accent();
    let _ = cx;
    div()
        .px(px(8.))
        .py(px(3.))
        .rounded_full()
        .bg(brand::accent_wash())
        .border_1()
        .border_color(brand::accent_tint())
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

/// The body of the create/edit dialog.
fn editor_form(
    editor: &EditorState,
    entity: &Entity<Wisp>,
    error: Option<String>,
    php_versions: &[String],
    cx: &App,
) -> impl IntoElement {
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
                .description("Point at the project root; public/ is served when it exists.")
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
    brand::apply(cx);
}

#[cfg(test)]
mod tests {
    use super::{domain_is_hand_edited, is_newer, should_offer_update, REPOSITORY_URL};
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
}
