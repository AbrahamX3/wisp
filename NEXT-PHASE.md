# Wisp — next phase

A working list of everything still open, kept in the repo so nothing depends on
remembering it. Grouped by area, roughly in the order I would tackle it.

## Resource monitor

**Done:** the sampler now measures what Wisp started — one tree for the shared
nginx (master plus forked workers) and one process per running project's
`php-cgi` — published from the supervisor as pids and resolved each tick, because
pids change on every worker restart. The machine totals remain underneath as
context, and the Overview tab shows only its own project.

Still open:

- nginx's cost is shown as one figure; a per-project share is not attributable,
  since one server serves every site. Consider labelling it "shared" more
  loudly, or showing requests/site instead of CPU.
- No throughput view. `stub_status` on nginx would give requests/site, which is a
  more honest per-site signal than CPU.

## Logs

**Done:** padding and a bordered pane that fills the height, a working
previous-match button, highlighted search hits, a Wrap toggle, filters in each
source's own terms (4xx/5xx for access lines, Fatal/Warning/Notice for errors),
Copy to the clipboard, a faint stripe on alternating rows, and the Follow, Wrap
and case choice persisted in `settings.json`.

Still open:

- **Row stripe is barely visible** — `list_hover` turned out to be very subtle as
  a row background. Needs a token with more presence, or a separator between
  rows instead.

## php.ini editor

Overrides are a plain textarea in the project form. Wanted: a proper editor sheet.

- **Generated / Custom modes** — show the generated file, and let the project's
  overrides sit on top of it.
- **Save and restart PHP** in one action.
- **Reset** back to the generated file.
- **Diff** — `gpui-kit` ships a `diff` component; use it to show generated vs
  effective.

## Runtimes

**Done:** installs report real download progress from the server's
`content-length` as a `Progress` bar with a percentage, the version list shows
`Skeleton` placeholders while the catalog loads, the Hosts tab shows the managed
`# wisp:start … # wisp:end` block as it is on disk, and the HTTPS tab lists each
project's domain with how long ago its certificate was issued.

Still open:

- **`DataTable`** for the version lists — deliberately not done. `DataTable`
  needs a full `TableDelegate` implementation per table, and the rows gained the
  information that mattered (progress, install state) without it. Revisit if the
  lists grow columns.
- Nothing reads the certificates themselves (issuer, expiry); only file ages are
  shown, because parsing `not_after` needs a date library that is not a
  dependency yet.

## Shell and chrome

**Done:** the sidebar footer lists Runtimes and Settings as proper full-width
rows, the status bar dropped the duplicated hosts control and groups the
all-sites buttons behind a separator, the project Services tab has its Start
both / Restart / Stop both back alongside the per-service controls, the Overview
rows put their open buttons beside the value instead of at the far edge, the
sidebar and content sit in an `h_resizable` pair, and the sidebar collapses to
icons from a toggle in the top bar, remembered in `settings.json`.

Still open:

- **Native menu bar** — the `native_menu` component, so Wisp has a real Windows
  menu (File / Project / Services / Help).
- **First-run wizard** — `Stepper` over: set up managed hosts access, install a
  PHP version, create the first project.
- **Tab strip styling** — the segmented `TabBar` draws a full-width container and
  the last tab reads as its own box. Confirmed by capture as the component's own
  styling; decide whether to override it.

## Notifications

`gpui-kit` has no `SystemNotification`, so an OS notification needs a Win32
balloon (`Shell_NotifyIcon` with `NIF_INFO`) or a toast, neither of which is
plumbed. Today failures are in-app banners and toasts only, which are invisible in
background mode. Decide whether to build the Win32 balloon through the tray icon,
or to keep failures in the window and make the tray icon show that something is
wrong.

## Tray

**Done:** each site's submenu has Start, Stop, Restart, Open and Open config
folder, and the whole menu is rebuilt from a published snapshot.

Still open:

- Reflect aggregate state in the icon itself (idle vs serving).

## Traps worth remembering

- **PowerShell splices over a function range can eat the next one.** Twice now,
  replacing a method by line range removed methods that sat between the start
  and the assumed end (`hosting_card`, then `toggle_palette` and
  `palette_overlay`). After any range edit, grep for the neighbouring method
  names before building.
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
