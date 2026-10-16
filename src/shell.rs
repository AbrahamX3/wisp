//! Opening folders and URLs with the operating system's default handlers.

use std::process::Command;

/// Windows `CREATE_NO_WINDOW`: no console flash while launching a handler.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Reveal a folder in File Explorer.
pub fn open_folder(path: &str) -> std::io::Result<()> {
    spawn("explorer.exe", &[path])
}

/// Open a URL in the default browser.
pub fn open_url(url: &str) -> std::io::Result<()> {
    // `start` is a cmd built-in; the empty argument is the window title slot.
    spawn("cmd.exe", &["/C", "start", "", url])
}

fn spawn(program: &str, args: &[&str]) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt as _;
    Command::new(program)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
}
