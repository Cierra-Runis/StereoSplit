// Don't open a black console window in release builds
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod default_device;
mod engine;
mod volume;

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use config::Config;
use tray_icon::menu::{
    CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{Icon, TrayIconBuilder, TrayIconEvent};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Recovery::{RegisterApplicationRestart, RESTART_NO_REBOOT};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, MessageBoxW, PostQuitMessage,
    RegisterClassW, TranslateMessage, MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MSG,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_ENDSESSION, WM_QUERYENDSESSION, WNDCLASSW,
};

const APP_NAME: &str = "Stereo Split";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const LATENCIES: [u32; 4] = [15, 20, 30, 50];

/// Append a line to stereo-split.log next to the exe
pub fn log(msg: &str) {
    let path = config::exe_dir().join("stereo-split.log");
    // Start over once the log exceeds 1 MB
    if std::fs::metadata(&path)
        .map(|m| m.len() > 1_000_000)
        .unwrap_or(false)
    {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "[{t}] {msg}");
    }
}

fn message_box(text: &str, error: bool) {
    let flags = MB_OK
        | if error {
            MB_ICONERROR
        } else {
            MB_ICONINFORMATION
        };
    unsafe {
        MessageBoxW(None, &HSTRING::from(text), &HSTRING::from(APP_NAME), flags);
    }
}

fn autostart_enabled() -> bool {
    use winreg::enums::HKEY_CURRENT_USER;
    winreg::RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(RUN_KEY)
        .and_then(|k| k.get_value::<String, _>(APP_NAME))
        .is_ok()
}

fn set_autostart(on: bool) -> std::io::Result<()> {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
    let key =
        winreg::RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE)?;
    if on {
        let exe = std::env::current_exe()?;
        key.set_value(APP_NAME, &format!("\"{}\"", exe.display()))
    } else {
        key.delete_value(APP_NAME)
    }
}

/// Generated tray icon: left half blue, right half orange, for the left and right channels
fn make_icon() -> Icon {
    const N: u32 = 32;
    const SS: u32 = 4;
    const LEFT: [f32; 3] = [59.0, 130.0, 246.0];
    const RIGHT: [f32; 3] = [245.0, 158.0, 11.0];
    let (c, radius, half_gap) = (N as f32 / 2.0, 15.0f32, 1.0f32);
    let mut rgba = Vec::with_capacity((N * N * 4) as usize);
    for y in 0..N {
        for x in 0..N {
            let (mut left, mut right) = (0u32, 0u32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let px = x as f32 + (sx as f32 + 0.5) / SS as f32;
                    let py = y as f32 + (sy as f32 + 0.5) / SS as f32;
                    let (dx, dy) = (px - c, py - c);
                    if dx * dx + dy * dy > radius * radius {
                        continue;
                    }
                    if dx < -half_gap {
                        left += 1;
                    } else if dx > half_gap {
                        right += 1;
                    }
                }
            }
            let covered = left + right;
            if covered == 0 {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
                continue;
            }
            let (wl, wr) = (left as f32 / covered as f32, right as f32 / covered as f32);
            let mix = |i: usize| (LEFT[i] * wl + RIGHT[i] * wr).round() as u8;
            let a = (covered as f32 * 255.0 / (SS * SS) as f32).round() as u8;
            rgba.extend_from_slice(&[mix(0), mix(1), mix(2), a]);
        }
    }
    Icon::from_rgba(rgba, N, N).expect("icon")
}

/// Audio supervisor thread: starts the engine, retries automatically after errors (e.g. a
/// speaker being unplugged), and restarts with the new config whenever config.toml changes
/// (whether from the tray menu or edited by hand).
fn spawn_supervisor(quit: Arc<AtomicBool>, status: Arc<Mutex<String>>) {
    std::thread::spawn(move || {
        let set_status = |s: &str| *status.lock().unwrap() = s.into();
        let mut shown_error: Option<String> = None;
        let mut hinted = false;
        // Whether this process has made the source the default playback device
        let mut managed = false;

        while !quit.load(Ordering::Relaxed) {
            let stamp = config::modified();
            let cfg = match config::load() {
                Ok(c) => c,
                Err(e) => {
                    let msg = format!("{e:#}");
                    log(&msg);
                    set_status("Config error");
                    if shown_error.as_deref() != Some(msg.as_str()) {
                        message_box(&msg, true);
                        shown_error = Some(msg);
                    }
                    wait_for_change(&quit, stamp);
                    continue;
                }
            };

            // Hand the default device back when management was just switched off
            if managed && !cfg.manage_default_device {
                release(&cfg, &mut managed);
            }

            if cfg.left.is_empty() || cfg.right.is_empty() {
                release(&cfg, &mut managed);
                set_status("Choose speakers");
                if !hinted {
                    hinted = true;
                    message_box(
                        "Right-click the tray icon (the blue and orange dot) and choose your \
                         speakers under \"Left speaker\" and \"Right speaker\".",
                        false,
                    );
                }
                wait_for_change(&quit, stamp);
                continue;
            }

            let vol_stop = Arc::new(AtomicBool::new(false));
            let gain = volume::Gain::new(1.0);
            volume::spawn_watcher(cfg.volume_endpoint.clone(), gain.clone(), vol_stop.clone());

            let failed = Arc::new(AtomicBool::new(false));
            match engine::Engine::start(&cfg, gain, failed.clone()) {
                Ok(engine) => {
                    shown_error = None;
                    set_status("Running");
                    if cfg.manage_default_device {
                        match default_device::take_over(&cfg) {
                            Ok(()) => managed = true,
                            Err(e) => log(&format!("{e:#}")),
                        }
                    }
                    while !quit.load(Ordering::Relaxed)
                        && config::modified() == stamp
                        && !failed.load(Ordering::Relaxed)
                    {
                        std::thread::sleep(Duration::from_millis(200));
                    }
                    drop(engine);
                    if failed.load(Ordering::Relaxed) {
                        log("Audio interrupted, reconnecting in 2 seconds");
                        set_status("Reconnecting");
                        std::thread::sleep(Duration::from_secs(2));
                    }
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    log(&msg);
                    set_status("Failed to start (see log)");
                    // Nothing is playing through the source now, so let Windows play
                    // straight to a speaker until the engine is back
                    release(&cfg, &mut managed);
                    // Show each distinct error only once, then retry silently
                    // (e.g. a speaker that isn't plugged in yet)
                    if shown_error.as_deref() != Some(msg.as_str()) {
                        message_box(
                            &format!(
                                "{msg}\n\nThe program will retry automatically every 3 seconds."
                            ),
                            true,
                        );
                        shown_error = Some(msg);
                    }
                    for _ in 0..15 {
                        if quit.load(Ordering::Relaxed) || config::modified() != stamp {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
            }
            vol_stop.store(true, Ordering::Relaxed);
        }
    });
}

/// Switch the default playback device back if this process took it over
fn release(cfg: &Config, managed: &mut bool) {
    if std::mem::take(managed) {
        if let Err(e) = default_device::restore(cfg) {
            log(&format!("{e:#}"));
        }
    }
}

fn wait_for_change(quit: &AtomicBool, stamp: Option<SystemTime>) {
    while !quit.load(Ordering::Relaxed) && config::modified() == stamp {
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Load the config, change it and save it. On a broken config file nothing is changed.
fn edit_config(f: impl FnOnce(&mut Config)) -> Option<Config> {
    match config::load() {
        Ok(mut cfg) => {
            f(&mut cfg);
            if let Err(e) = config::save(&cfg) {
                message_box(&format!("{e:#}"), true);
            }
            Some(cfg)
        }
        Err(e) => {
            message_box(
                &format!("{e:#}\n\nFix config.toml (or delete it to start over) first."),
                true,
            );
            None
        }
    }
}

/// Put `name` on one side. If it was on the other side, the two sides swap.
fn choose(this: &mut String, other: &mut String, name: &str, devices: &[String]) {
    if engine::pick(devices, other) == engine::pick(devices, name) {
        *other = this.clone();
    }
    *this = name.to_string();
}

/// Menu text: '&' would otherwise mark a keyboard shortcut
fn menu_text(s: &str) -> String {
    s.replace('&', "&&")
}

struct Tray {
    menu: Menu,
    status: MenuItem,
    left: Submenu,
    right: Submenu,
    /// Speakers currently listed in the Left/Right submenus; item ids index into this
    devices: Vec<String>,
    left_items: Vec<CheckMenuItem>,
    right_items: Vec<CheckMenuItem>,
    swap: MenuItem,
    test_left: MenuItem,
    test_right: MenuItem,
    latency: Vec<(u32, CheckMenuItem)>,
    follow: CheckMenuItem,
    manage: CheckMenuItem,
    auto: CheckMenuItem,
    open_config: MenuItem,
    view_log: MenuItem,
    quit: MenuItem,
}

impl Tray {
    fn new() -> Tray {
        let latency_menu = Submenu::new("Latency", true);
        let latency: Vec<(u32, CheckMenuItem)> = LATENCIES
            .iter()
            .map(|&ms| {
                let item = CheckMenuItem::with_id(
                    format!("latency:{ms}"),
                    format!("{ms} ms"),
                    true,
                    false,
                    None,
                );
                let _ = latency_menu.append(&item);
                (ms, item)
            })
            .collect();

        let tray = Tray {
            menu: Menu::new(),
            status: MenuItem::new("Status: Starting", false, None),
            left: Submenu::new("Left speaker", true),
            right: Submenu::new("Right speaker", true),
            devices: Vec::new(),
            left_items: Vec::new(),
            right_items: Vec::new(),
            swap: MenuItem::new("Swap left / right", true, None),
            test_left: MenuItem::new("Test left", true, None),
            test_right: MenuItem::new("Test right", true, None),
            latency,
            follow: CheckMenuItem::new("Follow Windows volume keys", true, false, None),
            manage: CheckMenuItem::new("Manage default playback device", true, false, None),
            auto: CheckMenuItem::new("Start with Windows", true, autostart_enabled(), None),
            open_config: MenuItem::new("Open config file", true, None),
            view_log: MenuItem::new("View log", true, None),
            quit: MenuItem::new("Exit", true, None),
        };
        let _ = tray.menu.append_items(&[
            &tray.status,
            &PredefinedMenuItem::separator(),
            &tray.left,
            &tray.right,
            &tray.swap,
            &tray.test_left,
            &tray.test_right,
            &PredefinedMenuItem::separator(),
            &latency_menu,
            &tray.follow,
            &tray.manage,
            &tray.auto,
            &PredefinedMenuItem::separator(),
            &tray.open_config,
            &tray.view_log,
            &PredefinedMenuItem::separator(),
            &tray.quit,
        ]);
        tray
    }

    /// Re-read the config and the device list, and update the menu to match
    fn refresh(&mut self) {
        let Ok(cfg) = config::load() else {
            return;
        };
        let hide = |n: &str| {
            let n = n.to_lowercase();
            [&cfg.source, &cfg.volume_endpoint]
                .iter()
                .any(|p| !p.is_empty() && n.contains(&p.to_lowercase()))
        };
        let devices: Vec<String> = engine::output_device_names()
            .into_iter()
            .filter(|n| !hide(n))
            .collect();
        if devices != self.devices {
            self.rebuild_devices(devices);
        }
        self.sync(&cfg);
    }

    fn rebuild_devices(&mut self, devices: Vec<String>) {
        for (menu, items, side) in [
            (&self.left, &mut self.left_items, "left"),
            (&self.right, &mut self.right_items, "right"),
        ] {
            while menu.remove_at(0).is_some() {}
            items.clear();
            for (i, name) in devices.iter().enumerate() {
                let item = CheckMenuItem::with_id(
                    format!("{side}:{i}"),
                    menu_text(name),
                    true,
                    false,
                    None,
                );
                let _ = menu.append(&item);
                items.push(item);
            }
            if devices.is_empty() {
                let _ = menu.append(&MenuItem::new("(No speakers found)", false, None));
            }
        }
        self.devices = devices;
    }

    /// Set every check mark from the config
    fn sync(&self, cfg: &Config) {
        let l = engine::pick(&self.devices, &cfg.left);
        let r = engine::pick(&self.devices, &cfg.right);
        for (i, item) in self.left_items.iter().enumerate() {
            item.set_checked(l == Some(i));
        }
        for (i, item) in self.right_items.iter().enumerate() {
            item.set_checked(r == Some(i));
        }
        self.test_left.set_enabled(l.is_some());
        self.test_right.set_enabled(r.is_some());
        for (ms, item) in &self.latency {
            item.set_checked(*ms == cfg.latency_ms);
        }
        self.manage.set_checked(cfg.manage_default_device);
    }

    fn handle(&mut self, id: &MenuId) {
        let id_str = id.as_ref();
        let index = |prefix: &str| -> Option<usize> {
            id_str
                .strip_prefix(prefix)?
                .parse()
                .ok()
                .filter(|&i| i < self.devices.len())
        };
        let devices = self.devices.clone();

        let edited = if let Some(i) = index("left:") {
            edit_config(|c| choose(&mut c.left, &mut c.right, &devices[i], &devices))
        } else if let Some(i) = index("right:") {
            edit_config(|c| choose(&mut c.right, &mut c.left, &devices[i], &devices))
        } else if let Some(ms) = id_str.strip_prefix("latency:").and_then(|s| s.parse().ok()) {
            edit_config(|c| c.latency_ms = ms)
        } else if id == self.swap.id() {
            edit_config(|c| std::mem::swap(&mut c.left, &mut c.right))
        } else if id == self.manage.id() {
            edit_config(|c| c.manage_default_device = !c.manage_default_device)
        } else if id == self.test_left.id() || id == self.test_right.id() {
            if let Ok(cfg) = config::load() {
                let side = if id == self.test_left.id() {
                    &cfg.left
                } else {
                    &cfg.right
                };
                if let Some(i) = engine::pick(&devices, side) {
                    engine::play_test_tone(devices[i].clone());
                }
            }
            None
        } else if id == self.auto.id() {
            let want = self.auto.is_checked();
            if let Err(e) = set_autostart(want) {
                message_box(
                    &format!("Failed to enable/disable start with Windows: {e}"),
                    true,
                );
                self.auto.set_checked(!want);
            }
            None
        } else if id == self.open_config.id() {
            let _ = std::process::Command::new("notepad")
                .arg(config::config_path())
                .spawn();
            None
        } else if id == self.view_log.id() {
            let _ = std::process::Command::new("notepad")
                .arg(config::exe_dir().join("stereo-split.log"))
                .spawn();
            None
        } else {
            None
        };

        match edited {
            Some(cfg) => self.sync(&cfg),
            // Menu clicks toggle check marks on their own; put them back
            None => self.refresh(),
        }
    }
}

/// True if another copy of the program is already running
fn already_running() -> bool {
    unsafe {
        // The handle is deliberately kept open for the lifetime of the process
        CreateMutexW(None, false, w!("Local\\StereoSplit")).is_ok()
            && GetLastError() == ERROR_ALREADY_EXISTS
    }
}

unsafe extern "system" fn session_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_QUERYENDSESSION => LRESULT(1),
        WM_ENDSESSION => {
            if wparam.0 != 0 {
                log("Windows is logging off or shutting down");
                default_device::restore_from_disk();
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Hidden top-level window whose only job is to hear about logoff/shutdown
/// (message-only windows don't receive those broadcasts)
fn create_session_window() {
    unsafe {
        let Ok(module) = GetModuleHandleW(None) else {
            return;
        };
        let class = w!("StereoSplitSession");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(session_wndproc),
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
            log(&format!("Failed to create the session window: {e}"));
        }
    }
}

fn main() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--guard") {
        if let Some(pid) = args.get(2).and_then(|s| s.parse().ok()) {
            default_device::run_guard(pid);
        }
        return;
    }

    if already_running() {
        message_box(
            "Stereo Split is already running (see the system tray).",
            false,
        );
        return;
    }
    log("Program started");

    // Never leave the PC silent: switch the default device back on a panic, restart
    // automatically after a crash, and let a guard process clean up if this one is killed
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log(&format!("Panic: {info}"));
        default_device::restore_from_disk();
        default_hook(info);
    }));
    unsafe {
        let _ = RegisterApplicationRestart(PCWSTR::null(), RESTART_NO_REBOOT);
    }
    default_device::spawn_guard();
    create_session_window();

    let quit = Arc::new(AtomicBool::new(false));
    let status = Arc::new(Mutex::new(String::from("Starting")));
    spawn_supervisor(quit.clone(), status.clone());

    let mut tray = Tray::new();
    tray.refresh();

    let _tray_icon = match TrayIconBuilder::new()
        .with_menu(Box::new(tray.menu.clone()))
        .with_tooltip(APP_NAME)
        .with_icon(make_icon())
        .build()
    {
        Ok(t) => t,
        Err(e) => {
            message_box(&format!("Failed to create tray icon: {e}"), true);
            return;
        }
    };

    let menu_rx = MenuEvent::receiver();
    let tray_rx = TrayIconEvent::receiver();
    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);

            // Refresh the status text whenever a message is processed
            tray.status
                .set_text(format!("Status: {}", status.lock().unwrap()));

            // The mouse moving onto the icon comes before any right-click, so the device
            // list is fresh by the time the menu opens (e.g. after plugging in a speaker)
            let mut entered = false;
            while let Ok(ev) = tray_rx.try_recv() {
                entered |= matches!(ev, TrayIconEvent::Enter { .. });
            }
            if entered {
                tray.refresh();
            }

            while let Ok(ev) = menu_rx.try_recv() {
                if ev.id == *tray.quit.id() {
                    quit.store(true, Ordering::Relaxed);
                    PostQuitMessage(0);
                } else {
                    tray.handle(&ev.id);
                }
            }
        }
    }
    log("Program exited");
    // Give the audio threads a moment to wind down, then hand the default device back
    std::thread::sleep(Duration::from_millis(300));
    default_device::restore_from_disk();
}
