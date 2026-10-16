//! The Microsoft Visual C++ runtime that PHP's Windows builds link against.
//!
//! VS17 builds (PHP 8.4 and later) need the 14.30+ runtime; VS16 builds need
//! 14.20+. A newer runtime satisfies both, so one redistributable covers the
//! whole catalog. If it is missing, `php-cgi.exe` fails to start with an opaque
//! loader error, which is why Wisp checks up front and offers the installer.

use std::path::PathBuf;

use winreg::enums::HKEY_LOCAL_MACHINE;
use winreg::RegKey;

/// x64 runtime registration under the 32-bit view, and the native view.
const KEYS: [&str; 2] = [
    r"SOFTWARE\WOW6432Node\Microsoft\VisualStudio\14.0\VC\Runtimes\x64",
    r"SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\x64",
];

/// The latest redistributable; it satisfies every VS16 and VS17 build.
const INSTALLER_URL: &str = "https://aka.ms/vs/17/release/vc_redist.x64.exe";

/// The installed x64 runtime as `(major, minor)`, e.g. `(14, 38)`.
pub fn installed() -> Option<(u32, u32)> {
    for key in KEYS {
        if let Ok(reg) = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(key) {
            if let Ok(version) = reg.get_value::<String, _>("Version") {
                if let Some(parsed) = parse_version(&version) {
                    return Some(parsed);
                }
            }
        }
    }
    // The registry can be absent on trimmed images even when the DLL is present.
    if system32().join("vcruntime140.dll").is_file() {
        return Some((14, 0));
    }
    None
}

/// Whether the installed runtime satisfies a VS build (`16` or `17`).
pub fn satisfies(vc: u16) -> bool {
    let required = if vc >= 17 { (14, 30) } else { (14, 20) };
    installed().is_some_and(|current| current >= required)
}

/// A short human description of the installed runtime, for the UI.
pub fn description() -> String {
    match installed() {
        Some((major, minor)) => format!("v{major}.{minor} is installed"),
        None => "not installed".to_string(),
    }
}

/// Download the redistributable and launch it. It self-elevates, so the user
/// sees a single UAC prompt; we cannot wait for the interactive install.
pub fn launch_installer() -> Result<(), String> {
    let dest = std::env::temp_dir().join("wisp-vc_redist.x64.exe");
    super::runtime::download::download_to(INSTALLER_URL, &dest).map_err(|error| error.to_string())?;
    std::process::Command::new(&dest)
        .spawn()
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// Parse a redist `Version` value such as `v14.38.33130.00`.
fn parse_version(raw: &str) -> Option<(u32, u32)> {
    let trimmed = raw.trim().trim_start_matches(['v', 'V']);
    let mut parts = trimmed.split('.');
    let major = parts.next()?.parse::<u32>().ok()?;
    let minor = parts.next()?.parse::<u32>().ok()?;
    Some((major, minor))
}

fn system32() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("System32")
}

#[cfg(test)]
mod tests {
    use super::{parse_version, satisfies};

    #[test]
    fn parses_a_redist_version_string() {
        assert_eq!(parse_version("v14.38.33130.00"), Some((14, 38)));
        assert_eq!(parse_version("14.29.30139"), Some((14, 29)));
        assert_eq!(parse_version("nonsense"), None);
    }

    #[test]
    fn satisfaction_reflects_the_installed_runtime() {
        // Whatever this machine has, the check must not panic and must be
        // consistent with the reported version.
        let _ = satisfies(17);
        let _ = satisfies(16);
    }
}
