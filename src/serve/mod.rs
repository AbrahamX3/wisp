//! Keeping nginx and the per-project PHP workers in the state the user asked
//! for.
//!
//! The supervisor is stateless about *intent*: the UI hands it the projects that
//! should be running and it reconciles reality — writing configs, starting nginx
//! if needed, spawning or replacing workers — so a sync is idempotent.

pub mod config;
pub mod process;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Child;

use crate::runtime::{self, RuntimeKind};

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("{0}")]
    Io(#[from] std::io::Error),

    /// Something needed to serve is not installed or not configured yet.
    #[error("{0}")]
    Missing(String),

    /// nginx exited as soon as it was started (usually a port already in use).
    #[error("{0}")]
    Start(String),

    /// A certificate could not be created.
    #[error("{0}")]
    Tls(#[from] crate::tls::TlsError),
}

pub type Result<T> = std::result::Result<T, ServeError>;

/// What the UI wants running for one project.
#[derive(Clone, Debug)]
pub struct SiteSpec {
    pub id: i64,
    pub slug: String,
    pub domain: String,
    pub root: PathBuf,
    pub php_version: String,
    pub ssl_enabled: bool,
}

/// A snapshot of what is actually running.
#[derive(Clone, Debug, Default)]
pub struct Status {
    pub nginx_running: bool,
    pub running_projects: Vec<i64>,
}

struct Worker {
    child: Child,
    #[allow(dead_code)]
    port: u16,
}

#[derive(Default)]
pub struct Supervisor {
    nginx: Option<Child>,
    nginx_version: Option<String>,
    workers: HashMap<i64, Worker>,
    /// Allocated FastCGI ports, stable while a project keeps running.
    ports: HashMap<i64, u16>,
}

impl Supervisor {
    /// Reap exited processes and report what is still alive.
    pub fn status(&mut self) -> Status {
        let nginx_running = self
            .nginx
            .as_mut()
            .is_some_and(process::is_running);
        if !nginx_running {
            self.nginx = None;
            self.nginx_version = None;
        }

        let mut running_projects = Vec::new();
        let mut dead = Vec::new();
        for (id, worker) in self.workers.iter_mut() {
            if process::is_running(&mut worker.child) {
                running_projects.push(*id);
            } else {
                dead.push(*id);
            }
        }
        for id in dead {
            self.workers.remove(&id);
        }

        Status {
            nginx_running,
            running_projects,
        }
    }

    /// Bring the running set in line with `sites`, using `nginx_version`.
    pub fn sync(&mut self, nginx_version: Option<&str>, sites: &[SiteSpec]) -> Result<()> {
        let version = nginx_version.ok_or_else(|| {
            ServeError::Missing("No nginx version is active. Install one on Services.".into())
        })?;
        let prefix = config::nginx_prefix(version);
        if !prefix.join("nginx.exe").is_file() {
            return Err(ServeError::Missing(format!("nginx {version} is not installed.")));
        }

        self.write_configs(&prefix, sites)?;
        self.ensure_nginx(version, &prefix)?;
        self.reconcile_workers(sites)
    }

    /// Stop everything (used on exit).
    pub fn stop_all(&mut self) {
        for id in self.workers.keys().copied().collect::<Vec<_>>() {
            self.stop_project(id);
        }
        if let Some(version) = self.nginx_version.clone() {
            let prefix = config::nginx_prefix(&version);
            self.stop_nginx(&prefix);
        }
    }

    /// Stop and forget one project's worker.
    pub fn stop_project(&mut self, id: i64) {
        if let Some(mut worker) = self.workers.remove(&id) {
            let _ = process::kill_tree(&mut worker.child);
        }
        self.ports.remove(&id);
    }

    // ---- internals -------------------------------------------------------

    fn write_configs(&mut self, prefix: &Path, sites: &[SiteSpec]) -> Result<()> {
        let vhosts = config::vhosts_dir(prefix);
        std::fs::create_dir_all(&vhosts)?;

        // Drop vhosts for projects that are no longer running.
        let wanted: HashSet<&str> = sites.iter().map(|site| site.slug.as_str()).collect();
        for entry in std::fs::read_dir(&vhosts)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(slug) = name.strip_suffix(".conf") {
                if !wanted.contains(slug) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }

        for site in sites {
            let port = self.port_for(site.id);
            let docroot = config::resolve_docroot(&site.root);
            let log_dir = config::site_logs(&site.slug);
            std::fs::create_dir_all(&log_dir)?;

            let tls = if site.ssl_enabled {
                Some(crate::tls::issue(&site.domain)?)
            } else {
                None
            };

            let vhost = config::Vhost {
                domain: &site.domain,
                docroot: &docroot,
                port,
                listen_port: config::http_port(),
                log_dir: &log_dir,
                tls: tls.as_ref(),
            }
            .render();
            std::fs::write(vhosts.join(format!("{}.conf", site.slug)), vhost)?;

            self.write_php_ini(site)?;
        }

        std::fs::create_dir_all(config::conf_dir(prefix))?;
        std::fs::write(config::main_config_path(prefix), config::main_config())?;
        Ok(())
    }

    fn write_php_ini(&self, site: &SiteSpec) -> Result<()> {
        let runtime_dir = runtime::install_dir(RuntimeKind::Php, &site.php_version);
        let template =
            std::fs::read_to_string(runtime_dir.join("php.ini-development")).unwrap_or_default();
        let extension_dir = runtime_dir.join("ext");
        let dir = config::site_dir(&site.slug);
        std::fs::create_dir_all(&dir)?;
        let error_log = dir.join("logs").join("php-error.log");
        std::fs::write(
            dir.join("php.ini"),
            config::php_ini(&template, &extension_dir, &error_log),
        )?;
        Ok(())
    }

    fn reconcile_workers(&mut self, sites: &[SiteSpec]) -> Result<()> {
        let wanted: HashSet<i64> = sites.iter().map(|site| site.id).collect();
        let stale: Vec<i64> = self
            .workers
            .keys()
            .copied()
            .filter(|id| !wanted.contains(id))
            .collect();
        for id in stale {
            self.stop_project(id);
        }

        for site in sites {
            let alive = self
                .workers
                .get_mut(&site.id)
                .is_some_and(|worker| process::is_running(&mut worker.child));
            if alive {
                continue;
            }
            self.workers.remove(&site.id);
            let port = self.port_for(site.id);
            let child = self.spawn_worker(site, port)?;
            self.workers.insert(site.id, Worker { child, port });
        }
        Ok(())
    }

    fn spawn_worker(&self, site: &SiteSpec, port: u16) -> Result<Child> {
        let runtime_dir = runtime::install_dir(RuntimeKind::Php, &site.php_version);
        let exe = runtime::php_cgi(&site.php_version);
        if !exe.is_file() {
            return Err(ServeError::Missing(format!(
                "PHP {} is not installed.",
                site.php_version
            )));
        }
        let ini = config::site_dir(&site.slug).join("php.ini");
        let args = vec![
            "-b".to_string(),
            format!("127.0.0.1:{port}"),
            "-c".to_string(),
            ini.to_string_lossy().to_string(),
        ];
        // 0 requests means "never recycle", so php-cgi does not exit mid-session.
        let env = vec![
            ("PHP_FCGI_MAX_REQUESTS", "0".to_string()),
            ("PHP_FCGI_CHILDREN", "0".to_string()),
        ];
        Ok(process::spawn(&exe, &args, &env, Some(&runtime_dir))?)
    }

    fn ensure_nginx(&mut self, version: &str, prefix: &Path) -> Result<()> {
        let running = self.nginx.as_mut().is_some_and(process::is_running);
        if running {
            if self.nginx_version.as_deref() == Some(version) {
                // Same version: pick up any config changes without dropping it.
                let _ = self.signal(prefix, "-s", "reload");
                return Ok(());
            }
            self.stop_nginx(prefix);
        }

        let exe = prefix.join("nginx.exe");
        let args = nginx_args(prefix, &[]);
        let mut child = process::spawn(&exe, &args, &[], None)?;
        // nginx daemonises on Unix but not on Windows: the process we spawned is
        // the master. If it exits immediately, a bind failed (port in use) — say
        // so rather than leaving the UI thinking it started.
        std::thread::sleep(std::time::Duration::from_millis(600));
        if !process::is_running(&mut child) {
            let tail = read_log_tail(&prefix.join("logs").join("error.log"));
            return Err(ServeError::Start(format!(
                "nginx stopped right after starting. {tail}"
            )));
        }
        self.nginx = Some(child);
        self.nginx_version = Some(version.to_string());
        Ok(())
    }

    fn stop_nginx(&mut self, prefix: &Path) {
        if let Some(mut child) = self.nginx.take() {
            let _ = self.signal(prefix, "-s", "quit");
            std::thread::sleep(std::time::Duration::from_millis(400));
            if process::is_running(&mut child) {
                let _ = process::kill_tree(&mut child);
            }
        }
        self.nginx_version = None;
    }

    /// Ask the running nginx to reload or quit.
    fn signal(&self, prefix: &Path, flag: &str, value: &str) -> std::io::Result<()> {
        let exe = prefix.join("nginx.exe");
        let args = nginx_args(prefix, &[flag, value]);
        let status = std::process::Command::new(exe).args(args).status()?;
        if status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other("nginx signal failed"))
        }
    }

    fn port_for(&mut self, id: i64) -> u16 {
        if let Some(port) = self.ports.get(&id) {
            return *port;
        }
        let port = free_port();
        self.ports.insert(id, port);
        port
    }
}

/// A localhost port the OS says is free.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|listener| listener.local_addr().ok())
        .map_or(9070, |addr| addr.port())
}

fn nginx_args(prefix: &Path, extra: &[&str]) -> Vec<String> {
    let mut args = vec![
        "-p".to_string(),
        with_trailing_sep(prefix),
        "-c".to_string(),
        "conf/wisp.conf".to_string(),
    ];
    args.extend(extra.iter().map(|value| (*value).to_string()));
    args
}

fn with_trailing_sep(prefix: &Path) -> String {
    let mut value = prefix.to_string_lossy().to_string();
    if !value.ends_with('\\') && !value.ends_with('/') {
        value.push('\\');
    }
    value
}

/// The last part of nginx's error log, to explain a failed start.
fn read_log_tail(path: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return "See the nginx log for details.".to_string();
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "See the nginx log for details.".to_string();
    }
    let mut chars: Vec<char> = trimmed.chars().rev().take(600).collect();
    chars.reverse();
    format!("nginx log: {}", chars.into_iter().collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::{SiteSpec, Supervisor};
    use crate::runtime::{self, RuntimeKind, Release};

    fn ensure_nginx(version: &str) {
        let dir = runtime::install_dir(RuntimeKind::Nginx, version);
        if dir.join("nginx.exe").is_file() {
            return;
        }
        let release = Release {
            version: version.to_string(),
            url: format!("https://nginx.org/download/nginx-{version}.zip"),
            vc: None,
        };
        runtime::install(RuntimeKind::Nginx, &release).expect("nginx install");
    }

    fn ensure_php(version: &str) {
        if runtime::php_cgi(version).is_file() {
            return;
        }
        let release = Release {
            version: version.to_string(),
            url: format!(
                "https://windows.php.net/downloads/releases/php-{version}-nts-Win32-vs16-x64.zip"
            ),
            vc: Some(16),
        };
        runtime::install(RuntimeKind::Php, &release).expect("php install");
    }

    /// Starts the real nginx and php-cgi, serves a file, and stops again.
    /// Ignored by default; run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "starts real processes and downloads runtimes"]
    fn serves_a_project_over_http() {
        let _guard = crate::test_support::ENV_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join("wisp-serve-e2e");
        let _ = std::fs::remove_dir_all(&root);
        std::env::set_var("WISP_CONFIG_DIR", &root);
        // Honour an override (e.g. when port 80 is taken), else use the default.
        let port: u16 = std::env::var("WISP_HTTP_PORT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(80);

        ensure_nginx("1.27.0");
        ensure_php("8.3.35");

        let site = root.join("site");
        std::fs::create_dir_all(&site).unwrap();
        std::fs::write(site.join("index.php"), "<?php echo 'Hello from Wisp';").unwrap();

        let spec = SiteSpec {
            id: 1,
            slug: "demo".to_string(),
            domain: "demo.test".to_string(),
            root: site,
            php_version: "8.3.35".to_string(),
            ssl_enabled: false,
        };

        let mut supervisor = Supervisor::default();
        supervisor
            .sync(Some("1.27.0"), &[spec])
            .expect("sync should start nginx and a worker");

        std::thread::sleep(std::time::Duration::from_millis(900));

        let url = format!("http://127.0.0.1:{port}/");
        let body = ureq::get(&url)
            .header("Host", "demo.test")
            .call()
            .and_then(|mut response| response.body_mut().read_to_string());

        // Tear down before asserting, so a failure cannot orphan processes.
        supervisor.stop_all();
        std::thread::sleep(std::time::Duration::from_millis(400));

        let body = body.expect("the site should respond");
        assert!(body.contains("Hello from Wisp"), "unexpected body: {body}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Serves a project over HTTPS and verifies the certificate chain against
    /// the generated CA with curl (so the hostname/SAN is checked too).
    /// Ignored by default; run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "starts real processes and downloads runtimes"]
    fn serves_https_with_a_valid_chain() {
        let _guard = crate::test_support::ENV_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join("wisp-serve-tls");
        let _ = std::fs::remove_dir_all(&root);
        std::env::set_var("WISP_CONFIG_DIR", &root);

        ensure_nginx("1.27.0");
        ensure_php("8.3.35");

        let site = root.join("site");
        std::fs::create_dir_all(&site).unwrap();
        std::fs::write(site.join("index.php"), "<?php echo 'Secure hello';").unwrap();

        let spec = SiteSpec {
            id: 1,
            slug: "demo".to_string(),
            domain: "demo.test".to_string(),
            root: site,
            php_version: "8.3.35".to_string(),
            ssl_enabled: true,
        };

        let mut supervisor = Supervisor::default();
        supervisor
            .sync(Some("1.27.0"), &[spec])
            .expect("sync should start https");

        std::thread::sleep(std::time::Duration::from_millis(900));

        // Windows curl uses Schannel, which refuses a private CA it cannot
        // check for revocation; `--ssl-no-revoke` skips that unrelated check
        // while still verifying the signature and hostname.
        let output = std::process::Command::new("curl.exe")
            .args([
                "-s",
                "--ssl-no-revoke",
                "--resolve",
                "demo.test:443:127.0.0.1",
                "--cacert",
            ])
            .arg(crate::tls::ca_cert())
            .arg("https://demo.test/")
            .output();

        supervisor.stop_all();
        std::thread::sleep(std::time::Duration::from_millis(400));

        let output = output.expect("curl should run");
        let body = String::from_utf8_lossy(&output.stdout).to_string();
        assert!(
            body.contains("Secure hello"),
            "curl said: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
