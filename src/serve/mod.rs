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
use std::time::Duration;

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
    /// Raw `php.ini` directives the project adds, appended last.
    pub php_ini_overrides: String,
    /// Whether nginx should serve this site's server block.
    pub vhost: bool,
    /// Whether a php-cgi worker should run for this site.
    pub worker: bool,
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
    /// Whether the one-time sweep of workers left by a previous run has run.
    swept: bool,
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
    ///
    /// A site can be served (`vhost`) and/or have a worker (`worker`)
    /// independently, so nginx only runs while at least one site is served.
    pub fn sync(&mut self, nginx_version: Option<&str>, sites: &[SiteSpec]) -> Result<()> {
        // A previous run may have been killed without stopping its workers,
        // leaving them holding ports and stale config. Clear them once.
        if !self.swept {
            sweep_orphans();
            self.swept = true;
        }

        // Write each worker's php.ini; the worker must restart if it changed.
        let mut restart = Vec::new();
        for site in sites.iter().filter(|site| site.worker) {
            if self.write_php_ini(site)? {
                restart.push(site.id);
            }
        }

        // Drop workers for sites that no longer want one.
        let wanted_workers: HashSet<i64> = sites
            .iter()
            .filter(|site| site.worker)
            .map(|site| site.id)
            .collect();
        let stale: Vec<i64> = self
            .workers
            .keys()
            .copied()
            .filter(|id| !wanted_workers.contains(id))
            .collect();
        for id in stale {
            self.forget_project(id);
        }

        if sites.iter().any(|site| site.vhost) {
            let version = nginx_version.ok_or_else(|| {
                ServeError::Missing("No nginx version is active. Install one on Services.".into())
            })?;
            let prefix = config::nginx_prefix(version);
            if !prefix.join("nginx.exe").is_file() {
                return Err(ServeError::Missing(format!("nginx {version} is not installed.")));
            }
            self.write_vhosts(&prefix, sites)?;
            self.ensure_nginx(version, &prefix)?;
        } else {
            // Nothing is served, so nginx has nothing to do.
            self.stop_nginx_only();
        }

        self.reconcile_workers(sites, &restart)
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

    /// Stop one project's worker but keep its port, so a restart reuses the port
    /// the already-written vhost points at.
    pub fn stop_project(&mut self, id: i64) {
        if let Some(mut worker) = self.workers.remove(&id) {
            let _ = process::kill_tree(&mut worker.child);
        }
    }

    /// Forget a project's port entirely (used when it is no longer wanted).
    pub fn forget_project(&mut self, id: i64) {
        self.stop_project(id);
        self.ports.remove(&id);
    }

    /// Stop nginx, leaving the PHP workers running.
    pub fn stop_nginx_only(&mut self) {
        if let Some(version) = self.nginx_version.clone() {
            let prefix = config::nginx_prefix(&version);
            self.stop_nginx(&prefix);
        }
        self.nginx = None;
        self.nginx_version = None;
    }

    /// Stop every PHP worker, keeping their ports and their running intent.
    pub fn stop_workers(&mut self) {
        for id in self.workers.keys().copied().collect::<Vec<_>>() {
            self.stop_project(id);
        }
    }

    // ---- internals -------------------------------------------------------

    /// Write the server block for every served site, and drop the rest.
    fn write_vhosts(&mut self, prefix: &Path, sites: &[SiteSpec]) -> Result<()> {
        let vhosts = config::vhosts_dir(prefix);
        std::fs::create_dir_all(&vhosts)?;

        let wanted: HashSet<&str> = sites
            .iter()
            .filter(|site| site.vhost)
            .map(|site| site.slug.as_str())
            .collect();
        for entry in std::fs::read_dir(&vhosts)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(slug) = name.strip_suffix(".conf") {
                if !wanted.contains(slug) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }

        for site in sites.iter().filter(|site| site.vhost) {
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
        }

        std::fs::create_dir_all(config::conf_dir(prefix))?;
        std::fs::write(config::main_config_path(prefix), config::main_config())?;
        Ok(())
    }

    /// Write the per-project `php.ini`, returning whether it actually changed.
    fn write_php_ini(&self, site: &SiteSpec) -> Result<bool> {
        let runtime_dir = runtime::install_dir(RuntimeKind::Php, &site.php_version);
        let template =
            std::fs::read_to_string(runtime_dir.join("php.ini-development")).unwrap_or_default();
        let extension_dir = runtime_dir.join("ext");
        let dir = config::site_dir(&site.slug);
        std::fs::create_dir_all(&dir)?;
        let error_log = dir.join("logs").join("php-error.log");

        let path = dir.join("php.ini");
        let contents = config::php_ini(&template, &extension_dir, &error_log, &site.php_ini_overrides);
        let changed = match std::fs::read_to_string(&path) {
            Ok(existing) => existing != contents,
            Err(_) => true,
        };
        std::fs::write(&path, contents)?;
        Ok(changed)
    }

    fn reconcile_workers(&mut self, sites: &[SiteSpec], restart: &[i64]) -> Result<()> {
        // Only sites that want a worker; the rest were forgotten in `sync`.
        for site in sites.iter().filter(|site| site.worker) {
            let alive = self
                .workers
                .get_mut(&site.id)
                .is_some_and(|worker| process::is_running(&mut worker.child));
            // An alive worker is kept unless its config changed underneath it.
            if alive && !restart.contains(&site.id) {
                continue;
            }
            self.stop_project(site.id);
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
        let log = config::site_logs(&site.slug).join("php-cgi.log");
        let mut child = process::spawn_logged(&exe, &args, &env, Some(&runtime_dir), &log)?;

        // A worker that fails to start (bad ini, missing DLL, busy port) exits
        // immediately. Wait for it to accept connections so nginx never points
        // at a dead port — a 502 with no explanation is worse than an error.
        if !wait_for_port(port, Duration::from_millis(3000)) {
            let tail = read_log_tail(&log, "worker log");
            let _ = process::kill_tree(&mut child);
            return Err(ServeError::Start(format!(
                "the PHP worker for \"{}\" did not start. {tail}",
                site.domain
            )));
        }
        Ok(child)
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
            let tail = read_log_tail(&prefix.join("logs").join("error.log"), "nginx log");
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

/// Kill `php-cgi` workers left behind by a previous run, so they do not hold
/// ports and stale configuration. Only processes whose command line points at
/// Wisp's own sites directory are touched.
fn sweep_orphans() {
    let needle = crate::config::sites().to_string_lossy().to_string();
    let script = format!(
        "Get-CimInstance Win32_Process -Filter \"Name='php-cgi.exe'\" \
         | Where-Object {{ $_.CommandLine -like '*{needle}*' }} \
         | ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }}"
    );
    let powershell = system32()
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    let args = vec![
        "-NoProfile".to_string(),
        "-NonInteractive".to_string(),
        "-Command".to_string(),
        script,
    ];
    let _ = process::spawn(&powershell, &args, &[], None);
}

fn system32() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("System32")
}

/// Wait until something accepts connections on `port`, or the timeout passes.
fn wait_for_port(port: u16, timeout: Duration) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(250)).is_ok() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A localhost port the OS says is free.
fn free_port() -> u16 {    std::net::TcpListener::bind("127.0.0.1:0")
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

/// The last part of a log file, to explain a failed start.
fn read_log_tail(path: &Path, label: &str) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return "See the log for details.".to_string();
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "See the log for details.".to_string();
    }
    let mut chars: Vec<char> = trimmed.chars().rev().take(600).collect();
    chars.reverse();
    format!("{label}: {}", chars.into_iter().collect::<String>())
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
            php_ini_overrides: String::new(),
            vhost: true,
            worker: true,
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

    /// Runs the real sync against a real config directory, then hits each
    /// project's domain. Point `WISP_CONFIG_DIR` at a Wisp data directory to use
    /// it. Leaves the server running so it can be inspected.
    /// Ignored by default: `WISP_CONFIG_DIR=... cargo test syncs_the_configured_projects -- --ignored`
    #[test]
    #[ignore = "runs the real sync against a real config directory"]
    fn syncs_the_configured_projects() {
        let Ok(dir) = std::env::var("WISP_CONFIG_DIR") else {
            eprintln!("set WISP_CONFIG_DIR to a Wisp data directory; skipping");
            return;
        };
        if !std::path::Path::new(&dir).join("wisp.db").is_file() {
            eprintln!("no wisp.db under {dir}; skipping");
            return;
        }

        let store = crate::db::Store::open().expect("the store should open");
        let specs: Vec<SiteSpec> = store
            .list_projects()
            .expect("projects")
            .into_iter()
            .filter_map(|project| {
                project.php_version.clone().map(|php| SiteSpec {
                    id: project.id,
                    slug: project.slug(),
                    domain: project.domain.clone(),
                    root: std::path::PathBuf::from(&project.root_path),
                    php_version: php,
                    ssl_enabled: project.ssl_enabled,
                    php_ini_overrides: project.php_ini_overrides.clone(),
                    vhost: true,
                    worker: true,
                })
            })
            .collect();
        assert!(!specs.is_empty(), "no projects with a PHP version");

        let settings = crate::settings::Settings::load();
        let mut supervisor = Supervisor::default();
        supervisor
            .sync(settings.active_nginx.as_deref(), &specs)
            .expect("sync should start the server");

        // Let nginx and the workers settle.
        std::thread::sleep(std::time::Duration::from_millis(1200));

        for spec in &specs {
            let scheme = if spec.ssl_enabled { "https" } else { "http" };
            let port = if spec.ssl_enabled {
                443
            } else {
                super::config::http_port()
            };
            let output = std::process::Command::new("curl.exe")
                .args([
                    "-skL",
                    "-o",
                    "NUL",
                    "-w",
                    "%{http_code}",
                    "--resolve",
                ])
                .arg(format!("{}:{}:127.0.0.1", spec.domain, port))
                .arg(format!("{scheme}://{}/", spec.domain))
                .output()
                .expect("curl should run");
            let status = String::from_utf8_lossy(&output.stdout);
            eprintln!("{} -> {status}", spec.domain);
            assert!(
                status.starts_with('2') || status.starts_with("30"),
                "{} returned {status}",
                spec.domain
            );
        }

        // Stop what we started so the test does not leave processes behind.
        supervisor.stop_all();
        std::thread::sleep(std::time::Duration::from_millis(400));
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
            php_ini_overrides: String::new(),
            vhost: true,
            worker: true,
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
