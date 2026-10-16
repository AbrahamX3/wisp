//! Installed PHP and nginx runtimes.

use super::{now_unix, Result, Store};

/// A runtime Wisp has downloaded and unpacked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledRuntime {
    /// `"php"` or `"nginx"`.
    pub kind: String,
    /// The exact version, e.g. `8.3.35` or `1.27.0`.
    pub version: String,
    /// The install directory, a nginx prefix or the PHP binary's folder.
    pub path: String,
    pub installed_at: i64,
}

impl InstalledRuntime {
    /// The `major.minor` line this patch belongs to (`8.3.35` → `8.3`).
    pub fn line(&self) -> String {
        minor_line(&self.version)
    }
}

impl Store {
    /// Every installed runtime, newest first within each kind.
    pub fn list_runtimes(&self) -> Result<Vec<InstalledRuntime>> {
        let conn = self.connect()?;
        let mut statement = conn.prepare(
            "SELECT kind, version, path, installed_at
             FROM runtimes ORDER BY kind, installed_at DESC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(InstalledRuntime {
                kind: row.get("kind")?,
                version: row.get("version")?,
                path: row.get("path")?,
                installed_at: row.get("installed_at")?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Record a runtime as installed (idempotent).
    pub fn add_runtime(&self, kind: &str, version: &str, path: &str) -> Result<()> {
        self.connect()?.execute(
            "INSERT OR REPLACE INTO runtimes (kind, version, path, installed_at)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![kind, version, path, now_unix()],
        )?;
        Ok(())
    }

    /// Forget a runtime (the caller removes its files).
    pub fn remove_runtime(&self, kind: &str, version: &str) -> Result<()> {
        self.connect()?.execute(
            "DELETE FROM runtimes WHERE kind = ?1 AND version = ?2",
            rusqlite::params![kind, version],
        )?;
        Ok(())
    }
}

/// The `major.minor` line of a dotted version, e.g. `8.3.35` → `8.3`.
pub fn minor_line(version: &str) -> String {
    let mut parts = version.split('.');
    match (parts.next(), parts.next()) {
        (Some(major), Some(minor)) => format!("{major}.{minor}"),
        _ => version.to_string(),
    }
}
