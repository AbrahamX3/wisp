# Wisp

Local PHP development for Windows — a Herd/FlyEnv-style tool that serves your
projects on `.test` domains over trusted HTTPS, with one nginx and one PHP
FastCGI worker per project.

Built in Rust with [GPUI Kit](https://gpui-kit.com).

## Install

Download **`Wisp-Setup-<version>.exe`** from the
[latest release](https://github.com/AbrahamX3/wisp/releases/latest) and run it.
It installs per user, so it never asks for elevation, and Wisp keeps itself up to
date from then on.

PHP and nginx are not bundled. Wisp installs them for you from the built-in
catalog, so a fresh install needs no manual setup: add a project, pick a PHP
version, and the app fetches the runtime it needs.

## What it does

- **Projects** — a name, a folder and a `.test` domain. Wisp writes the nginx
  server block, issues the certificate and adds the hosts entry. **Removing a
  project only removes it from Wisp; the files on disk are never touched.**
- **PHP and nginx, managed** — install, update and remove runtime builds from
  the Runtimes page. Pick a PHP version per project; nginx runs on one active
  version for all of them. Downloads report their progress.
- **Per-project `php.ini`** — generated from the runtime's own template, with
  your overrides applied last. An editor shows the generated file, your
  overrides, and a diff of what they change; saving restarts the worker.
- **Per-service control** — start, stop and restart a project's nginx entry and
  its PHP worker independently, or both together, from the window or the tray.
- **HTTPS by default** — a local certificate authority issues a certificate per
  domain and is trusted once, so `https://myapp.test` just works. The Runtimes
  page lists each certificate and when it expires.
- **Logs** — access, error, PHP and worker logs, with search that highlights its
  hits, match navigation, level or status filters, follow, wrap and copy.
- **Links, not buttons** — a domain or a folder is clickable wherever it appears.
- **Project icons** — each project gets a generated colour-and-letter icon, or
  you can pick an image.
- **Live resource use** — CPU and memory for the nginx and PHP processes Wisp
  started, sampled every two seconds, not just whole-machine figures.
- **Ctrl-K** - jump to a screen, open a project, or start, stop and restart
  everything.
- **Local MCP server** - off by default. Turn it on in Settings → MCP and a CLI
  agent (Claude Code, OpenCode, Codex, Cursor, VS Code) can read logs, check
  sites, control services and manage projects over a loopback-only endpoint.
  See [docs/mcp.md](docs/mcp.md).
- **Windows-native** - optional start-with-Windows, lives in the tray with a
  submenu per site, and updates itself from GitHub Releases.

## Where it keeps things

Everything lives under `%USERPROFILE%\.config\wisp`:

```
wisp.db        projects and installed runtimes (SQLite)
settings.json  theme, background mode, update cache, MCP toggle
mcp.json       MCP endpoint port and per-launch token (only while enabled)
runtimes/      installed PHP and nginx builds
nginx/         the running nginx's configuration
sites/<slug>/  each project's generated nginx config and php.ini
certs/         the local certificate authority and issued certificates
```

Wisp owns the block between `# wisp:start` and `# wisp:end` in your hosts file
and leaves every other line alone. Writing it needs administrator rights once;
Wisp sets up a scheduled task for that automatically so it never prompts again.

## Local MCP server

Wisp can expose the running app to a CLI agent over the Model Context Protocol.
It is off by default. Enable it in **Settings → MCP**, then copy the generated
configuration for Claude Code, OpenCode, Codex CLI, Cursor, VS Code, or any
client that accepts the common `mcpServers` JSON.

The GUI hosts a loopback-only endpoint and `wisp.exe --mcp` is the stdio bridge
the client launches. The bridge starts Wisp when it is not running and forwards
each call to the app, so an agent sees and changes exactly the state you see.
Tools cover projects, logs, certificates, hosts, runtimes, services and a
`check_site` probe, plus a `diagnose-site` prompt.
[`docs/mcp.md`](docs/mcp.md) has the full reference, the security model, and
troubleshooting.

## Requirements

- Windows 10 or 11, 64-bit.
- Nothing else. Wisp offers to install the Visual C++ runtime if PHP needs it.

## Building from source

```
cargo run
```

The first build compiles a lot of UI dependencies and takes several minutes.
Later builds are fast.

- Rust with the MSVC toolchain.
- Visual Studio Build Tools with the **Desktop development with C++** workload.

## Project layout

```
src/
  main.rs        # entry point; single instance; window options
  app.rs         # the GPUI window: sidebar, project tabs, status bar, tray bridge
  ui/            # theme tokens and the shared section/dot primitives
  assets.rs      # serves the brand icon and the icons beyond the kit's defaults
  autostart.rs   # the "start with Windows" Run entry
  tray.rs        # the tray icon and its per-site menu (background mode)
  platform.rs    # the Win32 calls with no safe wrapper: mutex, show/hide window
  config.rs      # where Wisp keeps its data (%USERPROFILE%\.config\wisp)
  db/            # SQLite storage for projects and installed runtimes
  runtime/       # the PHP/nginx catalog and download/install machinery
  serve/         # the supervisor: nginx config, per-project php.ini, workers
  elevation.rs   # the elevated-control helper and its scheduled task
  logs.rs        # tailing a log file
  settings.rs    # theme, background mode, update cache, MCP toggle
  mcp/           # local MCP: wire protocol, loopback endpoint, stdio bridge, snippets
  update.rs      # self-update from GitHub Releases
docs/            # user documentation, including the MCP reference
build.rs         # rasterises assets/icon.svg into the .ico + tray pixels
packaging/       # Inno Setup installer script
scripts/         # release notes, icon export, window capture
```

## Icon

`assets/icon.svg` is the app's icon artwork — a **W** monogram with a spark — and
it is the single source of truth. `build.rs` rasterises it at build time into the
multi-size `.ico` embedded in the executable and the raw 32×32 RGBA the tray
needs. Nothing generated is committed.

## Releases and updates

Releases are built by CI. Pushing a tag triggers
`.github/workflows/release.yml`, which builds the release binary on
`windows-latest` and publishes to the GitHub Release:

- `wisp-x86_64-pc-windows-msvc.zip` — contains `wisp.exe`
- `Wisp-Setup-<version>.exe` — a per-user installer (no UAC)
- `SHA256SUMS` — the archives' SHA-256

To cut a release, bump `version` in `Cargo.toml`, commit, then tag and push:

```
# edit Cargo.toml: version = "0.2.0"
cargo build --release --locked   # refreshes Cargo.lock
git commit -am "chore: release v0.2.0"
git tag v0.2.0
git push && git push --tags
```

Wisp checks GitHub Releases a few seconds after launch, at most once a day, and
silently. In the app, **Settings → Updates** checks on demand. A downloaded
update is verified against `SHA256SUMS` before it is applied.

## Known limitations

- Windows only, for now.
- File paths inside log lines are not yet clickable; everything else that names
  a domain or a folder is.
- Failures are reported in the window, not as OS notifications, so a failure
  while Wisp is hidden in the tray is easy to miss.

[NEXT-PHASE.md](NEXT-PHASE.md) tracks what is left.
