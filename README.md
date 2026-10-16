# Wisp

Local PHP development for Windows — a Herd/FlyEnv-style tool that serves your
projects on `.test` domains over trusted HTTPS, with one nginx and one PHP
FastCGI worker per project.

Built in Rust with [GPUI Kit](https://gpui-kit.com).

See [NEXT-PHASE.md](NEXT-PHASE.md) for what is still open.

> **Status:** the shell, the icon pipeline, settings/tray/autostart/update, SQLite
> project storage with the projects UI, the Services page (PHP/nginx runtimes with
> install, update and uninstall), serving per project (generated nginx vhosts plus
> a per-project php-cgi worker with its own php.ini), local HTTPS with a
> per-domain certificate authority, `.test` hosts entries via an elevated helper,
> and a per-project log viewer are in place. The interface is built on GPUI Kit's
> own components: a collapsible `Sidebar` with project search, tabs for each
> project (Overview · Services · Logs · Settings), a log viewer with search, match
> navigation and follow, a toolbar of per-service controls, live CPU/RAM
> sparklines, a Ctrl-K command palette, and a tray menu with a submenu per site.

## What it will do

- **Projects** — add a project with a name, a root path and a domain; its `.test`
  domain is managed for you. Edit and delete projects freely; **deleting a
  project only removes it from Wisp, never the files on disk.**
- **PHP and nginx, managed** — a Services page installs, updates and removes PHP
  and nginx builds, so you can pick a PHP version per project and keep nginx on
  one active version.
- **HTTPS by default** — a local certificate authority issues a certificate per
  domain and is trusted once, so `https://myapp.test` just works.
- **Per-project `php.ini`** — each project gets its own PHP configuration, with
  per-project overrides applied last.
- **Per-service control** — start, stop and restart a project's nginx entry and
  its PHP worker independently, from the window or the tray.
- **Logs** — nginx access/error and PHP/worker logs per project, with a search
  field, match navigation, follow and a scroll region of their own.
- **Ctrl-K** — a command palette for jumping to a destination, opening a project,
  or starting, stopping and restarting everything.
- **Windows-native** — starts with Windows, lives in the tray with a submenu per
  site, and updates itself from GitHub Releases.

## Running it

```
cargo run
```

The first build compiles a lot of UI dependencies and takes several minutes.
Later builds are fast.

### Requirements

- Rust (MSVC toolchain). Built on 1.99.
- Visual Studio Build Tools with the **Desktop development with C++** workload.

## Project layout

```
src/
  main.rs        # entry point; single instance; window options
  app.rs         # the GPUI window: sidebar, project tabs, status bar, tray bridge
  ui/            # theme tokens and the shared section/dot primitives
  assets.rs      # serves the embedded brand icon over GPUI Kit's defaults
  autostart.rs   # the "start with Windows" Run entry
  tray.rs        # the tray icon and its per-site menu (background mode)
  platform.rs    # the Win32 calls with no safe wrapper: mutex, show/hide window
  config.rs      # where Wisp keeps its data (%USERPROFILE%\.config\wisp)
  db/            # SQLite storage for projects and installed runtimes
  runtime/       # the PHP/nginx catalog and download/install machinery
  serve/         # the supervisor: nginx config, per-project php.ini, workers
  elevation.rs   # the elevated-control helper and its scheduled task
  logs.rs        # tailing a log file
  settings.rs    # theme, background mode, update cache
  update.rs      # self-update from GitHub Releases
build.rs         # rasterises assets/icon.svg into the .ico + tray pixels
packaging/       # Inno Setup installer script
scripts/         # release notes, icon export
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

In the app, **Menu → Check for updates** asks GitHub Releases whether a newer
version exists (verified against `SHA256SUMS` before installing). Wisp also
checks automatically a few seconds after launch, at most once a day, silently.
