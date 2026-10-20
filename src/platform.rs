//! The handful of places this crate calls Win32 directly.
//!
//! Everything else goes through a safe wrapper. What lives here has no safe
//! equivalent: holding a single-instance mutex, and showing, hiding or focusing
//! our own window.
//!
//! GPUI's own `App::activate()` and `App::hide()` are empty stubs on Windows,
//! and it exposes no `Window::hide`/`show` at all, so window visibility cannot
//! come from the framework either. The window is addressed by its title, which
//! is unique to this app and is also how a second instance finds the first.

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowW, SetForegroundWindow, ShowWindowAsync, SW_HIDE, SW_RESTORE, SW_SHOWNORMAL,
};

/// Window title. Also how the tray and a second instance find the window.
pub const WINDOW_TITLE: &str = "Wisp";

/// Guard that keeps a second copy of the app from starting.
///
/// Dropping it releases the name, which only happens when the process exits.
pub struct SingleInstance {
    handle: HANDLE,
}

impl SingleInstance {
    /// `Some` when this is the only instance, `None` when one already exists.
    ///
    /// A failure to create the mutex at all also returns `None`; refusing to
    /// start is the safer side of that trade.
    #[expect(
        unsafe_code,
        reason = "CreateMutexW holds the single-instance name and has no safe wrapper"
    )]
    pub fn acquire(name: &str) -> Option<Self> {
        let name = wide(name);

        // Safety: `name` is NUL-terminated and outlives the call, and a null
        // security descriptor asks for the default access.
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() {
            return None;
        }

        // Safety: no arguments, and the error is only read to distinguish
        // "another instance owns this name" from "we created it".
        let already_running = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        if already_running {
            // Safety: `handle` came from CreateMutexW and is not used again.
            unsafe { CloseHandle(handle) };
            return None;
        }

        Some(Self { handle })
    }
}

impl Drop for SingleInstance {
    #[expect(unsafe_code, reason = "releasing the single-instance mutex")]
    fn drop(&mut self) {
        // Safety: `handle` came from CreateMutexW and is closed exactly once.
        unsafe { CloseHandle(self.handle) };
    }
}

/// Bring a window with this title to the front. False when there is none.
pub fn focus_window(title: &str) -> bool {
    show_window(title, true)
}

/// Show or hide a window with this title. False when there is none.
///
/// Showing also focuses, which is what the tray's "Open Wisp" wants. Hiding
/// takes the window out of the taskbar and Alt-Tab entirely, which is what
/// "minimize to tray" means.
#[expect(
    unsafe_code,
    reason = "FindWindowW/ShowWindowAsync/SetForegroundWindow have no safe wrapper"
)]
pub fn show_window(title: &str, visible: bool) -> bool {
    let title = wide(title);

    // Safety: `title` is NUL-terminated and outlives the calls.
    let hwnd = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
    if hwnd.is_null() {
        return false;
    }

    // Safety: `hwnd` came from FindWindowW and stays valid for these calls.
    unsafe {
        if visible {
            // Restore first, so a minimised window is visible before focusing.
            ShowWindowAsync(hwnd, SW_RESTORE);
            SetForegroundWindow(hwnd);
        } else {
            ShowWindowAsync(hwnd, SW_HIDE);
        }
    }
    true
}

/// Start `exe` as a detached GUI process.
///
/// `std::process::Command` creates the child with handle inheritance enabled,
/// so a GUI child inherits the caller's stdout/stderr pipes even when its own
/// standard handles point at `NUL`. The MCP bridge launches the app this way,
/// and an inherited pipe would stay open until the app exits, which can hang a
/// client that waits for the bridge's stderr to close.
///
/// `ShellExecuteW` asks the shell to start the process and inherits nothing.
#[expect(unsafe_code, reason = "ShellExecuteW has no safe wrapper")]
pub fn spawn_detached(exe: &std::path::Path) -> bool {
    let file = wide(&exe.to_string_lossy());

    // Safety: `file` is NUL-terminated and outlives the call. The other
    // arguments ask for the default verb, no parameters, the current
    // directory, and a normally-shown window.
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            std::ptr::null(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecute's documented success range is greater than 32.
    result as isize > 32
}

/// A NUL-terminated UTF-16 copy of `text`, for the `*W` Win32 entry points.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The `System32` directory, from `%SystemRoot%` with the usual fallback.
///
/// One definition, because every Windows binary lives there: `schtasks.exe`,
/// `certutil.exe`, `taskkill.exe` and PowerShell are all looked up through it.
pub fn system32() -> std::path::PathBuf {
    std::env::var_os("SystemRoot")
        .map_or_else(
            || std::path::PathBuf::from(r"C:\Windows"),
            std::path::PathBuf::from,
        )
        .join("System32")
}
