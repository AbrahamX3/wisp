//! `wisp --mcp`: the stdio MCP server a client launches.
//!
//! This process owns no state. Every tool call is forwarded over loopback to
//! the running Wisp app, where nginx and PHP actually live. If the app is not
//! running the bridge starts it (the setting must be enabled) and waits for the
//! endpoint to appear.
//!
//! **stdout belongs to the MCP protocol.** Diagnostics go to stderr only.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, GetPromptRequestParams, GetPromptResult, Implementation,
    ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult, PaginatedRequestParams,
    Prompt, PromptArgument, PromptMessage, ReadResourceRequestParams, ReadResourceResult, Resource,
    ResourceContents, ResourceTemplate, Role, ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::transport::stdio;
use rmcp::{tool, tool_handler, tool_router, ErrorData, RoleServer, ServerHandler, ServiceExt};

use super::endpoint;
use super::protocol::{
    AllServicesParams, CreateProjectParams, GetPhpIniParams, GetResourcesParams,
    InstallRuntimeParams, ProjectOnlyParams, ProjectRef, ReadLogParams, SetActiveNginxParams,
    SetPhpIniParams, SetProjectHttpsParams, SetProjectPhpParams, SetServiceParams,
    UninstallRuntimeParams, UpdateProjectParams,
};

/// A local call is cheap; an action that waits on a sync or a UAC prompt is
/// not. Anything longer than this means the app is wedged.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the bridge waits for a freshly launched app to expose its endpoint.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(25);

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Entry point for `wisp --mcp`. Returns a process exit code.
pub fn run() -> i32 {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "wisp --mcp: no async runtime: {error}");
            return 1;
        }
    };

    runtime.block_on(async {
        match serve().await {
            Ok(()) => 0,
            Err(error) => {
                let _ = writeln!(std::io::stderr(), "wisp --mcp: {error}");
                1
            }
        }
    })
}

async fn serve() -> Result<(), Box<dyn std::error::Error>> {
    let client = Arc::new(WispClient::new()?);
    let server = WispServer { client };
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Loopback client
// ---------------------------------------------------------------------------

struct WispClient {
    exe: std::path::PathBuf,
}

enum CallFailure {
    /// The endpoint could not be reached (no app, or a stale discovery file).
    Unreachable(String),
    /// The app answered with an error it chose to show.
    Remote { code: String, message: String },
}

impl CallFailure {
    fn message(&self) -> String {
        match self {
            CallFailure::Unreachable(message) => message.clone(),
            CallFailure::Remote { message, .. } => message.clone(),
        }
    }

    fn into_result(self) -> CallToolResult {
        CallToolResult::error(vec![ContentBlock::text(self.message())])
    }

    fn into_error_data(self) -> ErrorData {
        match self {
            CallFailure::Remote { code, message } if code == "not_found" => {
                ErrorData::invalid_params(message, None)
            }
            other => ErrorData::internal_error(other.message(), None),
        }
    }
}

impl WispClient {
    fn new() -> std::io::Result<Self> {
        Ok(Self {
            exe: std::env::current_exe()?,
        })
    }

    /// Forward one request, starting Wisp first when it is not running.
    fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, CallFailure> {
        match Self::try_call(method, &params, CALL_TIMEOUT) {
            Ok(value) => return Ok(value),
            Err(error @ CallFailure::Remote { .. }) => return Err(error),
            Err(CallFailure::Unreachable(_)) => {}
        }

        Self::ensure_enabled()?;
        self.launch()?;

        let deadline = Instant::now() + LAUNCH_TIMEOUT;
        loop {
            match Self::try_call("ping", &serde_json::json!({}), Duration::from_secs(3)) {
                Ok(_) => break,
                Err(CallFailure::Remote { code, message }) => {
                    return Err(CallFailure::Remote { code, message });
                }
                Err(CallFailure::Unreachable(_)) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(250));
                }
                Err(CallFailure::Unreachable(message)) => {
                    return Err(CallFailure::Unreachable(format!(
                        "Wisp did not start its MCP endpoint. {message}"
                    )));
                }
            }
        }

        Self::try_call(method, &params, CALL_TIMEOUT)
    }

    async fn call_async(
        self: Arc<Self>,
        method: &'static str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, CallFailure> {
        tokio::task::spawn_blocking(move || self.call(method, params))
            .await
            .unwrap_or_else(|error| {
                Err(CallFailure::Unreachable(format!(
                    "The MCP bridge task stopped: {error}"
                )))
            })
    }

    fn try_call(
        method: &str,
        params: &serde_json::Value,
        read_timeout: Duration,
    ) -> Result<serde_json::Value, CallFailure> {
        let info = endpoint::read_discovery().map_err(|error| {
            CallFailure::Unreachable(format!(
                "Wisp is not running or its MCP server is off ({error})."
            ))
        })?;

        let address = SocketAddr::from(([127, 0, 0, 1], info.port));
        let mut stream =
            TcpStream::connect_timeout(&address, Duration::from_secs(2)).map_err(|error| {
                CallFailure::Unreachable(format!(
                    "Could not reach Wisp on port {} ({error}).",
                    info.port
                ))
            })?;
        let _ = stream.set_read_timeout(Some(read_timeout));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));

        let request = super::protocol::Request {
            token: info.token,
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            method: method.to_string(),
            params: params.clone(),
        };
        let mut line = serde_json::to_string(&request).map_err(|error| {
            CallFailure::Unreachable(format!("Could not encode the request: {error}"))
        })?;
        line.push('\n');
        stream.write_all(line.as_bytes()).map_err(|error| {
            CallFailure::Unreachable(format!("Could not send the request to Wisp: {error}"))
        })?;

        let mut reply = String::new();
        BufReader::new(&stream)
            .read_line(&mut reply)
            .map_err(|error| CallFailure::Unreachable(format!("Wisp did not reply: {error}")))?;
        if reply.trim().is_empty() {
            return Err(CallFailure::Unreachable(
                "Wisp closed the MCP connection without replying.".to_string(),
            ));
        }

        let response: super::protocol::Response =
            serde_json::from_str(reply.trim()).map_err(|error| {
                CallFailure::Unreachable(format!(
                    "Wisp's MCP endpoint sent an unexpected reply: {error}"
                ))
            })?;
        if response.ok {
            Ok(response.result.unwrap_or(serde_json::Value::Null))
        } else {
            let error = response.error.unwrap_or(super::protocol::ErrorBody {
                code: "internal".to_string(),
                message: "Wisp returned an error without a message.".to_string(),
            });
            Err(CallFailure::Remote {
                code: error.code,
                message: error.message,
            })
        }
    }

    /// MCP is off by default; launching the app would not help when it is off.
    fn ensure_enabled() -> Result<(), CallFailure> {
        let settings = crate::settings::Settings::load();
        if settings.mcp_enabled {
            return Ok(());
        }
        Err(CallFailure::Unreachable(
            "MCP is turned off in Wisp. Open Wisp and enable Settings \u{2192} MCP \u{2192} Local MCP server, then try again."
                .to_string(),
        ))
    }

    fn launch(&self) -> Result<(), CallFailure> {
        if crate::platform::spawn_detached(&self.exe) {
            Ok(())
        } else {
            Err(CallFailure::Unreachable(format!(
                "Could not start Wisp ({}).",
                self.exe.display()
            )))
        }
    }
}

fn params<T: serde::Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
}

fn pretty(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

/// A browser-like request to one of Wisp's local domains.
///
/// Runs in the bridge rather than the app: a connection that hangs for ten
/// seconds must not stop the UI from repainting. The CA is read from the same
/// config root, so Wisp's own certificates verify without trusting the OS store.
fn probe_site(url: &str, ssl: bool, ca: &std::path::Path) -> Result<serde_json::Value, String> {
    let mut builder = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(10)));
    if ssl {
        let pem = std::fs::read(ca).map_err(|error| {
            format!(
                "Wisp's CA certificate is missing ({}). Start the site once to create it: {error}",
                ca.display()
            )
        })?;
        let certificate = ureq::tls::Certificate::from_pem(&pem)
            .map_err(|error| format!("Wisp's CA certificate could not be read: {error}"))?;
        let tls = ureq::tls::TlsConfig::builder()
            .root_certs(ureq::tls::RootCerts::new_with_certs(&[certificate]))
            .build();
        builder = builder.tls_config(tls);
    }
    let agent = builder.build().new_agent();

    let started = Instant::now();
    let mut response = agent
        .get(url)
        .call()
        .map_err(|error| format!("{url} could not be reached: {error}"))?;
    let status = response.status().as_u16();
    let mut headers = serde_json::Map::new();
    for (name, value) in response.headers().iter() {
        headers.insert(
            name.as_str().to_string(),
            serde_json::Value::String(value.to_str().unwrap_or_default().to_string()),
        );
    }
    let body_preview = response
        .body_mut()
        .with_config()
        .limit(8192)
        .read_to_string()
        .unwrap_or_default();

    Ok(serde_json::json!({
        "url": url,
        "status": status,
        "ok": (200..400).contains(&status),
        "headers": headers,
        "body_preview": body_preview,
        "elapsed_ms": started.elapsed().as_millis(),
    }))
}

// ---------------------------------------------------------------------------
// MCP server
// ---------------------------------------------------------------------------

struct WispServer {
    client: Arc<WispClient>,
}

impl WispServer {
    async fn fetch(
        &self,
        method: &'static str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, CallFailure> {
        self.client.clone().call_async(method, params).await
    }

    /// The common shape of a read-only tool: forward, then pretty-print JSON.
    async fn tool(
        &self,
        method: &'static str,
        params: serde_json::Value,
    ) -> Result<CallToolResult, ErrorData> {
        match self.fetch(method, params).await {
            Ok(value) => Ok(CallToolResult::success(vec![ContentBlock::text(pretty(
                &value,
            ))])),
            Err(error) => Ok(error.into_result()),
        }
    }

    /// Wait until the app has finished a serve/stop operation.
    async fn settle_serving(&self) -> Result<serde_json::Value, CallFailure> {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let state = self
                .fetch("get_operation_state", serde_json::json!({}))
                .await?;
            if !state["serve_busy"].as_bool().unwrap_or(false) {
                if let Some(error) = state["serve_error"].as_str() {
                    return Err(CallFailure::Remote {
                        code: "serve_error".to_string(),
                        message: error.to_string(),
                    });
                }
                return Ok(state);
            }
            if Instant::now() > deadline {
                return Err(CallFailure::Remote {
                    code: "timeout".to_string(),
                    message: "Wisp is still starting or stopping services; check `wisp_status` in a moment."
                        .to_string(),
                });
            }
        }
    }

    /// Wait for a runtime install/removal to finish, then confirm the outcome.
    async fn settle_runtime(
        &self,
        kind: &str,
        version: &str,
        expect_installed: bool,
    ) -> Result<serde_json::Value, CallFailure> {
        let deadline = Instant::now() + Duration::from_secs(15 * 60);
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let state = self
                .fetch("get_operation_state", serde_json::json!({}))
                .await?;
            if state["busy_runtime"].is_null() {
                break;
            }
            if Instant::now() > deadline {
                return Err(CallFailure::Remote {
                    code: "timeout".to_string(),
                    message: format!(
                        "{kind} {version} is still installing. Check `list_runtimes` in a few minutes."
                    ),
                });
            }
        }

        let runtimes = self.fetch("list_runtimes", serde_json::json!({})).await?;
        let installed = runtimes[kind]
            .as_array()
            .is_some_and(|list| list.iter().any(|item| item["version"] == version));
        if installed == expect_installed {
            Ok(runtimes)
        } else {
            let state = self
                .fetch("get_operation_state", serde_json::json!({}))
                .await?;
            let error = state["last_install_error"]
                .as_str()
                .unwrap_or(if expect_installed {
                    "The install finished but the version is not listed."
                } else {
                    "The uninstall finished but the version is still listed."
                });
            Err(CallFailure::Remote {
                code: "runtime_error".to_string(),
                message: error.to_string(),
            })
        }
    }
}

#[tool_router]
impl WispServer {
    // ---- read ------------------------------------------------------------

    #[tool(
        description = "Overview of Wisp: app version, whether nginx is running, how many projects are served, the active nginx version, CA trust, hosts-file mode, and any current serving/install/hosts errors. Start here when you do not know the state.",
        annotations(read_only_hint = true)
    )]
    async fn wisp_status(&self) -> Result<CallToolResult, ErrorData> {
        self.tool("wisp_status", serde_json::json!({})).await
    }

    #[tool(
        description = "List every project: id, name, slug, domain, URL, root path, PHP version, HTTPS, and whether nginx and the PHP worker are actually running.",
        annotations(read_only_hint = true)
    )]
    async fn list_projects(&self) -> Result<CallToolResult, ErrorData> {
        self.tool("list_projects", serde_json::json!({})).await
    }

    #[tool(
        description = "Full details for one project: resolved document root, generated config/log paths, php.ini overrides, certificate expiry, and live service state. `project` accepts an id, name, slug, or domain.",
        annotations(read_only_hint = true)
    )]
    async fn get_project(
        &self,
        Parameters(arguments): Parameters<ProjectRef>,
    ) -> Result<CallToolResult, ErrorData> {
        self.tool("get_project", params(&arguments)).await
    }

    #[tool(
        description = "Installed PHP and nginx versions, each with its install path and major.minor line, plus the active nginx version.",
        annotations(read_only_hint = true)
    )]
    async fn list_runtimes(&self) -> Result<CallToolResult, ErrorData> {
        self.tool("list_runtimes", serde_json::json!({})).await
    }

    #[tool(
        description = "Versions available to install, from the PHP and nginx download catalogs. Includes whether each is already installed. The catalog may be loading when Wisp has just started.",
        annotations(read_only_hint = true)
    )]
    async fn list_available_runtimes(&self) -> Result<CallToolResult, ErrorData> {
        self.tool("list_available_runtimes", serde_json::json!({}))
            .await
    }

    #[tool(
        description = "HTTPS certificates Wisp issued, per domain: certificate path, expiry timestamp, days remaining, and whether Wisp's local CA is trusted.",
        annotations(read_only_hint = true)
    )]
    async fn get_certificates(&self) -> Result<CallToolResult, ErrorData> {
        self.tool("get_certificates", serde_json::json!({})).await
    }

    #[tool(
        description = "The block Wisp manages in the Windows hosts file, its path, and the parsed entries. Empty entries mean the .test domains will not resolve.",
        annotations(read_only_hint = true)
    )]
    async fn get_hosts_block(&self) -> Result<CallToolResult, ErrorData> {
        self.tool("get_hosts_block", serde_json::json!({})).await
    }

    #[tool(
        description = "Read the newest lines of a project's log. `kind` must be one of: access (nginx access log), error (nginx error log), php (PHP errors), worker (php-cgi worker stdout/stderr). Optional `grep` is applied to the newest 20,000 lines before the `lines` limit, so a match deep in the file is still found. `level` filters error-like logs to fatal/warning/notice, or the access log to 4xx/5xx. Returns the matching lines as plain text.",
        annotations(read_only_hint = true)
    )]
    async fn read_log(
        &self,
        Parameters(arguments): Parameters<ReadLogParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let value = match self.fetch("read_log", params(&arguments)).await {
            Ok(value) => value,
            Err(error) => return Ok(error.into_result()),
        };

        let header = format!(
            "{} log for {} \u{2014} {} ({} line(s) of {} scanned{})",
            value["kind"].as_str().unwrap_or("log"),
            value["project"].as_str().unwrap_or("project"),
            value["path"].as_str().unwrap_or(""),
            value["shown"].as_u64().unwrap_or(0),
            value["scanned"].as_u64().unwrap_or(0),
            value["filter"]
                .as_str()
                .map(|filter| format!(", filter: {filter}"))
                .unwrap_or_default(),
        );
        let lines = value["lines"]
            .as_array()
            .map(|lines| {
                lines
                    .iter()
                    .filter_map(|line| line.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();

        let text = if lines.is_empty() {
            format!("{header}\n\nNo matching lines.")
        } else {
            format!("{header}\n\n{lines}")
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Latest CPU and RAM samples for nginx, each running PHP worker, and the whole machine. Optionally limit to one project.",
        annotations(read_only_hint = true)
    )]
    async fn get_resources(
        &self,
        Parameters(arguments): Parameters<GetResourcesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.tool("get_resources", params(&arguments)).await
    }

    #[tool(
        description = "A project's php.ini: its custom directives and the diff against Wisp's generated file, which is what shows whether a limit or extension is set. Pass `full: true` to include the entire effective file (hundreds of lines).",
        annotations(read_only_hint = true)
    )]
    async fn get_php_ini(
        &self,
        Parameters(arguments): Parameters<GetPhpIniParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.tool("get_php_ini", params(&arguments)).await
    }

    #[tool(
        description = "The generated nginx server block for a project, with its path. Empty when the project has never been served.",
        annotations(read_only_hint = true)
    )]
    async fn read_nginx_config(
        &self,
        Parameters(arguments): Parameters<ProjectOnlyParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let value = match self.fetch("read_nginx_config", params(&arguments)).await {
            Ok(value) => value,
            Err(error) => return Ok(error.into_result()),
        };
        let path = value["path"].as_str().unwrap_or("");
        let contents = value["contents"].as_str().unwrap_or("");
        let text = if contents.is_empty() {
            format!("{path}\n\n(no generated server block yet \u{2014} start the project first)")
        } else {
            format!("{path}\n\n{contents}")
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Request the project's URL through its local domain the way a browser would, and report the status code, headers, and a short body preview. This exercises nginx, the hosts entry, TLS, and PHP together. The probe runs in the bridge, so it does not freeze Wisp and 4xx/5xx responses are reported as results.",
        annotations(read_only_hint = true)
    )]
    async fn check_site(
        &self,
        Parameters(arguments): Parameters<ProjectOnlyParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let details = match self.fetch("get_project", params(&arguments)).await {
            Ok(details) => details,
            Err(error) => return Ok(error.into_result()),
        };
        let Some(url) = details["probe_url"].as_str().map(str::to_string) else {
            return Ok(CallFailure::Remote {
                code: "internal".to_string(),
                message: "Wisp returned a project without a probe URL.".to_string(),
            }
            .into_result());
        };
        let ssl = details["ssl_enabled"].as_bool().unwrap_or(false);
        let ca = crate::tls::ca_cert();

        let outcome = tokio::task::spawn_blocking(move || probe_site(&url, ssl, &ca))
            .await
            .unwrap_or_else(|error| Err(format!("The site probe stopped: {error}")));
        match outcome {
            Ok(value) => Ok(CallToolResult::success(vec![ContentBlock::text(pretty(
                &value,
            ))])),
            Err(message) => Ok(CallFailure::Remote {
                code: "site_error".to_string(),
                message,
            }
            .into_result()),
        }
    }

    #[tool(
        description = "Where Wisp keeps everything: config root, database, generated sites, logs, certificates, CA, runtimes, the hosts file, and the MCP discovery file.",
        annotations(read_only_hint = true)
    )]
    async fn get_config_paths(&self) -> Result<CallToolResult, ErrorData> {
        self.tool("get_config_paths", serde_json::json!({})).await
    }

    #[tool(
        description = "Current settings relevant to automation: theme, background mode, autostart, active nginx, CA trust, onboarding, and whether MCP is enabled.",
        annotations(read_only_hint = true)
    )]
    async fn get_settings(&self) -> Result<CallToolResult, ErrorData> {
        self.tool("get_settings", serde_json::json!({})).await
    }

    // ---- act -------------------------------------------------------------

    #[tool(
        description = "Serve a project: add its nginx server block and start its PHP worker. Waits for the operation to finish and returns the project's resulting state.",
        annotations(destructive_hint = false)
    )]
    async fn start_project(
        &self,
        Parameters(arguments): Parameters<ProjectRef>,
    ) -> Result<CallToolResult, ErrorData> {
        match self.fetch("start_project", params(&arguments)).await {
            Ok(_) => self.after_serving(&arguments.project).await,
            Err(error) => Ok(error.into_result()),
        }
    }

    #[tool(
        description = "Stop both of a project's services (its nginx server block and PHP worker). Other projects keep running.",
        annotations(destructive_hint = true)
    )]
    async fn stop_project(
        &self,
        Parameters(arguments): Parameters<ProjectRef>,
    ) -> Result<CallToolResult, ErrorData> {
        match self.fetch("stop_project", params(&arguments)).await {
            Ok(_) => self.after_serving(&arguments.project).await,
            Err(error) => Ok(error.into_result()),
        }
    }

    #[tool(
        description = "Restart a project's PHP worker. nginx keeps its server block and reconnects to the new worker automatically.",
        annotations(destructive_hint = false)
    )]
    async fn restart_project(
        &self,
        Parameters(arguments): Parameters<ProjectRef>,
    ) -> Result<CallToolResult, ErrorData> {
        match self.fetch("restart_project", params(&arguments)).await {
            Ok(_) => self.after_serving(&arguments.project).await,
            Err(error) => Ok(error.into_result()),
        }
    }

    #[tool(
        description = "Start, stop, or restart one service of a project. `service` is `nginx` (the shared nginx server block) or `php` (the project's FastCGI worker).",
        annotations(destructive_hint = false)
    )]
    async fn set_service(
        &self,
        Parameters(arguments): Parameters<SetServiceParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match self.fetch("set_service", params(&arguments)).await {
            Ok(_) => self.after_serving(&arguments.project).await,
            Err(error) => Ok(error.into_result()),
        }
    }

    #[tool(
        description = "Start, stop, or restart every site. Start serves every project that has a PHP version set; stop tears down all workers and nginx.",
        annotations(destructive_hint = false)
    )]
    async fn all_services(
        &self,
        Parameters(arguments): Parameters<AllServicesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match self.fetch("all_services", params(&arguments)).await {
            Ok(_) => match self.settle_serving().await {
                Ok(_) => self.tool("wisp_status", serde_json::json!({})).await,
                Err(error) => Ok(error.into_result()),
            },
            Err(error) => Ok(error.into_result()),
        }
    }

    #[tool(
        description = "Download and install a PHP or nginx version. A newly installed nginx becomes the active version. This waits for the download and unpack to finish, which can take several minutes, and returns the updated installed list. Use list_available_runtimes for exact versions.",
        annotations(destructive_hint = false)
    )]
    async fn install_runtime(
        &self,
        Parameters(arguments): Parameters<InstallRuntimeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let kind = arguments.kind.as_str();
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            match self.fetch("install_runtime", params(&arguments)).await {
                Ok(_) => break,
                Err(CallFailure::Remote { code, message }) if code == "catalog_loading" => {
                    if Instant::now() > deadline {
                        return Ok(CallFailure::Remote { code, message }.into_result());
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                Err(error) => return Ok(error.into_result()),
            }
        }

        match self.settle_runtime(kind, &arguments.version, true).await {
            Ok(runtimes) => Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "Installed {kind} {}.\n\n{}",
                arguments.version,
                pretty(&runtimes)
            ))])),
            Err(error) => Ok(error.into_result()),
        }
    }

    #[tool(
        description = "Remove an installed PHP or nginx version. Projects still pointing at it are left alone; set them to another version before removing the one they use.",
        annotations(destructive_hint = true)
    )]
    async fn uninstall_runtime(
        &self,
        Parameters(arguments): Parameters<UninstallRuntimeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let kind = arguments.kind.as_str();
        match self.fetch("uninstall_runtime", params(&arguments)).await {
            Ok(_) => match self.settle_runtime(kind, &arguments.version, false).await {
                Ok(runtimes) => Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "Removed {kind} {}.\n\n{}",
                    arguments.version,
                    pretty(&runtimes)
                ))])),
                Err(error) => Ok(error.into_result()),
            },
            Err(error) => Ok(error.into_result()),
        }
    }

    #[tool(
        description = "Choose which installed nginx version serves every site. If nginx is running it restarts on the new version."
    )]
    async fn set_active_nginx(
        &self,
        Parameters(arguments): Parameters<SetActiveNginxParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match self.fetch("set_active_nginx", params(&arguments)).await {
            Ok(_) => match self.settle_serving().await {
                Ok(_) => self.tool("list_runtimes", serde_json::json!({})).await,
                Err(error) => Ok(error.into_result()),
            },
            Err(error) => Ok(error.into_result()),
        }
    }

    #[tool(
        description = "Point a project at an installed PHP version (or clear it with an empty string). Restarts the project's worker if it is running.",
        annotations(destructive_hint = false)
    )]
    async fn set_project_php(
        &self,
        Parameters(arguments): Parameters<SetProjectPhpParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match self.fetch("set_project_php", params(&arguments)).await {
            Ok(_) => self.after_serving(&arguments.project).await,
            Err(error) => Ok(error.into_result()),
        }
    }

    #[tool(
        description = "Turn HTTPS on or off for a project. Turning it on issues a certificate for the domain and adds the TLS server block; turning it off leaves the certificate on disk but serves plain HTTP.",
        annotations(destructive_hint = false)
    )]
    async fn set_project_https(
        &self,
        Parameters(arguments): Parameters<SetProjectHttpsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match self.fetch("set_project_https", params(&arguments)).await {
            Ok(_) => self.after_serving(&arguments.project).await,
            Err(error) => Ok(error.into_result()),
        }
    }

    #[tool(
        description = "Replace a project's php.ini directives. Pass raw directives, one per line (for example `memory_limit = 1G`); this replaces the whole override block. Restarts the worker so the change applies.",
        annotations(destructive_hint = false)
    )]
    async fn set_php_ini_overrides(
        &self,
        Parameters(arguments): Parameters<SetPhpIniParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match self
            .fetch("set_php_ini_overrides", params(&arguments))
            .await
        {
            Ok(_) => self.after_serving(&arguments.project).await,
            Err(error) => Ok(error.into_result()),
        }
    }

    #[tool(
        description = "Remove all of a project's custom php.ini directives, returning it to Wisp's generated file. Restarts the worker.",
        annotations(destructive_hint = true)
    )]
    async fn reset_php_ini(
        &self,
        Parameters(arguments): Parameters<ProjectOnlyParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match self.fetch("reset_php_ini", params(&arguments)).await {
            Ok(_) => self.after_serving(&arguments.project).await,
            Err(error) => Ok(error.into_result()),
        }
    }

    #[tool(
        description = "Rewrite Wisp's block in the Windows hosts file from the projects that should be served. May show a UAC prompt; waits for it to finish."
    )]
    async fn repair_hosts(&self) -> Result<CallToolResult, ErrorData> {
        match self.fetch("repair_hosts", serde_json::json!({})).await {
            Ok(_) => {
                let deadline = Instant::now() + Duration::from_secs(150);
                loop {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let state = match self
                        .fetch("get_operation_state", serde_json::json!({}))
                        .await
                    {
                        Ok(state) => state,
                        Err(error) => return Ok(error.into_result()),
                    };
                    let busy = state["hosts_busy"].as_bool().unwrap_or(false)
                        || state["elevation_busy"].as_bool().unwrap_or(false);
                    if busy {
                        if Instant::now() > deadline {
                            return Ok(CallFailure::Remote {
                                code: "timeout".to_string(),
                                message: "Wisp is waiting for the hosts-file permission prompt."
                                    .to_string(),
                            }
                            .into_result());
                        }
                        continue;
                    }
                    if let Some(error) = state["hosts_error"].as_str() {
                        return Ok(CallFailure::Remote {
                            code: "hosts_error".to_string(),
                            message: error.to_string(),
                        }
                        .into_result());
                    }
                    return self.tool("get_hosts_block", serde_json::json!({})).await;
                }
            }
            Err(error) => Ok(error.into_result()),
        }
    }

    #[tool(
        description = "Add Wisp's local certificate authority to the current user's trusted roots. May show a Windows confirmation prompt; waits for it to finish."
    )]
    async fn trust_ca(&self) -> Result<CallToolResult, ErrorData> {
        match self.fetch("trust_ca", serde_json::json!({})).await {
            Ok(_) => {
                let deadline = Instant::now() + Duration::from_secs(90);
                loop {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    let state = match self
                        .fetch("get_operation_state", serde_json::json!({}))
                        .await
                    {
                        Ok(state) => state,
                        Err(error) => return Ok(error.into_result()),
                    };
                    if state["trust_busy"].as_bool().unwrap_or(false) {
                        if Instant::now() > deadline {
                            return Ok(CallFailure::Remote {
                                code: "timeout".to_string(),
                                message: "Wisp is waiting for the certificate confirmation prompt."
                                    .to_string(),
                            }
                            .into_result());
                        }
                        continue;
                    }
                    return self.tool("get_certificates", serde_json::json!({})).await;
                }
            }
            Err(error) => Ok(error.into_result()),
        }
    }

    #[tool(
        description = "Create a project record. Does not start it; call start_project afterwards. The domain defaults to the slug plus .test, and HTTPS defaults to on. Files on disk are not created or modified.",
        annotations(destructive_hint = false)
    )]
    async fn create_project(
        &self,
        Parameters(arguments): Parameters<CreateProjectParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match self.fetch("create_project", params(&arguments)).await {
            Ok(value) => Ok(CallToolResult::success(vec![ContentBlock::text(pretty(
                &value,
            ))])),
            Err(error) => Ok(error.into_result()),
        }
    }

    #[tool(
        description = "Change a project's name, slug, root folder, domain, PHP version, HTTPS setting, or php.ini overrides. Only supplied fields change. Restarts services when needed.",
        annotations(destructive_hint = false)
    )]
    async fn update_project(
        &self,
        Parameters(arguments): Parameters<UpdateProjectParams>,
    ) -> Result<CallToolResult, ErrorData> {
        // The project key may have changed (a rename or domain edit), so the
        // app's returned detail is the only safe thing to hand back. Fetching
        // again with the old key would report a false "not found".
        self.tool("update_project", params(&arguments)).await
    }

    #[tool(
        description = "Remove a project from Wisp and stop its services. Files on disk are untouched. This cannot be undone through the MCP tools.",
        annotations(destructive_hint = true)
    )]
    async fn delete_project(
        &self,
        Parameters(arguments): Parameters<ProjectRef>,
    ) -> Result<CallToolResult, ErrorData> {
        match self.fetch("delete_project", params(&arguments)).await {
            Ok(deleted) => match self.settle_serving().await {
                Ok(_) => match self.fetch("list_projects", serde_json::json!({})).await {
                    Ok(projects) => Ok(CallToolResult::success(vec![ContentBlock::text(pretty(
                        &serde_json::json!({
                            "deleted": deleted,
                            "projects": projects,
                        }),
                    ))])),
                    Err(error) => Ok(error.into_result()),
                },
                Err(error) => Ok(error.into_result()),
            },
            Err(error) => Ok(error.into_result()),
        }
    }

    // ---- shared ----------------------------------------------------------

    async fn after_serving(&self, project: &str) -> Result<CallToolResult, ErrorData> {
        match self.settle_serving().await {
            Ok(_) => {
                self.tool("get_project", serde_json::json!({ "project": project }))
                    .await
            }
            Err(error) => Ok(error.into_result()),
        }
    }
}

#[tool_handler]
impl ServerHandler for WispServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_prompts()
                .build(),
        )
        .with_server_info(Implementation::new("wisp", env!("CARGO_PKG_VERSION")))
        .with_instructions(
            "Wisp manages local PHP sites on Windows: nginx, PHP workers, .test domains and trusted HTTPS. \
             Start with `wisp_status` or `list_projects`. When a site fails, run `check_site` and read the \
             `error` and `php` logs. Action tools wait for the app to settle and return the resulting state.",
        )
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        // Resource listing is also how a client discovers whether Wisp is
        // reachable, so this is where a not-running app surfaces.
        self.fetch("wisp_status", serde_json::json!({}))
            .await
            .map_err(CallFailure::into_error_data)?;
        let projects = self
            .fetch("list_projects", serde_json::json!({}))
            .await
            .map_err(CallFailure::into_error_data)?;

        let mut resources = vec![
            Resource::new("wisp://status", "Wisp status")
                .with_description("App version, running services, and operation state.")
                .with_mime_type("application/json"),
            Resource::new("wisp://projects", "Projects")
                .with_description("Every project in Wisp, as JSON.")
                .with_mime_type("application/json"),
        ];
        if let Some(projects) = projects.as_array() {
            for project in projects {
                let id = project["id"].as_i64().unwrap_or_default();
                let name = project["name"].as_str().unwrap_or("project");
                let domain = project["domain"].as_str().unwrap_or("");
                resources.push(
                    Resource::new(format!("wisp://projects/{id}"), name)
                        .with_description(format!("{name} ({domain})"))
                        .with_mime_type("application/json"),
                );
            }
        }
        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        Ok(ListResourceTemplatesResult::with_all_items(vec![
            ResourceTemplate::new("wisp://projects/{project}", "Project details")
                .with_description("One project by id, name, slug, or domain.")
                .with_mime_type("application/json"),
            ResourceTemplate::new("wisp://projects/{project}/logs/{kind}", "Project log")
                .with_description(
                    "The newest 200 lines of a project log: access, error, php, or worker.",
                )
                .with_mime_type("text/plain"),
        ]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::ReadResourceResponse, ErrorData> {
        let uri = request.uri.clone();
        let rest = uri.strip_prefix("wisp://").ok_or_else(|| {
            ErrorData::resource_not_found(format!("Not a Wisp resource: {uri}"), None)
        })?;

        let (method, params) = match rest {
            "status" => ("wisp_status", serde_json::json!({})),
            "projects" => ("list_projects", serde_json::json!({})),
            _ => {
                let Some(path) = rest.strip_prefix("projects/") else {
                    return Err(ErrorData::resource_not_found(
                        format!("Unknown Wisp resource: {uri}"),
                        None,
                    ));
                };
                let mut parts = path.splitn(2, '/');
                let project = parts.next().unwrap_or_default();
                match parts.next() {
                    None => ("get_project", serde_json::json!({ "project": project })),
                    Some(tail) => {
                        let Some(kind) = tail.strip_prefix("logs/") else {
                            return Err(ErrorData::resource_not_found(
                                format!("Unknown Wisp resource: {uri}"),
                                None,
                            ));
                        };
                        (
                            "read_log",
                            serde_json::json!({
                                "project": project,
                                "kind": kind,
                                "lines": 200
                            }),
                        )
                    }
                }
            }
        };

        let value = self
            .fetch(method, params)
            .await
            .map_err(CallFailure::into_error_data)?;

        let (text, mime) = if method == "read_log" {
            let lines = value["lines"]
                .as_array()
                .map(|lines| {
                    lines
                        .iter()
                        .filter_map(|line| line.as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            (lines, "text/plain")
        } else {
            (pretty(&value), "application/json")
        };

        Ok(ReadResourceResult::new(vec![
            ResourceContents::text(text, uri.clone()).with_mime_type(mime)
        ])
        .into())
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        Ok(ListPromptsResult::with_all_items(vec![Prompt::new(
            "diagnose-site",
            Some("Investigate why a Wisp site is not loading."),
            Some(vec![PromptArgument::new("project")
                .with_description("Project id, name, slug, or domain.")
                .with_required(true)]),
        )]))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::GetPromptResponse, ErrorData> {
        if request.name != "diagnose-site" {
            return Err(ErrorData::invalid_params(
                format!("Unknown prompt: {}", request.name),
                None,
            ));
        }
        let project = request
            .arguments
            .as_ref()
            .and_then(|arguments| arguments.get("project"))
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                ErrorData::invalid_params("diagnose-site needs a `project` argument.", None)
            })?;

        let details = self
            .fetch("get_project", serde_json::json!({ "project": project }))
            .await
            .map_err(CallFailure::into_error_data)?;
        let error_log = self
            .fetch(
                "read_log",
                serde_json::json!({ "project": project, "kind": "error", "lines": 40 }),
            )
            .await
            .ok();
        let php_log = self
            .fetch(
                "read_log",
                serde_json::json!({ "project": project, "kind": "php", "lines": 40 }),
            )
            .await
            .ok();

        let excerpt = |value: &Option<serde_json::Value>| {
            value
                .as_ref()
                .and_then(|value| value["lines"].as_array())
                .map(|lines| {
                    lines
                        .iter()
                        .filter_map(|line| line.as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| "(no recent lines)".to_string())
        };

        let text = format!(
            "Diagnose this Wisp site.\n\n\
             Project:\n{details}\n\n\
             Recent nginx error log:\n{nginx_errors}\n\n\
             Recent PHP error log:\n{php_errors}\n\n\
             Work through these checks and then report a ranked cause with evidence:\n\
             1. `wisp_status` and `get_project`: are nginx and the PHP worker running, and does the PHP version exist?\n\
             2. `check_site`: does the domain resolve, connect, and return the expected status?\n\
             3. If `check_site` fails: `get_hosts_block` (missing entry?), `get_certificates` (expired or untrusted?), \
             and `read_nginx_config` (wrong root or FastCGI port?).\n\
             4. If it returns 502/500: read the `error`, `php`, and `worker` logs. Run `get_php_ini` if an extension or limit is suspected.\n\
             5. `get_resources` to see whether a process is pegged or missing.\n\n\
             Use the action tools when a fix is clear, and say what you changed. Do not delete projects or uninstall runtimes without asking.",
            details = pretty(&details),
            nginx_errors = excerpt(&error_log),
            php_errors = excerpt(&php_log),
        );

        Ok(
            GetPromptResult::new(vec![PromptMessage::new_text(Role::User, text)])
                .with_description(format!("Diagnose {project}"))
                .into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_router_exposes_every_tool() {
        let router = WispServer::tool_router();
        let names: Vec<String> = router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        for expected in [
            "wisp_status",
            "list_projects",
            "get_project",
            "read_log",
            "install_runtime",
            "create_project",
            "delete_project",
            "check_site",
        ] {
            assert!(
                names.iter().any(|name| name == expected),
                "missing tool {expected}; got {names:?}"
            );
        }
    }
}
