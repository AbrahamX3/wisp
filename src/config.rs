//! Where Wisp keeps everything, all under `%USERPROFILE%\.config\wisp`.
//!
//! One root holds the SQLite database, the generated nginx/PHP configuration,
//! the local certificate authority, and the managed nginx/PHP runtimes. Per-user
//! and writable without elevation, which is what keeps the in-app updater and
//! the runtime manager free of prompts.

use std::path::PathBuf;

/// Root of the Wisp data directory.
///
/// `WISP_CONFIG_DIR` overrides it, which the tests and a portable layout use.
pub fn root() -> PathBuf {
    if let Some(dir) = std::env::var_os("WISP_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    default_root()
}

/// The historical per-user root, ignoring `WISP_CONFIG_DIR`.
///
/// Callers that need to tell a normal install from an override (the elevated
/// helper task is named per install) compare `root()` against this.
pub fn default_root() -> PathBuf {
    home().join(".config").join("wisp")
}

/// The SQLite database that holds projects, settings and installed runtimes.
/// Wired up in the project phase.
#[allow(dead_code)]
pub fn database() -> PathBuf {
    root().join("wisp.db")
}

/// Download-able nginx and PHP runtimes, one directory per version.
pub fn runtimes() -> PathBuf {
    root().join("runtimes")
}

/// The active nginx prefix (`conf`, `logs`, `temp`).
pub fn nginx() -> PathBuf {
    root().join("nginx")
}

/// Generated per-project `php.ini` and logs, one directory per project slug.
pub fn sites() -> PathBuf {
    root().join("sites")
}

/// Issued per-domain leaf certificates.
pub fn certs() -> PathBuf {
    root().join("certs")
}

/// The local certificate authority.
pub fn ca() -> PathBuf {
    root().join("ca")
}

/// Create the directory tree if it is missing. Best-effort: a failure surfaces
/// later, when the specific write is attempted.
pub fn ensure() {
    for dir in [root(), runtimes(), nginx(), sites(), certs(), ca()] {
        let _ = std::fs::create_dir_all(dir);
    }
}

/// The user's home directory, from `USERPROFILE` first (Windows-native), then
/// the drive/path pair, then the current directory as a last resort.
fn home() -> PathBuf {
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        return PathBuf::from(profile);
    }
    if let (Some(drive), Some(path)) = (std::env::var_os("HOMEDRIVE"), std::env::var_os("HOMEPATH"))
    {
        let path = path.to_string_lossy();
        return PathBuf::from(drive).join(path.trim_start_matches('\\'));
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}
