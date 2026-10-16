# Wisp — next phase

A working list of everything still open, kept in the repo so nothing depends on
remembering it. Grouped by area, roughly in the order I would tackle it.

## Resource monitor should watch *our* services, not the machine

Right now the Resources card samples global CPU and RAM. It answers "is my PC
busy", not "is this site slow". It should target the processes Wisp actually
starts:

- **nginx** — one shared process tree, so one row.
- **PHP workers** — one `php-cgi` per project, so a row per running site.

Wanted shape:

- Per-service CPU and memory, with the same 2s sampling and five-minute history
  behind a sparkline, but sampled per process.
- A project-scoped view (on the project's Overview, is *this* project heavy?) and
  a global view on Runtimes › Resources (which site is heavy?).
- Identify each process by PID from the supervisor, so the mapping cannot drift
  when a worker is restarted.

Notes: `sysinfo` can refresh a specific PID set (`Process::new` / `refresh_processes`
with a filter), but CPU needs two samples per process, and PIDs change on every
worker restart, so the sampler has to re-resolve PIDs each tick rather than cache
them. The supervisor already knows which worker belongs to which project.

## Logs

The pane is now padded and fills the height, but the viewer is still thin:

- **Level and status filters** — errors/PHP by level, access by status code. This
  was planned earlier and never built.
- **Highlight the matched substring** in each line rather than only filtering.
- **Wrap toggle** — currently lines are nowrap only; the planned toggle never
  shipped.
- **Copy** — selecting text out of the log is not possible today.
- **Row treatment** — consider alternating row tint and a subtle separator so
  400 lines of stack trace is readable.
- **Persist** the source, follow, wrap and case choices across restarts.

## php.ini editor

Overrides are a plain textarea in the project form. Wanted: a proper editor sheet.

- **Generated / Custom modes** — show the generated file, and let the project's
  overrides sit on top of it.
- **Save and restart PHP** in one action.
- **Reset** back to the generated file.
- **Diff** — `gpui-kit` ships a `diff` component; use it to show generated vs
  effective.

## Runtimes

- **`Progress`** for downloads — currently a spinner plus a label.
- **`Skeleton`** while the catalog loads, instead of an empty page.
- **`DataTable`** for the version lists (PHP lines, nginx builds) with columns for
  version, size and install state.
- **HTTPS tab** — list the issued certificates with subject, issuer and expiry,
  instead of only the CA controls.
- **Hosts tab** — show the managed `# wisp:start … # wisp:end` block and which
  domains currently map where.

## Shell and chrome

- **Resizable master–detail** — `h_resizable` so the sidebar width can be dragged,
  with the width persisted.
- **Collapsible sidebar** — `Sidebar` supports it; expose a toggle and remember
  the state.
- **Native menu bar** — the `native_menu` component, so Wisp has a real Windows
  menu (File / Project / Services / Help).
- **First-run wizard** — `Stepper` over: set up managed hosts access, install a
  PHP version, create the first project.
- **Tab bar styling** — in the current build the tab strip draws a box that runs
  to the right edge and the last tab sits in its own outline. Verify and fix.

## Notifications

`SystemNotification` for things worth seeing when the window is hidden:

- a site failed to start (nginx or the worker exited),
- the shared nginx stopped unexpectedly,
- an update is available.

Today these are in-app banners and toasts only, which are invisible in background
mode.

## Tray

- Add **Open logs folder** per site.
- Reflect aggregate state in the icon itself (idle vs serving).

## Traps worth remembering

- **Force-killing Wisp leaks nginx.** `Stop-Process -Force` skips the app-quit
  hook that stops nginx and the workers, so `nginx.exe` keeps ports 80 and 443 and
  the next start fails with a confusing sync error. The supervisor sweeps orphans
  during `sync`, but Wisp should also reap a *pre-existing* nginx on launch.
- **The main thread needs its stack.** `build.rs` links `/STACK:8388608`. MSVC's
  1 MiB default is not enough for a GPUI screen built from deeply nested generic
  builders in a debug build — removing that flag reintroduces
  `STATUS_STACK_OVERFLOW` at startup.
- **Only build the visible tab.** `projects_content` deliberately constructs one
  tab body per frame. Building all four was what tipped the stack over, and it is
  wasted work regardless.

## Verification

- `cargo test` (54) plus `cargo test syncs_the_configured_projects -- --ignored
  --nocapture` with `WISP_CONFIG_DIR` set, which must print 200 for `sinco.test`
  and `tester.local`.
- No end-to-end test drives the window itself yet; a smoke test that starts,
  serves and stops a project through the UI would catch regressions the
  supervisor test cannot.
