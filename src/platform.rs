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
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowW, SetForegroundWindow, ShowWindowAsync, SW_HIDE, SW_RESTORE,
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

/// A NUL-terminated UTF-16 copy of `text`, for the `*W` Win32 entry points.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}
