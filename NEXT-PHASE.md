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
previous-match button, highlighted search hits, and a Wrap toggle.

Still open:

- **Level and status filters** — errors/PHP by level, access by status code.
  This was planned earlier and still is not built.
- **Copy** — text cannot be selected out of the log yet.
- **Row treatment** — alternating row tint or a separator, so a dense log is
  easier to scan.
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

**Done:** the sidebar footer lists Runtimes and Settings as proper full-width
rows, the status bar dropped the duplicated hosts control and groups the
all-sites buttons behind a separator, the project Services tab has its Start
both / Restart / Stop both back alongside the per-service controls, and the
Overview rows put their open buttons next to the value instead of at the far
edge.

Still open:

- **Resizable master–detail** — `h_resizable` so the sidebar width can be dragged,
  with the width persisted.
- **Collapsible sidebar** — `Sidebar` supports it; expose a toggle and remember
  the state.
- **Native menu bar** — the `native_menu` component, so Wisp has a real Windows
  menu (File / Project / Services / Help).
- **First-run wizard** — `Stepper` over: set up managed hosts access, install a
  PHP version, create the first project.
- **Tab strip styling** — the segmented `TabBar` draws a full-width container and
  the last tab reads as its own box. Confirm whether that is the component's
  intent or something to override.

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
