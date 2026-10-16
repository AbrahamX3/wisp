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
mod platform;
mod runtime;
mod serve;
mod settings;
mod shell;
#[cfg(test)]
mod test_support;
mod tray;
mod tls;
mod ui;
mod update;
mod vcrt;

use gpui_kit::*;
use std::io::Write as _;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Privileged helpers share this executable.
    match args.get(1).map(String::as_str) {
        Some("--ctl") => std::process::exit(ctl::run(&args)),
        Some("--ctl-from-task") => std::process::exit(ctl::run_from_task()),
        _ => {}
    }

    // Starting with Windows makes a second launch likely (autostart plus a
    // manual open), so surface the window that already exists instead of
    // adding a duplicate. Held for the rest of `main`.
    let Some(_instance) = platform::SingleInstance::acquire("Wisp.single-instance") else {
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

            // Stop nginx and the PHP workers on the way out, so nothing is left
            // holding port 80 or a FastCGI port.
            let _ = cx.on_app_quit(|cx| {
                cx.global_mut::<app::ServerGlobal>().0.stop_all();
                async {}
            });

            // Register the bundled typeface before the first window, so the
            // first frame is already laid out in it.
            fonts::install(cx);

            // Tray actions are ordinary GPUI actions, dispatched to the app.
            let _ = cx.on_action(|_: &tray::OpenWindow, _cx: &mut App| {
                platform::show_window(platform::WINDOW_TITLE, true);
            });
            let _ = cx.on_action(|_: &tray::QuitApp, cx: &mut App| {
                tray::remove(cx);
                cx.quit();
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
