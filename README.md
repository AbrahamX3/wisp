# Wisp

Local PHP development for Windows — a Herd/FlyEnv-style tool that serves your
projects on `.test` domains over trusted HTTPS, with one nginx and one PHP
FastCGI worker per project.

Built in Rust with [GPUI Kit](https://gpui-kit.com).

> **Status:** early. The application shell, the icon pipeline, and the
> settings/tray/autostart/update foundations are in place. The project list, the
> nginx/PHP runtime manager, TLS and the log viewer are being built next.

## What it will do

- **Projects** — add a project with a name, a root path and a domain; its `.test`
  domain is managed for you. Edit and delete projects freely; **deleting a
  project only removes it from Wisp, never the files on disk.**
- **PHP and nginx, managed** — a Services page installs, updates and removes PHP
  and nginx builds, so you can pick a PHP version per project and keep nginx on
  one active version.
- **HTTPS by default** — a local certificate authority issues a certificate per
  domain and is trusted once, so `https://myapp.test` just works.
- **Per-project `php.ini`** — each project gets its own PHP configuration.
- **Logs** — nginx access/error and PHP error logs per project, viewable in-app.
- **Windows-native** — starts with Windows, runs in the tray, and updates itself
  from GitHub Releases.

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
  app.rs         # the GPUI window: header, settings menu, update banner, About
  assets.rs      # serves the embedded brand icon over GPUI Kit's defaults
  autostart.rs   # the "start with Windows" Run entry
  tray.rs        # the tray icon and its menu (background mode)
  platform.rs    # the Win32 calls with no safe wrapper: mutex, show/hide window
  config.rs      # where Wisp keeps its data (%USERPROFILE%\.config\wisp)
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
