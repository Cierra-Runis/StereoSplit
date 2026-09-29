// Don't open a black console window in release builds
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod engine;
mod volume;

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};
use windows::core::HSTRING;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MessageBoxW, PostQuitMessage, TranslateMessage, MB_ICONERROR,
    MB_ICONINFORMATION, MB_OK, MSG,
};

const APP_NAME: &str = "Stereo Split";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

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
    let n = 32u32;
    let mut rgba = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let (dx, dy) = (x as f32 - 15.5, y as f32 - 15.5);
            let inside = dx * dx + dy * dy <= 15.0 * 15.0;
            let gap = (x == 15 || x == 16) && inside;
            let (r, g, b, a) = if !inside || gap {
                (0, 0, 0, 0)
            } else if x < 16 {
                (0x3b, 0x82, 0xf6, 255)
            } else {
                (0xf5, 0x9e, 0x0b, 255)
            };
            rgba.extend_from_slice(&[r, g, b, a]);
        }
    }
    Icon::from_rgba(rgba, n, n).expect("icon")
}

/// Audio supervisor thread: starts the engine, retries automatically after errors (e.g. a
/// speaker being unplugged), and restarts with the new config when a reload is requested.
fn spawn_supervisor(quit: Arc<AtomicBool>, reload: Arc<AtomicBool>, status: Arc<Mutex<String>>) {
    std::thread::spawn(move || {
        let mut shown_error: Option<String> = None;
        while !quit.load(Ordering::Relaxed) {
            reload.store(false, Ordering::Relaxed);
            engine::dump_devices(&config::exe_dir().join("devices.txt"));

            let cfg = match config::load() {
                Ok(Some(c)) => c,
                Ok(None) => {
                    let msg = String::from(
                        "First run: config.toml has been created in the program folder.\n\n\
                         Use \"Open config file\" in the tray menu to enter the names of both \
                         speakers\n(see devices.txt in the same folder), save, then choose \
                         \"Reload config\".",
                    );
                    *status.lock().unwrap() = "Waiting for config".into();
                    message_box(&msg, false);
                    wait_for_reload(&quit, &reload);
                    continue;
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    log(&msg);
                    *status.lock().unwrap() = "Config error".into();
                    message_box(&msg, true);
                    wait_for_reload(&quit, &reload);
                    continue;
                }
            };

            let vol_stop = Arc::new(AtomicBool::new(false));
            let gain = volume::Gain::new(1.0);
            volume::spawn_watcher(
                cfg.volume_endpoint.clone(),
                cfg.follow_windows_volume,
                gain.clone(),
                vol_stop.clone(),
            );

            let failed = Arc::new(AtomicBool::new(false));
            match engine::Engine::start(&cfg, gain, failed.clone()) {
                Ok(engine) => {
                    shown_error = None;
                    *status.lock().unwrap() = "Running".into();
                    while !quit.load(Ordering::Relaxed)
                        && !reload.load(Ordering::Relaxed)
                        && !failed.load(Ordering::Relaxed)
                    {
                        std::thread::sleep(Duration::from_millis(200));
                    }
                    drop(engine);
                    if failed.load(Ordering::Relaxed) {
                        log("Audio interrupted, reconnecting in 2 seconds");
                        *status.lock().unwrap() = "Reconnecting".into();
                        std::thread::sleep(Duration::from_secs(2));
                    }
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    log(&msg);
                    *status.lock().unwrap() = "Failed to start (see log)".into();
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
                        if quit.load(Ordering::Relaxed) || reload.load(Ordering::Relaxed) {
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

fn wait_for_reload(quit: &AtomicBool, reload: &AtomicBool) {
    while !quit.load(Ordering::Relaxed) && !reload.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn main() {
    log("Program started");
    let quit = Arc::new(AtomicBool::new(false));
    let reload = Arc::new(AtomicBool::new(false));
    let status = Arc::new(Mutex::new(String::from("Starting")));

    spawn_supervisor(quit.clone(), reload.clone(), status.clone());

    let menu = Menu::new();
    let item_status = MenuItem::new("Status: Starting", false, None);
    let item_edit = MenuItem::new("Open config file", true, None);
    let item_reload = MenuItem::new("Reload config", true, None);
    let item_log = MenuItem::new("View log", true, None);
    let item_auto = CheckMenuItem::new("Start with Windows", true, autostart_enabled(), None);
    let item_quit = MenuItem::new("Exit", true, None);
    let _ = menu.append_items(&[
        &item_status,
        &PredefinedMenuItem::separator(),
        &item_edit,
        &item_reload,
        &item_log,
        &item_auto,
        &PredefinedMenuItem::separator(),
        &item_quit,
    ]);

    let _tray = match TrayIconBuilder::new()
        .with_menu(Box::new(menu))
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
    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);

            // Refresh the status text whenever a message is processed
            item_status.set_text(format!("Status: {}", status.lock().unwrap()));

            while let Ok(ev) = menu_rx.try_recv() {
                let id = ev.id;
                if id == *item_quit.id() {
                    quit.store(true, Ordering::Relaxed);
                    PostQuitMessage(0);
                } else if id == *item_reload.id() {
                    reload.store(true, Ordering::Relaxed);
                } else if id == *item_edit.id() {
                    let _ = std::process::Command::new("notepad")
                        .arg(config::config_path())
                        .spawn();
                } else if id == *item_log.id() {
                    let _ = std::process::Command::new("notepad")
                        .arg(config::exe_dir().join("stereo-split.log"))
                        .spawn();
                } else if id == *item_auto.id() {
                    let want = item_auto.is_checked();
                    if let Err(e) = set_autostart(want) {
                        message_box(
                            &format!("Failed to enable/disable start with Windows: {e}"),
                            true,
                        );
                        item_auto.set_checked(!want);
                    }
                }
            }
        }
    }
    log("Program exited");
    // Give the audio threads a moment to wind down
    std::thread::sleep(Duration::from_millis(300));
}
