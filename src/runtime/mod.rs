//! Managing the PHP and nginx runtimes Wisp downloads.
//!
//! Every runtime lives under `~/.config/wisp/runtimes/<kind>/<version>/`. PHP
//! archives unpack flat, so that folder holds `php-cgi.exe`; nginx archives wrap
//! everything in `nginx-<version>/`, which is stripped so the folder itself is
//! the nginx prefix (`conf/`, `logs/`, `nginx.exe`).

pub mod catalog;
pub mod download;

use std::path::PathBuf;

use crate::config;

pub use catalog::Release;

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("{0}")]
    Download(#[from] download::DownloadError),

    #[error("{0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, RuntimeError>;

/// Which runtime family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeKind {
    Php,
    Nginx,
}

impl RuntimeKind {
    /// The `kind` stored in the database and used for the folder name.
    pub fn as_str(self) -> &'static str {
        match self {
            RuntimeKind::Php => "php",
            RuntimeKind::Nginx => "nginx",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            RuntimeKind::Php => "PHP",
            RuntimeKind::Nginx => "nginx",
        }
    }

    /// How many leading archive path components to drop on extraction.
    fn strip_components(self) -> usize {
        match self {
            RuntimeKind::Php => 0,
            RuntimeKind::Nginx => 1,
        }
    }
}

/// Where a version of a runtime lives on disk.
pub fn install_dir(kind: RuntimeKind, version: &str) -> PathBuf {
    config::runtimes().join(kind.as_str()).join(version)
}

/// The PHP `FastCGI` binary for an installed version. Used once projects start
/// their own `FastCGI` worker.
#[allow(dead_code)]
pub fn php_cgi(version: &str) -> PathBuf {
    install_dir(RuntimeKind::Php, version).join("php-cgi.exe")
}

/// Download and unpack a release into its install directory.
pub fn install(
    kind: RuntimeKind,
    release: &Release,
    progress: &(dyn Fn(f32) + Sync),
) -> Result<PathBuf> {
    config::ensure();
    let dir = install_dir(kind, &release.version);
    let scratch = config::runtimes().join(".downloads");
    std::fs::create_dir_all(&scratch)?;
    let archive = scratch.join(format!("{}-{}.zip", kind.as_str(), release.version));

    download::download_to_reporting(&release.url, &archive, progress)?;

    if dir.exists() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    std::fs::create_dir_all(&dir)?;

    if let Err(error) = download::extract_zip(&archive, &dir, kind.strip_components()) {
        // A half-unpacked runtime is worse than none.
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&archive);
        return Err(error.into());
    }
    let _ = std::fs::remove_file(&archive);

    if kind == RuntimeKind::Nginx {
        for sub in ["logs", "temp"] {
            std::fs::create_dir_all(dir.join(sub))?;
        }
    }
    Ok(dir)
}

/// Delete an installed runtime's files. The caller clears the database record.
pub fn remove(kind: RuntimeKind, version: &str) -> Result<()> {
    let dir = install_dir(kind, version);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{install, remove, RuntimeKind};

    /// Exercises the real download and unpack against nginx.org. Ignored by
    /// default so the suite stays offline; run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "downloads from the network"]
    fn installs_and_removes_nginx_end_to_end() {
        let _guard = crate::test_support::ENV_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join("wisp-nginx-e2e");
        let _ = std::fs::remove_dir_all(&root);
        std::env::set_var("WISP_CONFIG_DIR", &root);

        let release = crate::runtime::Release {
            version: "1.27.0".to_string(),
            url: "https://nginx.org/download/nginx-1.27.0.zip".to_string(),
            vc: None,
        };

        let dir = install(RuntimeKind::Nginx, &release, &|_| {}).expect("install should succeed");
        assert!(dir.join("nginx.exe").is_file(), "nginx.exe should be unpacked");
        assert!(dir.join("conf").join("nginx.conf").is_file());
        assert!(dir.join("logs").is_dir());
        assert!(dir.join("temp").is_dir());

        remove(RuntimeKind::Nginx, "1.27.0").expect("remove should succeed");
        assert!(!dir.exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Downloads a full PHP build (~33 MB) and checks the `FastCGI` binary lands.
    /// Ignored by default; run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "downloads ~33 MB from the network"]
    fn installs_php_and_finds_the_fastcgi_binary() {
        let _guard = crate::test_support::ENV_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join("wisp-php-e2e");
        let _ = std::fs::remove_dir_all(&root);
        std::env::set_var("WISP_CONFIG_DIR", &root);

        let version = "8.3.35";
        let release = crate::runtime::Release {
            version: version.to_string(),
            url: format!(
                "https://windows.php.net/downloads/releases/php-{version}-nts-Win32-vs16-x64.zip"
            ),
            vc: Some(16),
        };

        let dir = install(RuntimeKind::Php, &release, &|_| {}).expect("install should succeed");
        assert!(
            crate::runtime::php_cgi(version).is_file(),
            "php-cgi.exe should be unpacked"
        );
        assert!(dir.join("php.ini-development").is_file());

        remove(RuntimeKind::Php, version).expect("remove should succeed");
        let _ = std::fs::remove_dir_all(&root);
    }
}
