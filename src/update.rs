//! In-app updates from GitHub Releases.
//!
//! Releases are built by `.github/workflows/release.yml`, which publishes
//! `wisp-x86_64-pc-windows-msvc.zip` (containing `wisp.exe`) plus a
//! `SHA256SUMS` asset. The updater verifies the download against that checksum
//! before replacing the running executable.
//!
//! Checking and installing both block on the network, so callers run them on
//! the background executor.

use self_update::backends::github::Update;

// Crate-visible so the About dialog can assert it links to the same repository
// the updater targets (see `about_dialog_repository_url_matches_the_updater`).
pub(crate) const REPO_OWNER: &str = "AbrahamX3";
pub(crate) const REPO_NAME: &str = "wisp";
const BIN_NAME: &str = "wisp";

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("{0}")]
    Backend(#[from] self_update::errors::Error),
}

/// `Some(version)` when a release newer than the running build exists.
pub fn check() -> Result<Option<String>, UpdateError> {
    let release = configured()?.is_update_available()?;
    Ok(release.map(|release| release.version().to_string()))
}

/// Download, verify and replace this executable.
/// `Some(version)` when the binary was replaced; `None` when already current.
pub fn install() -> Result<Option<String>, UpdateError> {
    let status = configured()?.update()?;
    Ok(status.is_updated().then(|| status.version().to_owned()))
}

fn configured() -> Result<Update, UpdateError> {
    Ok(Update::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name(BIN_NAME)
        .current_version(env!("CARGO_PKG_VERSION"))
        .bin_path_in_archive(format!("{BIN_NAME}.exe"))
        .checksum_from_asset("SHA256SUMS")
        .no_confirm(true)
        .show_output(false)
        .show_download_progress(false)
        .build()?)
}
