//! SQLite storage.
//!
//! A thin repository layer over `rusqlite`, not an ORM: statements are written
//! by hand and rows are mapped to plain structs. The schema is tiny and local,
//! so a connection is opened per operation, which keeps `Store` free of thread
//! affinity and cheap to hold in the UI.

mod projects;
mod runtimes;

pub use projects::{slugify, split_domain, Project, ProjectDraft, DOMAIN_SUFFIXES};
pub use runtimes::{minor_line, InstalledRuntime};

use std::path::PathBuf;

use rusqlite::Connection;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Sqlite(#[from] rusqlite::Error),

    /// A user-facing validation problem, shown verbatim in the editor.
    #[error("{0}")]
    Validation(String),

    #[error("another project already uses \"{0}\"")]
    DomainTaken(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// The schema. `IF NOT EXISTS` keeps this idempotent; a real migration runner
/// arrives when the first breaking change does.
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS projects (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    name               TEXT    NOT NULL,
    app_name           TEXT    NOT NULL,
    root_path          TEXT    NOT NULL,
    domain             TEXT    NOT NULL UNIQUE,
    php_version        TEXT,
    ssl_enabled        INTEGER NOT NULL DEFAULT 1,
    php_ini_overrides  TEXT    NOT NULL DEFAULT '{}',
    created_at         INTEGER NOT NULL,
    updated_at         INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS runtimes (
    kind         TEXT    NOT NULL,
    version      TEXT    NOT NULL,
    path         TEXT    NOT NULL,
    installed_at INTEGER NOT NULL,
    PRIMARY KEY (kind, version)
);
"#;

/// Handle to the on-disk database.
#[derive(Debug, Clone)]
pub struct Store {
    path: PathBuf,
}

impl Store {
    /// Open (creating if needed) the database under the config directory.
    pub fn open() -> Result<Self> {
        crate::config::ensure();
        let store = Self {
            path: crate::config::database(),
        };
        store.migrate()?;
        Ok(store)
    }

    fn connect(&self) -> Result<Connection> {
        Ok(Connection::open(&self.path)?)
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.connect()?;
        conn.execute_batch(SCHEMA)?;
        // Older records stored a placeholder empty object for "no overrides",
        // which would be appended to php.ini as `{}` and break it.
        conn.execute(
            "UPDATE projects SET php_ini_overrides = '' WHERE TRIM(php_ini_overrides) = '{}'",
            [],
        )?;
        Ok(())
    }
}

/// Seconds since the Unix epoch, used for created/updated stamps.
pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}
