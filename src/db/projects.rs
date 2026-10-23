//! Project records and the SQL statements that manage them.

use rusqlite::Row;

use super::{now_unix, Error, Result, Store};

/// A project as stored. `Delete` removes the row and everything Wisp generated
/// for it, never `root_path`.
#[derive(Debug, Clone)]
pub struct Project {
    pub id: i64,
    pub name: String,
    /// Slug used to derive the default domain and the generated config folder.
    pub app_name: String,
    /// The project's document root on disk.
    pub root_path: String,
    /// The `.test` domain this project answers on.
    pub domain: String,
    /// The PHP version the project runs, once runtimes are managed.
    pub php_version: Option<String>,
    pub ssl_enabled: bool,
    /// Per-project `php.ini` overrides, as a JSON object string.
    pub php_ini_overrides: String,
    /// The icon this project chose, if any. Empty means the generated
    /// squircle is used, so existing projects need nothing written for them.
    pub icon_path: Option<String>,
    /// Record timestamps, surfaced once the detail view grows a history.
    #[allow(dead_code)]
    pub created_at: i64,
    #[allow(dead_code)]
    pub updated_at: i64,
}

impl Project {
    /// The folder name Wisp uses under `sites/` for this project's config.
    pub fn slug(&self) -> String {
        if self.app_name.is_empty() {
            format!("site-{}", self.id)
        } else {
            self.app_name.clone()
        }
    }
}

/// The editable fields of a project, as typed in the editor.
#[derive(Debug, Clone, Default)]
pub struct ProjectDraft {
    pub name: String,
    pub app_name: String,
    pub root_path: String,
    pub domain: String,
    pub php_version: Option<String>,
    pub ssl_enabled: bool,
    pub php_ini_overrides: String,
    /// The chosen icon, carried through the form.
    pub icon_path: Option<String>,
}

impl ProjectDraft {
    /// From an existing project, for the edit form.
    pub fn from_project(project: &Project) -> Self {
        Self {
            icon_path: project.icon_path.clone(),
            name: project.name.clone(),
            app_name: project.app_name.clone(),
            root_path: project.root_path.clone(),
            domain: project.domain.clone(),
            php_version: project.php_version.clone(),
            ssl_enabled: project.ssl_enabled,
            php_ini_overrides: if project.php_ini_overrides.trim() == "{}" {
                String::new()
            } else {
                project.php_ini_overrides.clone()
            },
        }
    }

    /// Normalise and validate, returning the corrected draft.
    fn validated(mut self) -> Result<Self> {
        self.name = self.name.trim().to_string();
        self.root_path = self.root_path.trim().to_string();
        self.domain = self.domain.trim().to_lowercase();
        if self.app_name.trim().is_empty() {
            self.app_name = slugify(&self.name);
        }
        self.app_name = slugify(&self.app_name);
        self.php_ini_overrides = self.php_ini_overrides.trim().to_string();
        // Older records stored an empty JSON object; treat it as "none".
        if self.php_ini_overrides == "{}" {
            self.php_ini_overrides.clear();
        }

        if self.name.is_empty() {
            return Err(Error::Validation("Give the project a name.".into()));
        }
        if self.app_name.is_empty() {
            return Err(Error::Validation(
                "The app name needs at least one letter or number.".into(),
            ));
        }
        if self.root_path.is_empty() {
            return Err(Error::Validation(
                "Choose the project's root folder.".into(),
            ));
        }
        if let Err(message) = validate_domain(&self.domain) {
            return Err(Error::Validation(message));
        }
        Ok(self)
    }
}

impl Store {
    /// Every project, alphabetically.
    pub fn list_projects(&self) -> Result<Vec<Project>> {
        let conn = self.connect()?;
        let mut statement = conn.prepare(
            "SELECT id, name, app_name, root_path, domain, php_version,
                    ssl_enabled, php_ini_overrides, created_at, updated_at, icon_path
             FROM projects ORDER BY name COLLATE NOCASE",
        )?;
        let rows = statement.query_map([], row_to_project)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Insert a project and return its id.
    pub fn create_project(&self, draft: &ProjectDraft) -> Result<i64> {
        let draft = draft.clone().validated()?;
        if self.domain_taken(&draft.domain, None)? {
            return Err(Error::DomainTaken(draft.domain));
        }
        let now = now_unix();
        let conn = self.connect()?;
        conn.execute(
            "INSERT INTO projects
                (name, app_name, root_path, domain, php_version,
                 ssl_enabled, php_ini_overrides, icon_path, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?9, ?8, ?8)",
            rusqlite::params![
                draft.name,
                draft.app_name,
                draft.root_path,
                draft.domain,
                draft.php_version,
                i64::from(draft.ssl_enabled),
                draft.php_ini_overrides,
                now,
                draft.icon_path,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Update an existing project from a draft.
    pub fn update_project(&self, id: i64, draft: &ProjectDraft) -> Result<()> {
        let draft = draft.clone().validated()?;
        if self.domain_taken(&draft.domain, Some(id))? {
            return Err(Error::DomainTaken(draft.domain));
        }
        let conn = self.connect()?;
        conn.execute(
            "UPDATE projects SET
                name = ?2, app_name = ?3, root_path = ?4, domain = ?5,
                php_version = ?6, ssl_enabled = ?7, php_ini_overrides = ?8,
                icon_path = ?10, updated_at = ?9
             WHERE id = ?1",
            rusqlite::params![
                id,
                draft.name,
                draft.app_name,
                draft.root_path,
                draft.domain,
                draft.php_version,
                i64::from(draft.ssl_enabled),
                draft.php_ini_overrides,
                now_unix(),
                draft.icon_path,
            ],
        )?;
        Ok(())
    }

    /// Remove a project. Only the record; the files on disk are untouched.
    pub fn delete_project(&self, id: i64) -> Result<()> {
        self.connect()?
            .execute("DELETE FROM projects WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Point every project on the old PHP patch at the new one, after an update.
    pub fn repoint_php(&self, from: &str, to: &str) -> Result<()> {
        self.connect()?.execute(
            "UPDATE projects SET php_version = ?2, updated_at = ?3 WHERE php_version = ?1",
            rusqlite::params![from, to, now_unix()],
        )?;
        Ok(())
    }

    /// Whether another project already claims this domain.
    pub fn domain_taken(&self, domain: &str, except_id: Option<i64>) -> Result<bool> {
        let conn = self.connect()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM projects WHERE domain = ?1 AND id IS NOT ?2",
            rusqlite::params![domain, except_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }
}

fn row_to_project(row: &Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: row.get("id")?,
        name: row.get("name")?,
        app_name: row.get("app_name")?,
        root_path: row.get("root_path")?,
        domain: row.get("domain")?,
        php_version: row.get("php_version")?,
        ssl_enabled: row.get::<_, i64>("ssl_enabled")? != 0,
        php_ini_overrides: row.get("php_ini_overrides")?,
        icon_path: row.get("icon_path")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

/// Turn arbitrary text into a URL-safe slug (`My Shop` → `my-shop`).
pub fn slugify(input: &str) -> String {
    let mut out = String::new();
    let mut pending_dash = false;
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            pending_dash = true;
        }
    }
    out
}

/// The domain suffixes Wisp offers, shown in the editor's suffix select.
///
/// `.test` is reserved by RFC 6761 for testing and is the default. `.localhost`
/// resolves to the loopback interface. `.internal` is ICANN-reserved for private
/// use, and `.local` is common on local networks. All of them are safe from the
/// browser HSTS preload list (unlike `.dev` and `.app`).
pub const DOMAIN_SUFFIXES: &[&str] = &[".test", ".localhost", ".internal", ".local"];

/// Split a stored domain into its editable label and its suffix. An unknown
/// suffix is treated as part of the label, with `.test` as the default.
pub fn split_domain(domain: &str) -> (String, String) {
    let lower = domain.trim().to_lowercase();
    for suffix in DOMAIN_SUFFIXES {
        if let Some(label) = lower.strip_suffix(suffix) {
            return (label.to_string(), (*suffix).to_string());
        }
    }
    (lower, DOMAIN_SUFFIXES[0].to_string())
}

/// Check a domain (label plus one of [`DOMAIN_SUFFIXES`]), returning a message
/// for the editor when it is wrong.
pub fn validate_domain(domain: &str) -> std::result::Result<(), String> {
    let domain = domain.trim();
    if domain.is_empty() {
        return Err("A domain is required, for example myapp.test.".into());
    }
    let lower = domain.to_lowercase();
    let Some(label) = DOMAIN_SUFFIXES
        .iter()
        .find_map(|suffix| lower.strip_suffix(suffix))
    else {
        return Err(format!(
            "The domain must end with one of {}.",
            DOMAIN_SUFFIXES.join(", ")
        ));
    };
    if label.is_empty() {
        return Err("Add a name before the domain suffix.".into());
    }
    for part in label.split('.') {
        if part.is_empty() {
            return Err("The domain has an empty part (a stray dot?).".into());
        }
        if !part
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err("Domains may use letters, numbers, dots and hyphens.".into());
        }
        if part.starts_with('-') || part.ends_with('-') {
            return Err("A domain part cannot start or end with a hyphen.".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{slugify, split_domain, validate_domain};

    #[test]
    fn slugify_lowercases_and_dashes() {
        assert_eq!(slugify("My Shop"), "my-shop");
        assert_eq!(slugify("  API  v2 "), "api-v2");
        assert_eq!(slugify("---"), "");
    }

    #[test]
    fn valid_domains_pass() {
        assert!(validate_domain("myapp.test").is_ok());
        assert!(validate_domain("shop-1.test").is_ok());
        assert!(validate_domain("api.myapp.test").is_ok());
        assert!(validate_domain("MYAPP.test").is_ok());
        assert!(validate_domain("myapp.internal").is_ok());
        assert!(validate_domain("myapp.localhost").is_ok());
    }

    #[test]
    fn unsupported_suffixes_are_rejected() {
        assert!(validate_domain("myapp.dev").is_err());
        assert!(validate_domain("myapp").is_err());
        assert!(validate_domain("").is_err());
        assert!(validate_domain(".test").is_err());
    }

    #[test]
    fn domains_split_into_a_label_and_a_suffix() {
        assert_eq!(split_domain("myapp.test"), ("myapp".into(), ".test".into()));
        assert_eq!(
            split_domain("api.Shop.TEST"),
            ("api.shop".into(), ".test".into())
        );
        // An unknown suffix stays part of the label.
        assert_eq!(
            split_domain("myapp.dev"),
            ("myapp.dev".into(), ".test".into())
        );
    }

    #[test]
    fn malformed_labels_are_rejected() {
        assert!(validate_domain("-bad.test").is_err());
        assert!(validate_domain("bad-.test").is_err());
        assert!(validate_domain("a..b.test").is_err());
        assert!(validate_domain("under_score.test").is_err());
    }
}

#[cfg(test)]
mod store_tests {
    use super::{ProjectDraft, Store};

    fn draft(name: &str, domain: &str) -> ProjectDraft {
        ProjectDraft {
            icon_path: None,
            name: name.to_string(),
            app_name: String::new(),
            root_path: r"C:\Sites\example".to_string(),
            domain: domain.to_string(),
            php_version: Some("8.3".to_string()),
            ssl_enabled: true,
            php_ini_overrides: "{}".to_string(),
        }
    }

    /// One test drives the whole store, because it points `WISP_CONFIG_DIR` at a
    /// private temp directory and environment variables are process-global.
    #[test]
    fn create_update_and_delete_a_project() {
        let _guard = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join("wisp-store-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("WISP_CONFIG_DIR", &dir);

        let store = Store::open().expect("a store should open in a temp dir");
        assert!(store.list_projects().unwrap().is_empty());

        let id = store
            .create_project(&draft("My Shop", "MyShop.test"))
            .expect("create should succeed");

        let projects = store.list_projects().unwrap();
        assert_eq!(projects.len(), 1);
        let project = &projects[0];
        assert_eq!(project.domain, "myshop.test", "domain is normalised");
        assert_eq!(project.app_name, "my-shop", "app name is slugified");

        // A second project cannot claim the same domain.
        let clash = store.create_project(&draft("Other", "myshop.test"));
        assert!(clash.is_err());

        let mut edit = draft("My Shop", "shop.test");
        edit.name = "Renamed".to_string();
        store.update_project(id, &edit).unwrap();
        let projects = store.list_projects().unwrap();
        assert_eq!(projects[0].name, "Renamed");
        assert_eq!(projects[0].domain, "shop.test");

        store.delete_project(id).unwrap();
        assert!(store.list_projects().unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
