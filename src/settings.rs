//! Persisted application settings, stored per-user under
//! `%USERPROFILE%\.config\wisp\settings.json`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::config;

/// Three-way theme choice. `System` follows the operating system appearance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThemePreference {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub theme: ThemePreference,
    /// The newest version the last check found, or `None` when it found none.
    ///
    /// Cached so the status bar and the Updates page can keep offering an
    /// update without another network call.
    #[serde(default)]
    pub available_version: Option<String>,
    /// Keep the tray icon and hide the window instead of exiting when it is
    /// closed. Off by default: closing a window should normally close it.
    #[serde(default)]
    pub background: bool,
    /// The installed nginx version Wisp treats as active. One nginx process
    /// serves every project, so only one version runs at a time.
    #[serde(default)]
    pub active_nginx: Option<String>,
    /// Whether Wisp's local certificate authority has been added to the current
    /// user's trusted root store. A hint for the UI; trusting is idempotent.
    #[serde(default)]
    pub ca_trusted: bool,
    /// Log viewer: keep the newest line in view as the tail refreshes.
    #[serde(default)]
    pub log_follow: Option<bool>,
    /// Log viewer: wrap long lines instead of running off the side.
    #[serde(default)]
    pub log_wrap: Option<bool>,
    /// Log viewer: match the search term case-sensitively.
    #[serde(default)]
    pub log_case: Option<bool>,
    /// Whether the sidebar is collapsed to its icons.
    #[serde(default)]
    pub sidebar_collapsed: Option<bool>,
    /// Whether the first-run onboarding has been completed.
    #[serde(default)]
    pub onboarding_complete: bool,
    /// Host the local MCP endpoint so CLI agents can inspect and control Wisp.
    ///
    /// Off by default: an agent-facing control surface should be an explicit
    /// choice, and a stale endpoint file from an earlier version is removed on
    /// start when this is off.
    #[serde(default)]
    pub mcp_enabled: bool,
    /// The loopback address the `.test` domains point at and nginx binds,
    /// when it is not `127.0.0.1`.
    ///
    /// Another program can hold ports 80 and 443 on `127.0.0.1` (WSL does, for
    /// a Docker engine that publishes them), and then it gets the requests
    /// instead of Wisp. Any other address in `127.0.0.0/8` is still this
    /// computer, and nobody else listens there.
    #[serde(default)]
    pub loopback_address: Option<String>,
}

/// Read a loopback address the user typed: an IPv4 address in `127.0.0.0/8`.
///
/// Only those are accepted because the address goes into the hosts file, which
/// is written with administrator rights, and into nginx's `listen` lines.
pub fn parse_loopback(text: &str) -> Option<std::net::Ipv4Addr> {
    text.trim()
        .parse::<std::net::Ipv4Addr>()
        .ok()
        .filter(std::net::Ipv4Addr::is_loopback)
}

impl Settings {
    /// The loopback address to serve on; `127.0.0.1` unless another was set.
    pub fn loopback(&self) -> std::net::Ipv4Addr {
        self.loopback_address
            .as_deref()
            .and_then(parse_loopback)
            .unwrap_or(std::net::Ipv4Addr::LOCALHOST)
    }

    fn file() -> PathBuf {
        config::root().join("settings.json")
    }

    /// Load settings, falling back to defaults if the file is missing or bad.
    pub fn load() -> Self {
        match std::fs::read_to_string(Self::file()) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Best-effort persist; failures are ignored so a read-only profile never
    /// crashes the app.
    pub fn save(&self) {
        let _ = std::fs::create_dir_all(config::root());
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(Self::file(), text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Settings, ThemePreference};

    #[test]
    fn theme_preference_defaults_to_system() {
        assert_eq!(ThemePreference::default(), ThemePreference::System);
    }

    #[test]
    fn the_mcp_server_is_off_by_default() {
        assert!(!Settings::default().mcp_enabled);
    }

    #[test]
    fn the_loopback_address_defaults_to_localhost_and_stays_on_loopback() {
        use super::parse_loopback;
        use std::net::Ipv4Addr;

        assert_eq!(Settings::default().loopback(), Ipv4Addr::LOCALHOST);
        assert_eq!(
            parse_loopback(" 127.0.0.2 "),
            Some(Ipv4Addr::new(127, 0, 0, 2))
        );
        assert_eq!(parse_loopback("192.168.1.10"), None, "not this computer");
        assert_eq!(parse_loopback("0.0.0.0"), None);
        assert_eq!(parse_loopback("::1"), None);
        assert_eq!(parse_loopback("127.0.0.2 evil.test"), None);

        // A bad value in the file falls back instead of breaking serving.
        let settings = Settings {
            loopback_address: Some("10.0.0.1".to_string()),
            ..Settings::default()
        };
        assert_eq!(settings.loopback(), Ipv4Addr::LOCALHOST);
    }

    #[test]
    fn theme_preference_deserializes_from_lowercase() {
        let parsed: ThemePreference = serde_json::from_str("\"dark\"").unwrap();
        assert_eq!(parsed, ThemePreference::Dark);
    }
}
