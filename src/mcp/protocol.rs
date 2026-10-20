//! Wire types shared by the app's loopback endpoint and the `wisp --mcp`
//! bridge.
//!
//! The app owns every piece of live state (nginx and PHP handles, ports, the
//! install queue), so the MCP bridge is deliberately thin: it speaks MCP over
//! stdio, forwards each tool call to the running app over a loopback socket,
//! and renders the JSON reply. These types are the contract between the two
//! halves and are also where the tool argument schemas come from.

use serde::{Deserialize, Serialize};

/// Bumped when the loopback protocol changes incompatibly. The bridge refuses
/// to talk to an app that disagrees, which is what happens when an old app is
/// still running after an update.
pub const PROTOCOL_VERSION: u32 = 1;

/// Name of the discovery file under the config root.
pub const DISCOVERY_FILE: &str = "mcp.json";

/// What the app writes to `mcp.json` so the bridge can find it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointInfo {
    pub version: u32,
    pub port: u16,
    /// Per-launch secret. Any local process can connect to the port; only the
    /// holder of this token can make the app act.
    pub token: String,
    pub pid: u32,
    pub app_version: String,
}

/// One request on the loopback socket, sent as a single JSON line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub token: String,
    #[serde(default)]
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

/// One reply, sent as a single JSON line and then the connection is closed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

impl Response {
    pub fn ok(id: u64, result: serde_json::Value) -> Self {
        Self {
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: u64, code: &str, message: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(ErrorBody {
                code: code.to_string(),
                message: message.into(),
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    /// Stable machine-readable kind (`not_found`, `invalid_params`, ...).
    pub code: String,
    /// Human-readable explanation, safe to show to the user.
    pub message: String,
}

// ---------------------------------------------------------------------------
// Tool arguments
//
// Deriving `JsonSchema` here (rather than in the bridge) keeps the MCP tool
// schemas and the endpoint's parsing in lockstep: there is one definition of
// "what a read_log call looks like".
// ---------------------------------------------------------------------------

/// A project can be named by id, name, slug, or domain.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ProjectRef {
    /// Project id, name, slug (`app_name`), or domain. Ambiguous or unknown
    /// values are rejected with the list of projects to choose from.
    pub project: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum LogKindParam {
    /// nginx access log for the project's domain.
    Access,
    /// nginx error log for the project's server block.
    Error,
    /// PHP's own error log (`php-error.log`).
    Php,
    /// The php-cgi worker's stdout/stderr (`php-cgi.log`).
    Worker,
}

/// Arguments for `read_log`.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ReadLogParams {
    /// Project id, name, slug, or domain.
    pub project: String,
    /// Which log to read: `access`, `error`, `php`, or `worker`.
    pub kind: LogKindParam,
    /// How many of the newest lines to return. Defaults to 200, capped at 2000.
    #[serde(default = "default_log_lines")]
    pub lines: u32,
    /// Optional case-insensitive substring filter (made case-sensitive by
    /// `case_sensitive`). Applied before the line limit, so a match deep in the
    /// log is still found.
    #[serde(default)]
    pub grep: Option<String>,
    /// Optional level filter. For error-like logs: `error`, `warning`,
    /// `notice`, `info`, or `all`. For the access log: `4xx`, `5xx`, or `all`.
    #[serde(default)]
    pub level: Option<String>,
    /// Match `grep` case-sensitively.
    #[serde(default)]
    pub case_sensitive: bool,
}

fn default_log_lines() -> u32 {
    200
}

/// Which half of a project's stack an action applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ServiceKind {
    /// nginx's server block (the shared nginx process).
    Nginx,
    /// The project's PHP `FastCGI` worker.
    Php,
}

/// What to do to a service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ServiceAction {
    Start,
    Stop,
    Restart,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SetServiceParams {
    /// Project id, name, slug, or domain.
    pub project: String,
    /// `nginx` or `php`.
    pub service: ServiceKind,
    /// `start`, `stop`, or `restart`.
    pub action: ServiceAction,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct AllServicesParams {
    /// `start`, `stop`, or `restart`. Start serves every project that has a
    /// PHP version set; stop tears everything down.
    pub action: ServiceAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeKindParam {
    Php,
    Nginx,
}

impl RuntimeKindParam {
    pub fn as_str(self) -> &'static str {
        match self {
            RuntimeKindParam::Php => "php",
            RuntimeKindParam::Nginx => "nginx",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct InstallRuntimeParams {
    /// `php` or `nginx`.
    pub kind: RuntimeKindParam,
    /// Exact version from `list_available_runtimes`, e.g. `8.3.35` or `1.27.0`.
    pub version: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct UninstallRuntimeParams {
    /// `php` or `nginx`.
    pub kind: RuntimeKindParam,
    /// Exact installed version from `list_runtimes`.
    pub version: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SetActiveNginxParams {
    /// An installed nginx version. Empty string clears the active choice.
    pub version: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SetProjectPhpParams {
    /// Project id, name, slug, or domain.
    pub project: String,
    /// An installed PHP version, e.g. `8.3.35`. Empty string clears it.
    pub version: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SetProjectHttpsParams {
    /// Project id, name, slug, or domain.
    pub project: String,
    /// Whether the domain gets a trusted certificate and an HTTPS server block.
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SetPhpIniParams {
    /// Project id, name, slug, or domain.
    pub project: String,
    /// Raw php.ini directives, one per line, appended after Wisp's defaults.
    /// This replaces the whole override block; pass an empty string to clear.
    pub overrides: String,
}

/// Arguments for `get_php_ini`.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ProjectOnlyParams {
    /// Project id, name, slug, or domain.
    pub project: String,
}

/// Arguments for `get_php_ini`.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct GetPhpIniParams {
    /// Project id, name, slug, or domain.
    pub project: String,
    /// Include the whole effective `php.ini` (hundreds of lines). Off by
    /// default: the overrides and the diff against Wisp's generated file are
    /// what usually answer the question.
    #[serde(default)]
    pub full: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct GetResourcesParams {
    /// Limit the samples to one project (id, name, slug, or domain). Omit for
    /// every running service plus machine totals.
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct CreateProjectParams {
    /// Display name, e.g. `My Shop`.
    pub name: String,
    /// Absolute path to the project on disk.
    pub root_path: String,
    /// Slug used for the generated config folder and the default domain.
    /// Derived from `name` when omitted.
    #[serde(default)]
    pub app_name: Option<String>,
    /// Domain such as `my-shop.test`. Defaults to the slug plus `.test`.
    #[serde(default)]
    pub domain: Option<String>,
    /// Installed PHP version; omit for a project that serves only static files.
    #[serde(default)]
    pub php_version: Option<String>,
    /// Issue a certificate and serve HTTPS. Defaults to true.
    #[serde(default)]
    pub ssl_enabled: Option<bool>,
    /// Initial php.ini directives, one per line.
    #[serde(default)]
    pub php_ini_overrides: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct UpdateProjectParams {
    /// Project id, name, slug, or domain.
    pub project: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub app_name: Option<String>,
    #[serde(default)]
    pub root_path: Option<String>,
    #[serde(default)]
    pub domain: Option<String>,
    /// An installed PHP version; an empty string clears it.
    #[serde(default)]
    pub php_version: Option<String>,
    #[serde(default)]
    pub ssl_enabled: Option<bool>,
    /// Replacement php.ini directives; an empty string clears them.
    #[serde(default)]
    pub php_ini_overrides: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{LogKindParam, ReadLogParams, Request, Response, ServiceAction, SetServiceParams};

    #[test]
    fn a_request_round_trips_as_one_json_line() {
        let request = Request {
            token: "secret".into(),
            id: 7,
            method: "read_log".into(),
            params: serde_json::json!({"project": "myapp", "kind": "php"}),
        };
        let line = serde_json::to_string(&request).unwrap();
        assert!(!line.contains('\n'));
        let parsed: Request = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed.id, 7);
        assert_eq!(parsed.method, "read_log");
    }

    #[test]
    fn responses_carry_exactly_one_of_result_or_error() {
        let ok = serde_json::to_value(Response::ok(1, serde_json::json!({"ok": true}))).unwrap();
        assert_eq!(ok["ok"], true);
        assert!(ok.get("error").is_none());

        let err = serde_json::to_value(Response::err(2, "not_found", "no such project")).unwrap();
        assert_eq!(err["ok"], false);
        assert_eq!(err["error"]["code"], "not_found");
        assert!(err.get("result").is_none());
    }

    #[test]
    fn log_arguments_default_to_a_recent_tail() {
        let params: ReadLogParams =
            serde_json::from_value(serde_json::json!({"project": "1", "kind": "access"})).unwrap();
        assert_eq!(params.lines, 200);
        assert_eq!(params.kind, LogKindParam::Access);
        assert!(params.grep.is_none());
        assert!(!params.case_sensitive);
    }

    #[test]
    fn service_arguments_deserialize_from_lowercase() {
        let params: SetServiceParams = serde_json::from_value(serde_json::json!({
            "project": "myapp",
            "service": "php",
            "action": "restart"
        }))
        .unwrap();
        assert_eq!(params.action, ServiceAction::Restart);
    }
}
