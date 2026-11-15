//! Small Win32 helpers. All `unsafe` code of the project lives here.
//!
//! * Main window show / hide / close, done with plain Win32 calls so it works
//!   from any thread (tray thread, Ctrl-C handler, GUI thread) and also while
//!   the window is hidden and egui is not being driven.
//! * Re-attaching to the parent console for `--cli` mode (release builds use
//!   the GUI subsystem, which starts without a console).
//! * A tiny message loop with a "wake" message, used by the tray thread.

use std::os::windows::io::IntoRawHandle;

use windows_sys::Win32::Foundation::{BOOL, FALSE, HWND, INVALID_HANDLE_VALUE, LPARAM, TRUE};
use windows_sys::Win32::System::Console::{
    AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE,
    STD_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, EnumWindows, GetMessageW, GetWindowTextW, GetWindowThreadProcessId,
    IsIconic, PostMessageW, PostThreadMessageW, SetForegroundWindow, SetTimer, ShowWindow,
    TranslateMessage, MSG, SW_HIDE, SW_RESTORE, SW_SHOW, WM_APP, WM_CLOSE, WM_TIMER,
};

use crate::state::WINDOW_TITLE;

/// Posted to the tray thread when the shared state changed.
pub const WM_WAKE: u32 = WM_APP + 1;

// ===========================================================================
// Main window
// ===========================================================================

struct Search {
    pid: u32,
    hwnd: HWND,
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let search = &mut *(lparam as *mut Search);

    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, &mut pid);
    if pid != search.pid {
        return TRUE;
    }

    let mut buf = [0u16; 128];
    let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
    if n <= 0 {
        return TRUE;
    }
    if String::from_utf16_lossy(&buf[..n as usize]) == WINDOW_TITLE {
        search.hwnd = hwnd;
        return FALSE; // stop enumerating
    }
    TRUE
}

/// Finds this process' main window by title. Hidden windows are found too.
fn find_main_window() -> Option<HWND> {
    let mut search = Search {
        pid: unsafe { GetCurrentProcessId() },
        hwnd: std::ptr::null_mut(),
    };
    unsafe {
        EnumWindows(Some(enum_proc), &mut search as *mut Search as LPARAM);
    }
    if search.hwnd.is_null() {
        None
    } else {
        Some(search.hwnd)
    }
}

/// Show (restore + focus) or hide the main window. No-op if the window does
/// not exist yet.
pub fn set_main_window_visible(visible: bool) {
    let Some(hwnd) = find_main_window() else {
        return;
    };
    unsafe {
        if visible {
            if IsIconic(hwnd) != 0 {
                ShowWindow(hwnd, SW_RESTORE);
            } else {
                ShowWindow(hwnd, SW_SHOW);
            }
            SetForegroundWindow(hwnd);
        } else {
            ShowWindow(hwnd, SW_HIDE);
        }
    }
}

/// Ask the main window to close (as if the user clicked the X). Posted, never
/// blocks.
pub fn close_main_window() {
    if let Some(hwnd) = find_main_window() {
        unsafe {
            PostMessageW(hwnd, WM_CLOSE, 0, 0);
        }
    }
}

// ===========================================================================
// Console (for --cli in a GUI-subsystem build)
// ===========================================================================

fn redirect_std_handle(which: STD_HANDLE) {
    unsafe {
        let h = GetStdHandle(which);
        // A valid handle means the output was redirected (`> log.txt`) or we
        // already have a console: leave it alone.
        if !h.is_null() && h != INVALID_HANDLE_VALUE {
            return;
        }
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open("CONOUT$") {
            // The handle is intentionally leaked: it must live as long as the process.
            SetStdHandle(which, f.into_raw_handle());
        }
    }
}

/// Attach to the console of the parent process (cmd.exe / PowerShell /
/// Windows Terminal) so `println!` shows up there. Returns false if there is
/// no parent console (or we already own one, e.g. in debug builds).
pub fn attach_parent_console() -> bool {
    let ok = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } != 0;
    if ok {
        redirect_std_handle(STD_OUTPUT_HANDLE);
        redirect_std_handle(STD_ERROR_HANDLE);
    }
    ok
}

// ===========================================================================
// Message loop for the tray thread
// ===========================================================================

pub fn current_thread_id() -> u32 {
    unsafe { GetCurrentThreadId() }
}

/// Wake the message loop of `thread_id` (see `run_message_loop`). Never blocks.
pub fn post_wake(thread_id: u32) {
    unsafe {
        PostThreadMessageW(thread_id, WM_WAKE, 0, 0);
    }
}

/// Run a Win32 message loop on the current thread until `on_wake` returns
/// false or `WM_QUIT` arrives. `on_wake` is called for every `WM_WAKE` and,
/// as a safety net (thread messages posted while a popup menu is open are
/// swallowed by the menu's modal loop), once per second from a timer.
///
/// Windows created on this thread (the tray icon's hidden window) get their
/// messages dispatched from here.
pub fn run_message_loop(mut on_wake: impl FnMut() -> bool) {
    unsafe {
        SetTimer(std::ptr::null_mut(), 0, 1000, None);

        let mut msg: MSG = std::mem::zeroed();
        loop {
            let r = GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0);
            if r <= 0 {
                break; // 0 = WM_QUIT, -1 = error
            }
            let thread_msg = msg.hwnd.is_null();
            if thread_msg && (msg.message == WM_WAKE || msg.message == WM_TIMER) {
                if !on_wake() {
                    break;
                }
                continue;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
