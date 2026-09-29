//! How this process fits into the Windows session: only one copy runs at a time, and the
//! default playback device is switched back when the user logs off or shuts down.

use crate::default_device;
use tracing::{error, info};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, RegisterClassW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_ENDSESSION,
    WM_QUERYENDSESSION, WNDCLASSW,
};

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
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Hidden top-level window whose only job is to hear about logoff/shutdown
/// (message-only windows don't receive those broadcasts)
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
            module,
            None,
        );
        if let Err(e) = created {
            error!(error = %e, "failed to create the session window");
        }
    }
}
