# Local MCP server

Wisp can expose itself over the [Model Context Protocol](https://modelcontextprotocol.io)
so a CLI coding agent can inspect and control the same running app you use:
list projects, read logs, run `check_site`, start and stop services, install
runtimes, and edit project settings.

The server is **off by default**. Turn it on in **Settings → MCP**.

## How it works

Wisp runs nginx, the PHP workers, and the install queue inside the app process.
A second, standalone controller would fight that supervisor, so the MCP feature
is split in two:

```
MCP client (Claude Code, OpenCode, ...)
        │  stdio JSON-RPC
        ▼
wisp.exe --mcp                 the bridge, one process per client
        │  127.0.0.1, random port, token
        ▼
Wisp (GUI)                     owns projects, nginx, PHP, ports, installs
```

- The app listens on a random loopback port and writes the port plus a
  per-launch token to `%USERPROFILE%\.config\wisp\mcp.json`.
- The bridge is a thin stdio MCP server. It forwards each tool call to the app
  over one JSON line and returns the reply.
- If the app is not running, the bridge starts `wisp.exe` and waits up to 25
  seconds for the endpoint. It only does this when the setting is on.
- The endpoint is loopback-only. The token is the authorization check, so treat
  `mcp.json` as a secret: any local process that can read it can control Wisp.
  Wisp never writes logs or diagnostics to stdout in `--mcp` mode; stdout is the
  MCP protocol.

Because the GUI owns the state, everything an agent sees and changes is the same
state you see in the window. There is no second database and no second nginx.

## Enable it

1. Open **Settings → MCP**.
2. Turn on **Local MCP server**. The page shows the listening port and the
   credentials file.
3. Copy the configuration for your client. The Settings page fills in the real
   path to `wisp.exe`; the snippets below use `<wisp.exe>` as a placeholder.

Turning the setting off closes the port, deletes `mcp.json`, and leaves the app
running normally.

### Custom config directories

`WISP_CONFIG_DIR` moves Wisp's data root (used by tests and portable installs).
If you set it for Wisp, set the same variable for the MCP client too: the bridge
looks for the discovery file and settings under that root. The single-instance
guard is keyed by profile, so a test profile can run alongside the installed
app.

## Client configuration

### Claude Code

```sh
claude mcp add --transport stdio wisp -- "<wisp.exe>" --mcp
```

Add `-s project` to keep the server in the current repository instead of your
user scope.

### OpenCode

Merge into `opencode.json`, or `~/.config/opencode/opencode.json`:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "wisp": {
      "type": "local",
      "command": ["<wisp.exe>", "--mcp"],
      "enabled": true
    }
  }
}
```

### Codex CLI

Merge into `~/.codex/config.toml`:

```toml
[mcp_servers.wisp]
command = "<wisp.exe>"
args = ["--mcp"]
```

### Cursor

Merge into `.cursor/mcp.json`, or `~/.cursor/mcp.json`:

```json
{
  "mcpServers": {
    "wisp": {
      "command": "<wisp.exe>",
      "args": ["--mcp"]
    }
  }
}
```

### VS Code

Merge into `.vscode/mcp.json` in the workspace:

```json
{
  "servers": {
    "wisp": {
      "type": "stdio",
      "command": "<wisp.exe>",
      "args": ["--mcp"]
    }
  }
}
```

### Other clients

Clients that accept the common `mcpServers` shape (Claude Desktop, Windsurf,
and others) can use:

```json
{
  "mcpServers": {
    "wisp": {
      "command": "<wisp.exe>",
      "args": ["--mcp"]
    }
  }
}
```

The Settings page keeps all of these in sync with the path of the running
executable and offers a copy button for each.

## Tools

A `project` argument accepts an id, name, slug, or domain. If it is unknown or
matches more than one project, the error lists the choices.

### Read

| Tool | Arguments | What it returns |
| --- | --- | --- |
| `wisp_status` | — | App version, MCP state, nginx state, running project count, catalog/install/hosts state, and current errors. Start here. |
| `list_projects` | — | Every project with its URL, root, PHP version, and live nginx/PHP state. |
| `get_project` | `project` | Full detail: resolved docroot, generated config/log/ini paths, certificate, overrides, live state. |
| `list_runtimes` | — | Installed PHP and nginx versions, with paths and the active nginx. |
| `list_available_runtimes` | — | Catalog versions with an `installed` flag. May report `loading` while the catalog refreshes. |
| `get_certificates` | — | Issued certificates, expiry and days left, plus the CA path and trust flag. |
| `get_hosts_block` | — | Wisp's managed block in the Windows hosts file, and parsed entries. |
| `read_log` | `project`, `kind`, optional `lines`, `grep`, `level`, `case_sensitive` | The newest matching log lines as text. |
| `get_resources` | optional `project` | CPU/RAM samples for nginx, each PHP worker, and the machine. |
| `get_php_ini` | `project`, optional `full` | Overrides and the diff against Wisp's generated file; `full: true` also returns the whole effective file. |
| `read_nginx_config` | `project` | The generated nginx server block, with its path. |
| `check_site` | `project` | An HTTP(S) request through the real domain: status, headers, body preview, elapsed time. |
| `get_config_paths` | — | Database, configs, logs, certificates, runtimes, hosts file, discovery file. |
| `get_settings` | — | Theme, background/autostart, active nginx, CA trust, onboarding, MCP state. |

### Log kinds

`read_log` uses one tool rather than one per file. `kind` is:

- `access` — nginx access log (`access.log`).
- `error` — nginx error log (`error.log`).
- `php` — PHP's own error log (`php-error.log`).
- `worker` — the php-cgi worker's stdout/stderr (`php-cgi.log`).

`grep` is applied to the newest 20,000 lines (up to 4 MB) *before* the `lines`
limit, so a match older than the last few hundred lines is still found. `level`
accepts `fatal`/`error`, `warning`, `notice`/`info` for error-like logs, and
`4xx`/`5xx` for the access log. `lines` defaults to 200 (maximum 2,000).

### Act

| Tool | Arguments | What it does |
| --- | --- | --- |
| `start_project` | `project` | Adds the nginx server block and starts the PHP worker. Requires an installed PHP version. |
| `stop_project` | `project` | Stops both of the project's services. Other projects keep running. |
| `restart_project` | `project` | Restarts the PHP worker; nginx keeps its server block. |
| `set_service` | `project`, `service` (`nginx`/`php`), `action` (`start`/`stop`/`restart`) | Controls one half of a project. |
| `all_services` | `action` | Starts, stops, or restarts every site and nginx. |
| `install_runtime` | `kind` (`php`/`nginx`), `version` | Downloads and installs a version. A new nginx becomes active. Waits for completion and returns the updated list. |
| `uninstall_runtime` | `kind`, `version` | Removes an installed version. Projects still pointing at it are left alone. |
| `set_active_nginx` | `version` | Chooses the nginx version for all sites; restarts nginx if it is running. |
| `set_project_php` | `project`, `version` | Points a project at an installed PHP version; an empty string clears it. Restarts the worker if it is running. |
| `set_project_https` | `project`, `enabled` | Issues a certificate and adds the TLS server block, or turns HTTPS off (the certificate file stays on disk). |
| `set_php_ini_overrides` | `project`, `overrides` | Replaces the project's raw php.ini directives (one per line). Restarts the worker. |
| `reset_php_ini` | `project` | Clears the overrides. Restarts the worker. |
| `repair_hosts` | — | Rewrites Wisp's hosts block. May show a UAC prompt; the tool waits for it. |
| `trust_ca` | — | Trusts Wisp's local CA for the current user. May show a Windows prompt; the tool waits. |

Action tools wait until the app has settled and return the resulting state, so
an agent does not have to poll. `install_runtime` waits up to 15 minutes; if the
catalog is still loading, the bridge retries for up to 45 seconds before asking
you to retry.

### Projects

| Tool | Arguments | What it does |
| --- | --- | --- |
| `create_project` | `name`, `root_path`, optional `app_name`, `domain`, `php_version`, `ssl_enabled`, `php_ini_overrides` | Creates the record. The domain defaults to the slug plus `.test`; HTTPS defaults to on. It does not start the project or create files. |
| `update_project` | `project`, optional `name`, `app_name`, `root_path`, `domain`, `php_version`, `ssl_enabled`, `php_ini_overrides` | Applies only the supplied fields, then restarts/reloads what needs it. Returns the updated project. |
| `delete_project` | `project` | Removes the record and stops its services. Files on disk are never touched. |

## Resources

The bridge exposes four resource shapes:

- `wisp://status` — the same JSON as `wisp_status`.
- `wisp://projects` — every project as JSON.
- `wisp://projects/{project}` — one project, using the same id/name/slug/domain
  lookup as the tools.
- `wisp://projects/{project}/logs/{kind}` — the newest 200 lines of a log as
  plain text.

## Prompt: `diagnose-site`

The prompt takes a `project` argument. It snapshots the project detail and the
recent nginx and PHP error logs, then gives the model a checklist: status and
services, `check_site`, hosts/certificate/nginx config, the three error logs,
and resource samples. It asks for a ranked cause with evidence and for the model
to say what it changed.

## Examples

- *"Why is myapp.test returning 502?"* — `check_site`, then `read_log` with
  `kind: error`, `php`, and `worker`; `get_project` confirms the worker is up.
- *"Install PHP 8.3.35 and switch the shop project to it."* —
  `list_runtimes`, `install_runtime`, `set_project_php`.
- *"Create a project for `C:\Sites\api` on `api.test` and start it."* —
  `create_project`, then `start_project`.
- *"The certificate expired."* — `get_certificates`, then `set_project_https`
  off and on, or restart the project to reissue.
- *"Is Wisp running anything?"* — `wisp_status` and `get_resources`.

## Troubleshooting

- **"MCP is turned off in Wisp."** — enable Settings → MCP → Local MCP server.
  The bridge deliberately does not launch the app when the feature is off.
- **"Wisp did not start its MCP endpoint."** — start Wisp manually and check the
  Settings → MCP status line. After an update, restart the app once so the new
  bridge and app speak the same protocol.
- **"The MCP token does not match this Wisp process."** — the app restarted. The
  next bridge call should pick up the new `mcp.json`; if a client caches a
  long-lived connection, restart the client.
- **`check_site` says "No such host is known."** — the domain is not in the
  hosts file. Run `repair_hosts` (UAC) or start the project in the app.
- **`check_site` reports a certificate error.** — run `trust_ca` for the
  browser/OS, or confirm the project's certificate exists with
  `get_certificates`. The tool itself verifies against Wisp's CA.
- **`install_runtime` says the catalog is loading.** — Wisp fetched the PHP and
  nginx indexes at launch; wait a few seconds and retry.
- **No endpoint from another machine.** — by design. It binds `127.0.0.1` only.

## Development notes

- `src/mcp/protocol.rs` — shared wire types and the tool argument schemas.
- `src/mcp/endpoint.rs` — the app's loopback listener and `mcp.json`.
- `src/mcp/stdio.rs` — the `wisp --mcp` bridge, its tools, resources, and prompt.
- `src/mcp/snippets.rs` — the per-client configuration shown in Settings.
- `src/app.rs`, under `// ---- MCP` — the handler that runs on the UI thread.
- `src/main.rs` — dispatches `--mcp` before the single-instance guard, because
  the bridge must run alongside the app.

The endpoint protocol and the bridge are versioned together by
`PROTOCOL_VERSION`. Bump it when a change is incompatible; an old app and a new
bridge then refuse to talk instead of misinterpreting each other.

Tests cover the wire types, the client snippets (including JSON validity), the
endpoint round trip, and that every tool is registered. Live MCP calls require a
running app; `cargo test` stays headless.
