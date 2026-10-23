//! Spawning and stopping the helper processes (nginx, php-cgi) without ever
//! flashing a console window.

use std::path::Path;
use std::process::{Child, Command, Stdio};

use crate::platform::system32;

/// Windows `CREATE_NO_WINDOW`.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Start a process with no window, optionally in a working directory and with
/// extra environment variables.
pub fn spawn(
    exe: &Path,
    args: &[String],
    env: &[(&str, String)],
    cwd: Option<&Path>,
) -> std::io::Result<Child> {
    build(exe, args, env, cwd).spawn()
}

/// Like [`spawn`], but captures the child's output into `log` so startup
/// failures are not lost. Used for the PHP workers.
pub fn spawn_logged(
    exe: &Path,
    args: &[String],
    env: &[(&str, String)],
    cwd: Option<&Path>,
    log: &Path,
) -> std::io::Result<Child> {
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(log)?;
    let sink = file.try_clone()?;
    build(exe, args, env, cwd)
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(sink))
        .spawn()
}

fn build(exe: &Path, args: &[String], env: &[(&str, String)], cwd: Option<&Path>) -> Command {
    use std::os::windows::process::CommandExt as _;

    let mut command = Command::new(exe);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW);
    for (key, value) in env {
        command.env(key, value);
    }
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    command
}

/// Whether a child is still alive, reaping it if it has exited.
pub fn is_running(child: &mut Child) -> bool {
    matches!(child.try_wait(), Ok(None))
}

/// Terminate a process and any children it started.
///
/// nginx's Windows master spawns worker processes; killing only the master can
/// leave workers holding the listening sockets, so the whole tree goes.
pub fn kill_tree(child: &mut Child) -> std::io::Result<()> {
    let pid = child.id();
    let status = Command::new(system32().join("taskkill.exe"))
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status();

    // Even if taskkill failed (already gone), make sure the handle is reaped.
    let _ = child.wait();
    match status {
        Ok(_) => Ok(()),
        Err(error) => Err(error),
    }
}

// `creation_flags` is provided by the same extension trait used above.
use std::os::windows::process::CommandExt;
