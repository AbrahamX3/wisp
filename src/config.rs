//! Where Wisp keeps everything, all under `%USERPROFILE%\.config\wisp`.
//!
//! One root holds the SQLite database, the generated nginx/PHP configuration,
//! the local certificate authority, and the managed nginx/PHP runtimes. Per-user
//! and writable without elevation, which is what keeps the in-app updater and
//! the runtime manager free of prompts.

use std::path::PathBuf;

use base64::Engine as _;

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

/// Public CA bundle used by PHP's curl and openssl extensions.
pub fn ca_bundle() -> PathBuf {
    ca().join("cacert.pem")
}

/// Materialise the Windows trust store as a PEM bundle for PHP.
///
/// PHP's OpenSSL build has no CA file of its own, so every request that uses
/// curl (Composer, AWS, HTTP clients) fails TLS verification until
/// `curl.cainfo` / `openssl.cafile` point at one. Generating it from the OS
/// store keeps it current with Windows Update and includes enterprise roots.
fn ensure_ca_bundle() {
    ensure_ca_bundle_at(&ca_bundle());
}

fn ensure_ca_bundle_at(path: &std::path::Path) {
    let result = rustls_native_certs::load_native_certs();
    if result.certs.is_empty() {
        return;
    }

    let mut pem = String::new();
    for cert in result.certs {
        pem.push_str("-----BEGIN CERTIFICATE-----\n");
        let encoded = base64::engine::general_purpose::STANDARD.encode(cert.as_ref());
        for chunk in encoded.as_bytes().chunks(64) {
            pem.push_str(std::str::from_utf8(chunk).unwrap_or_default());
            pem.push('\n');
        }
        pem.push_str("-----END CERTIFICATE-----\n");
    }

    if std::fs::read_to_string(path).is_ok_and(|existing| existing == pem) {
        return;
    }

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, pem);
}

/// A stable id for a non-default config root, or `None` for the normal profile.
///
/// The single-instance mutex is keyed by profile: a portable or test profile
/// (`WISP_CONFIG_DIR`) is a separate install with its own database and
/// runtimes, so it must be able to run alongside the installed app. The default
/// profile keeps the historical mutex name.
pub fn profile_id() -> Option<String> {
    let root = root();
    if root == default_root() {
        return None;
    }
    // Windows paths are case-insensitive, so equal directories must hash alike.
    use std::hash::{Hash as _, Hasher as _};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    root.to_string_lossy().to_lowercase().hash(&mut hasher);
    Some(format!("{:016x}", hasher.finish()))
}

/// Create the directory tree if it is missing. Best-effort: a failure surfaces
/// later, when the specific write is attempted.
pub fn ensure() {
    for dir in [root(), runtimes(), nginx(), sites(), certs(), ca()] {
        let _ = std::fs::create_dir_all(dir);
    }
    ensure_ca_bundle();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_store_becomes_a_pem_bundle() {
        let path = std::env::temp_dir().join("wisp-ca-bundle-test.pem");
        let _ = std::fs::remove_file(&path);

        ensure_ca_bundle_at(&path);

        let pem = std::fs::read_to_string(&path).expect("CA bundle should be generated");
        let count = pem.matches("BEGIN CERTIFICATE").count();
        assert!(
            count > 0,
            "the native store should contain at least one root"
        );
        eprintln!("native roots: {count}");

        let _ = std::fs::remove_file(&path);
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
