// Wisp - local PHP development for Windows.
//
// Starts nginx and a PHP FastCGI worker per project, serves each project on a
// `.test` domain, and issues a locally-trusted certificate for HTTPS.

// Build as a GUI app rather than a console app, so Windows never allocates a
// console window.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod assets;
mod autostart;
mod config;
mod ctl;
mod db;
mod elevation;
mod fonts;
mod helper_task;
mod hosts;
mod logs;
mod mcp;
mod platform;
mod runtime;
mod serve;
mod settings;
mod shell;
#[cfg(test)]
mod test_support;
mod tls;
mod tray;
mod ui;
mod update;
mod vcrt;

use gpui_kit::*;
use std::io::Write as _;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Privileged helpers are launched by Windows (UAC, the scheduled task) with
    // a fresh environment, so the config directory is passed explicitly rather
    // than relying on `WISP_CONFIG_DIR` surviving elevation.
    if let Some(index) = args.iter().position(|arg| arg == "--config-dir") {
        if let Some(dir) = args.get(index + 1) {
            std::env::set_var("WISP_CONFIG_DIR", dir);
        }
    }

    // Privileged helpers share this executable.
    match args.get(1).map(String::as_str) {
        Some("--ctl") => std::process::exit(ctl::run(&args)),
        Some("--ctl-from-task") => std::process::exit(ctl::run_from_task()),
        // Remove Wisp's managed block (and its markers) from the hosts file.
        // Needs administrator rights; used by the testing reset script.
        Some("--clear-hosts") => std::process::exit(clear_hosts()),
        // The stdio MCP server a client launches. It must run *alongside* the
        // GUI app, so it is handled before the single-instance guard.
        Some("--mcp") => std::process::exit(mcp::stdio::run()),
        _ => {}
    }

    // Starting with Windows makes a second launch likely (autostart plus a
    // manual open), so surface the window that already exists instead of
    // adding a duplicate. Held for the rest of `main`.
    //
    // The name follows the config profile: a portable or test profile is a
    // separate install and may run alongside the normal one. Two profiles both
    // serving sites still contend for ports 80/443, which surfaces as a serve
    // error rather than silently sharing state.
    let instance_name = config::profile_id().map_or_else(
        || "Wisp.single-instance".to_string(),
        |id| format!("Wisp.single-instance-{id}"),
    );
    let Some(_instance) = platform::SingleInstance::acquire(&instance_name) else {
        platform::focus_window(platform::WINDOW_TITLE);
        return;
    };

    // Launched by the Run entry rather than by the user: come up quietly.
    let from_run_entry = args.iter().any(|arg| arg == "--startup");

    config::ensure();

    gpui_kit::application()
        .with_assets(assets::AppAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            cx.set_app_identity("com.abraham.wisp", "Wisp");
            cx.set_global(tray::TrayState::default());
            cx.set_global(app::ServerGlobal(serve::Supervisor::default()));
            cx.set_global(app::TrackedProcesses::default());

            // Stop nginx and the PHP workers on the way out, so nothing is left
            // holding port 80 or a FastCGI port.
            let _ = cx.on_app_quit(|cx| {
                cx.global_mut::<app::ServerGlobal>().0.stop_all();
                // The endpoint file must not outlive the process that owns the
                // port; a stale one sends the next bridge to a dead port.
                mcp::endpoint::remove_discovery();
                async {}
            });
            // An in-app restart (the updater's) replaces the process too.
            let _ = cx.on_app_restart(|cx| {
                cx.global_mut::<app::ServerGlobal>().0.stop_all();
                // The replacement process writes a new endpoint; this one's
                // port must not be advertised in the gap.
                mcp::endpoint::remove_discovery();
            });

            // Register the bundled typeface before the first window, so the
            // first frame is already laid out in it.
            fonts::install(cx);

            // Tray actions are ordinary GPUI actions, dispatched to the app.
            cx.bind_keys([KeyBinding::new("ctrl-k", app::TogglePalette, None)]);
            let _ = cx.on_action(|_: &tray::OpenWindow, _cx: &mut App| {
                platform::show_window(platform::WINDOW_TITLE, true);
            });
            let _ = cx.on_action(|_: &tray::QuitApp, cx: &mut App| {
                // Stop the servers before anything else: quitting the app must
                // not leave nginx holding ports 80 and 443.
                cx.global_mut::<app::ServerGlobal>().0.stop_all();
                tray::remove(cx);
                cx.quit();
            });
            let _ = cx.on_action(|action: &tray::SiteAction, cx: &mut App| {
                tray::dispatch(
                    tray::TrayRequest::Site {
                        id: action.id,
                        command: action.command,
                    },
                    cx,
                );
            });
            let _ = cx.on_action(|_: &tray::StartAll, cx: &mut App| {
                tray::dispatch(tray::TrayRequest::All(tray::AllCommand::Start), cx);
            });
            let _ = cx.on_action(|_: &tray::StopAll, cx: &mut App| {
                tray::dispatch(tray::TrayRequest::All(tray::AllCommand::Stop), cx);
            });
            let _ = cx.on_action(|_: &tray::RestartAll, cx: &mut App| {
                tray::dispatch(tray::TrayRequest::All(tray::AllCommand::Restart), cx);
            });

            let settings = settings::Settings::load();
            app::apply_theme(settings.theme, None, cx);

            if settings.background {
                if let Err(error) = tray::install(cx) {
                    let _ = writeln!(std::io::stderr(), "Wisp: no tray icon: {error}");
                }
            }

            if let Err(error) = gpui_kit::open_window(window_options(), cx, move |window, cx| {
                let settings = settings.clone();
                // Closing the window is how background mode is left running.
                window.on_window_should_close(cx, app::window_should_close);
                cx.new(move |cx| app::Wisp::new(settings, window, cx))
            }) {
                // Best-effort: a GUI-subsystem build may have no console at all,
                // and `eprintln!` would panic on an invalid handle.
                let _ = writeln!(std::io::stderr(), "Wisp: failed to open window: {error}");
            }

            // Start hidden in the tray when both the Run entry and background
            // mode are on; the tray icon is how it is brought back.
            if from_run_entry && tray::is_enabled(cx) {
                platform::show_window(platform::WINDOW_TITLE, false);
            }
        });
}

/// `--clear-hosts`: drop Wisp's managed hosts block, markers included.
fn clear_hosts() -> i32 {
    match hosts::write(&[]) {
        Ok(()) => {
            let _ = writeln!(std::io::stdout(), "wisp: hosts block cleared");
            0
        }
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "wisp: could not clear hosts: {error}");
            1
        }
    }
}

fn window_options() -> WindowOptions {
    let bounds = Bounds::new(point(px(220.), px(160.)), size(px(920.), px(640.)));
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        // Keep the layout usable: below this the panes reflow rather than clip.
        window_min_size: Some(size(px(720.), px(480.))),
        titlebar: Some(TitlebarOptions {
            title: Some(platform::WINDOW_TITLE.into()),
            ..Default::default()
        }),
        ..Default::default()
    }
}
