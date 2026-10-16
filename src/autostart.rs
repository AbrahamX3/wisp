//! Opt-in "start Wisp with Windows".
//!
//! This is the per-user `Run` key, which needs no elevation, so the menu toggle
//! never involves a privileged helper. The registry is the source of truth: the
//! menu reads it back, so an entry turned off from Task Manager → Startup apps
//! is reflected here.

use std::io;
use std::path::{Path, PathBuf};

use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
use winreg::RegKey;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// Name of the value, which is also what Task Manager lists.
const VALUE_NAME: &str = "Wisp";

/// The command line Windows runs at sign-in.
///
/// `--startup` marks an autostarted launch, which brings the window up without
/// stealing focus.
fn command_line(exe: &Path) -> String {
    format!("\"{}\" --startup", exe.display())
}

/// Whether the Run entry exists *and* still points at this executable.
///
/// A stale entry (the app was moved, or reinstalled elsewhere) reads as off and
/// is rewritten the next time the toggle is switched on.
pub fn is_enabled() -> bool {
    let (Some(exe), Ok(key)) = (current_exe(), open(KEY_READ)) else {
        return false;
    };
    key.get_value::<String, _>(VALUE_NAME)
        .is_ok_and(|stored| points_at(&stored, &exe))
}

/// Add or remove the entry. Returns whether it is enabled afterwards.
pub fn set(enabled: bool) -> io::Result<()> {
    if enabled {
        let exe = current_exe()
            .ok_or_else(|| io::Error::other("could not resolve the executable path"))?;
        open(KEY_WRITE)?.set_value(VALUE_NAME, &command_line(&exe))
    } else {
        match open(KEY_WRITE)?.delete_value(VALUE_NAME) {
            // Already absent is the state we wanted.
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
}

fn current_exe() -> Option<PathBuf> {
    std::env::current_exe().ok()
}

fn open(access: u32) -> io::Result<RegKey> {
    RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN_KEY, access)
}

/// Whether a stored `Run` value launches the given executable.
///
/// The value is `"C:\path\wisp.exe" --startup`, so compare on the quoted path
/// rather than the whole string.
fn points_at(stored: &str, exe: &Path) -> bool {
    let stored = stored.trim_start_matches('"').to_lowercase();
    stored.starts_with(&exe.to_string_lossy().to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::points_at;
    use std::path::Path;

    #[test]
    fn matches_the_quoted_command_we_write() {
        let exe = Path::new(r"C:\Apps\Wisp\wisp.exe");
        assert!(points_at(r#""C:\Apps\Wisp\wisp.exe" --startup"#, exe));
    }

    #[test]
    fn matches_case_insensitively() {
        let exe = Path::new(r"C:\Apps\Wisp\wisp.exe");
        assert!(points_at(r#""c:\apps\wisp\WISP.EXE" --startup"#, exe));
    }

    #[test]
    fn rejects_an_entry_left_behind_by_an_older_install() {
        let exe = Path::new(r"C:\Apps\Wisp\wisp.exe");
        assert!(!points_at(r#""C:\Old\Somewhere\wisp.exe" --startup"#, exe));
    }

    #[test]
    fn rejects_an_unrelated_entry() {
        let exe = Path::new(r"C:\Apps\Wisp\wisp.exe");
        assert!(!points_at(r#""C:\Other\thing.exe""#, exe));
    }
}
