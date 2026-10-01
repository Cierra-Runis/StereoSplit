//! How this process fits into the Windows session: only one copy runs at a time, the
//! default playback device is switched back when the user logs off or shuts down, and the
//! program exits cleanly on Ctrl+C or when the console is closed (debug builds have one).
//! `taskkill` without `/F` doesn't reach it: it sends nothing to hidden windows.

use crate::default_device;
use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, Ordering};
use tracing::{error, info};
use windows::core::{w, BOOL, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::System::Console::{
    SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, PostMessageW, PostQuitMessage, RegisterClassW,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_ENDSESSION, WM_QUERYENDSESSION, WNDCLASSW,
};

/// The session window, for the console handler (which runs on its own thread) to close
static WINDOW: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// True if another copy of the program is already running
pub fn already_running() -> bool {
    unsafe {
        // The handle is deliberately kept open for the lifetime of the process
        CreateMutexW(None, false, w!("Local\\StereoSplit")).is_ok()
            && GetLastError() == ERROR_ALREADY_EXISTS
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_QUERYENDSESSION => LRESULT(1),
        WM_ENDSESSION => {
            if wparam.0 != 0 {
                info!("windows is logging off or shutting down");
                default_device::restore_from_disk();
            }
            LRESULT(0)
        }
        // Left to DefWindowProcW, the window would be destroyed while the program runs on.
        // The window lives on the UI thread, so this ends the message loop in main.
        WM_CLOSE => {
            info!("asked to close");
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe extern "system" fn on_console(ctrl: u32) -> BOOL {
    let window = WINDOW.load(Ordering::Relaxed);
    if window.is_null() || ![CTRL_C_EVENT, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT].contains(&ctrl) {
        return false.into();
    }
    let _ = PostMessageW(Some(HWND(window)), WM_CLOSE, WPARAM(0), LPARAM(0));
    // Returning from a close event ends the process, so wait here instead; the process
    // exits once main has cleaned up
    std::thread::park();
    true.into()
}

/// Hidden top-level window that hears about logoff/shutdown (message-only windows don't
/// receive those broadcasts) and `WM_CLOSE`, plus a console handler that closes it
pub fn create_window() {
    unsafe {
        let Ok(module) = GetModuleHandleW(None) else {
            return;
        };
        let class = w!("StereoSplitSession");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: module.into(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);
        let created = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            PCWSTR::null(),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(module.into()),
            None,
        );
        match created {
            Ok(hwnd) => {
                WINDOW.store(hwnd.0, Ordering::Relaxed);
                let _ = SetConsoleCtrlHandler(Some(on_console), true);
            }
            Err(e) => error!(error = %e, "failed to create the session window"),
        }
    }
}
