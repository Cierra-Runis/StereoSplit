// Don't open a black console window in release builds
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod default_device;
mod device_watch;
mod engine;
mod logging;
mod toast;
mod volume;

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use config::Config;
use tracing::{debug, error, info, warn};
use tray_icon::menu::{
    CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{Icon, TrayIconBuilder, TrayIconEvent};
use windows::core::{w, PCWSTR};
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
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, GetSystemMetrics,
    PostQuitMessage, RegisterClassW, TranslateMessage, MSG, SM_CXSMICON, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_ENDSESSION, WM_QUERYENDSESSION, WNDCLASSW,
};

/// Both defined in build.rs, which also writes them into the exe's resources
const APP_NAME: &str = env!("APP_NAME");
const ICON_RESOURCE: &str = env!("ICON_RESOURCE");
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const LATENCIES: [u32; 4] = [15, 20, 30, 50];
/// How long to wait before retrying a failed start when no device change comes first.
/// Device changes retry right away; this covers failures they don't announce, such as a
/// speaker held by another app in exclusive mode.
const RETRY_FALLBACK: Duration = Duration::from_secs(30);
/// Quiet time that ends a burst of config saves or of device changes (see `debounce`)
const CONFIG_QUIET: Duration = Duration::from_millis(150);
const DEVICES_QUIET: Duration = Duration::from_millis(500);

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

/// Tray icon (left half blue, right half orange, for the left and right channels), embedded
/// by build.rs from assets/icon/icon.ico at the size the tray uses on this screen
fn tray_icon() -> Icon {
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) } as u32;
    Icon::from_resource_name(ICON_RESOURCE, Some((size, size))).expect("icon resource")
}

/// What the supervisor thread waits for
enum Event {
    /// Exit was chosen from the tray menu
    Quit,
    /// A running audio stream failed (e.g. a speaker was unplugged)
    Failed {
        /// Of the engine it came from, since a stopped engine can still report failures
        /// for a while
        generation: u64,
        /// "input", "left" or "right"
        stream: &'static str,
    },
    /// config.toml was saved, from the tray menu or by hand
    ConfigChanged,
    /// An audio device was plugged in, unplugged, enabled or disabled
    DevicesChanged,
}

/// Report saves of config.toml. The folder is watched rather than the file, since some
/// editors save by writing a new file and renaming it over the old one.
fn watch_config(tx: Sender<Event>) -> Option<notify::RecommendedWatcher> {
    use notify::{EventKind, RecursiveMode, Watcher};
    let handler = move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else { return };
        let saved = matches!(
            ev.kind,
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
        ) && ev
            .paths
            .iter()
            .any(|p| p.file_name().is_some_and(|n| n == config::CONFIG_FILE));
        if saved {
            let _ = tx.send(Event::ConfigChanged);
        }
    };
    let watcher = notify::recommended_watcher(handler).and_then(|mut w| {
        w.watch(config::data_dir(), RecursiveMode::NonRecursive)?;
        Ok(w)
    });
    match watcher {
        Ok(w) => Some(w),
        Err(e) => {
            warn!(
                error = %e,
                "can't watch the config file; changes made by hand need a restart"
            );
            None
        }
    }
}

/// Let a change settle until nothing arrives for `quiet`: one save often arrives as several
/// events (e.g. truncate, then write), and so does plugging in a device.
/// Returns false on quit.
fn debounce(rx: &Receiver<Event>, quiet: Duration) -> bool {
    loop {
        match rx.recv_timeout(quiet) {
            Ok(Event::Quit) | Err(RecvTimeoutError::Disconnected) => return false,
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => return true,
        }
    }
}

/// Wait for the config to change. With `retry`, also stop waiting when a device changes or
/// after the time given, whichever comes first. Returns false on quit.
/// Failures still queued from an engine that is already gone are skipped here.
fn wait(rx: &Receiver<Event>, retry: Option<Duration>) -> bool {
    let deadline = retry.map(|t| Instant::now() + t);
    loop {
        let ev = match deadline {
            Some(d) => match rx.recv_timeout(d.saturating_duration_since(Instant::now())) {
                Ok(ev) => ev,
                Err(RecvTimeoutError::Timeout) => return true,
                Err(RecvTimeoutError::Disconnected) => return false,
            },
            None => match rx.recv() {
                Ok(ev) => ev,
                Err(_) => return false,
            },
        };
        match ev {
            Event::Quit => return false,
            Event::ConfigChanged => return debounce(rx, CONFIG_QUIET),
            Event::DevicesChanged if retry.is_some() => return debounce(rx, DEVICES_QUIET),
            Event::DevicesChanged | Event::Failed { .. } => {}
        }
    }
}

/// Audio supervisor thread: starts the engine, retries automatically after errors (e.g. a
/// speaker being unplugged), and restarts with the new config whenever config.toml changes
/// (whether from the tray menu or edited by hand).
fn spawn_supervisor(tx: Sender<Event>, rx: Receiver<Event>, status: Arc<Mutex<String>>) {
    std::thread::spawn(move || {
        let set_status = |s: &str| *status.lock().unwrap() = s.into();
        let mut shown_error: Option<String> = None;
        // Last start error written to the log, so retries that fail the same way don't
        // repeat it
        let mut logged_error: Option<String> = None;
        let mut hinted = false;
        // Whether this process has made the source the default playback device
        let mut managed = false;

        let _watcher = watch_config(tx.clone());
        default_device::com_init();
        let devices_tx = tx.clone();
        let _devices = unsafe {
            device_watch::DeviceWatch::new(move || {
                let _ = devices_tx.send(Event::DevicesChanged);
            })
        }
        .inspect_err(|e| {
            warn!(
                error = %e,
                retry_in_secs = RETRY_FALLBACK.as_secs(),
                "can't watch for device changes; reconnecting only retries on a timer"
            )
        })
        .ok();
        // Follows the volume of `volume_endpoint`; replaced only when that setting changes
        let mut volume: Option<volume::Follower> = None;
        // Counts engine starts, so failures from an earlier engine can be told apart
        let mut generation = 0u64;
        // The config whose engine was running until the audio was interrupted (e.g. a speaker
        // was unplugged). Starting it again is reconnecting, not a new failure to start.
        let mut interrupted: Option<Config> = None;

        loop {
            let cfg = match config::load() {
                Ok(c) => c,
                Err(e) => {
                    let msg = format!("{e:#}");
                    error!(error = %msg, "failed to load the config");
                    set_status("Config error");
                    if shown_error.as_deref() != Some(msg.as_str()) {
                        toast::show("Config error", &msg);
                        shown_error = Some(msg);
                    }
                    if !wait(&rx, None) {
                        break;
                    }
                    continue;
                }
            };

            if cfg.left.is_empty() || cfg.right.is_empty() {
                release(&cfg, &mut managed);
                set_status("Choose speakers");
                if !hinted {
                    hinted = true;
                    toast::show(
                        "Choose speakers",
                        "Right-click the tray icon (the blue and orange dot) and choose your \
                         speakers under \"Left speaker\" and \"Right speaker\".",
                    );
                }
                if !wait(&rx, None) {
                    break;
                }
                continue;
            }

            let gain = match &volume {
                Some(v) if v.pattern() == cfg.volume_endpoint => v.gain(),
                _ => volume
                    .insert(volume::Follower::start(cfg.volume_endpoint.clone()))
                    .gain(),
            };

            generation += 1;
            let on_error: engine::OnError = {
                let tx = tx.clone();
                let generation = generation;
                Arc::new(move |stream| {
                    let _ = tx.send(Event::Failed { generation, stream });
                })
            };

            let go_on = match engine::Engine::start(&cfg, gain, on_error) {
                Ok(engine) => {
                    shown_error = None;
                    logged_error = None;
                    interrupted = None;
                    set_status("Running");
                    match default_device::take_over(&cfg) {
                        Ok(()) => managed = true,
                        Err(e) => error!(
                            error = %format_args!("{e:#}"),
                            "failed to switch the default playback device"
                        ),
                    }
                    let ev = loop {
                        match rx.recv() {
                            // From an engine that was already stopped
                            Ok(Event::Failed { generation: g, .. }) if g != generation => {}
                            // A running engine hears about its own devices going away
                            Ok(Event::DevicesChanged) => {}
                            Ok(ev) => break ev,
                            Err(_) => break Event::Quit,
                        }
                    };
                    // Stops the engine without waiting for it (see `engine::Engine`)
                    drop(engine);
                    match ev {
                        Event::Quit => false,
                        Event::ConfigChanged => debounce(&rx, CONFIG_QUIET),
                        Event::DevicesChanged => unreachable!("skipped above"),
                        Event::Failed { stream, .. } => {
                            warn!(stream, retry_in_secs = 2, "audio interrupted, reconnecting");
                            set_status("Reconnecting");
                            // Shown once; the retries stay quiet until it's back
                            let (what, name) = match stream {
                                "left" => ("Left speaker", &cfg.left),
                                "right" => ("Right speaker", &cfg.right),
                                _ => ("Sound source", &cfg.source),
                            };
                            toast::show(
                                &format!("{what} disconnected"),
                                &format!(
                                    "Lost \"{name}\". Reconnecting automatically once it's back."
                                ),
                            );
                            interrupted = Some(cfg.clone());
                            wait(&rx, Some(Duration::from_secs(2)))
                        }
                    }
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    if logged_error.as_deref() != Some(msg.as_str()) {
                        error!(error = %msg, "failed to start the engine");
                        logged_error = Some(msg.clone());
                    } else {
                        debug!(error = %msg, "failed to start the engine again");
                    }
                    // Once the config is changed, a failure is a failure to start again
                    let reconnecting = interrupted.as_ref() == Some(&cfg);
                    set_status(if reconnecting {
                        "Reconnecting"
                    } else {
                        "Failed to start (see log)"
                    });
                    // Nothing is playing through the source now, so let Windows play
                    // straight to a speaker until the engine is back
                    release(&cfg, &mut managed);
                    // Show each distinct error only once, then retry silently
                    // (e.g. a speaker that isn't plugged in yet). Nothing is shown while
                    // reconnecting: the disconnect was shown already.
                    if !reconnecting && shown_error.as_deref() != Some(msg.as_str()) {
                        toast::show(
                            "Failed to start",
                            &format!(
                                "{msg}\n\nRetrying automatically when a device is plugged in."
                            ),
                        );
                        shown_error = Some(msg);
                    }
                    wait(&rx, Some(RETRY_FALLBACK))
                }
            };
            if !go_on {
                break;
            }
        }
    });
}

/// Switch the default playback device back if this process took it over
fn release(cfg: &Config, managed: &mut bool) {
    if std::mem::take(managed) {
        if let Err(e) = default_device::restore(cfg) {
            error!(
                error = %format_args!("{e:#}"),
                "failed to switch the default playback device back"
            );
        }
    }
}

/// Load the config, change it and save it. On a broken config file nothing is changed.
fn edit_config(f: impl FnOnce(&mut Config)) -> Option<Config> {
    match config::load() {
        Ok(mut cfg) => {
            f(&mut cfg);
            if let Err(e) = config::save(&cfg) {
                toast::show("Failed to save the config", &format!("{e:#}"));
            }
            Some(cfg)
        }
        Err(e) => {
            toast::show(
                "Config error",
                &format!("{e:#}\n\nFix config.toml (or delete it to start over) first."),
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
            auto: CheckMenuItem::new("Start with Windows", true, autostart_enabled(), None),
            open_config: MenuItem::new("Open config file", true, None),
            view_log: MenuItem::new("Open log folder", true, None),
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
            &tray.auto,
            &PredefinedMenuItem::separator(),
            &tray.open_config,
            &tray.view_log,
            &PredefinedMenuItem::separator(),
            &tray.quit,
        ]);
        tray
    }

    /// Re-read the config and the device list, and update the menu to match.
    /// Runs on the UI thread, so it is timed to see whether it ever stalls the tray.
    #[tracing::instrument(level = "debug", skip_all)]
    fn refresh(&mut self) {
        let started = Instant::now();
        let Ok(cfg) = config::load() else {
            return;
        };
        let hide = |n: &str| {
            let n = n.to_lowercase();
            [&cfg.source, &cfg.volume_endpoint]
                .iter()
                .any(|p| !p.is_empty() && n.contains(&p.to_lowercase()))
        };
        let devices: Vec<String> = default_device::render_device_names()
            .into_iter()
            .filter(|n| !hide(n))
            .collect();
        if devices != self.devices {
            self.rebuild_devices(devices);
        }
        self.sync(&cfg);
        let elapsed_ms = started.elapsed().as_millis() as u64;
        if elapsed_ms > 200 {
            warn!(elapsed_ms, "tray refresh was slow");
        }
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
                toast::show(
                    "Failed to enable/disable start with Windows",
                    &e.to_string(),
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
            let _ = std::process::Command::new("explorer")
                .arg(config::log_dir())
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
            error!(error = %e, "failed to create the session window");
        }
    }
}

fn main() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--guard") {
        // Append to the main process's log file rather than start one per guard
        logging::init(args.get(3).map(PathBuf::from));
        if let Some(pid) = args.get(2).and_then(|s| s.parse().ok()) {
            default_device::run_guard(pid);
        }
        return;
    }

    if already_running() {
        toast::show(
            "Already running",
            "Stereo Split is already running (see the system tray).",
        );
        return;
    }
    // Only now, so a second copy that exits right away leaves no log file behind
    let log_file = logging::init(None);
    info!(version = env!("CARGO_PKG_VERSION"), "program started");

    // Never leave the PC silent: switch the default device back on a panic, restart
    // automatically after a crash, and let a guard process clean up if this one is killed
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        error!(panic = %info, "panic");
        default_device::restore_from_disk();
        default_hook(info);
    }));
    unsafe {
        let _ = RegisterApplicationRestart(PCWSTR::null(), RESTART_NO_REBOOT);
    }
    default_device::spawn_guard(log_file.as_deref());
    create_session_window();

    let (tx, rx) = mpsc::channel();
    let status = Arc::new(Mutex::new(String::from("Starting")));
    spawn_supervisor(tx.clone(), rx, status.clone());

    let mut tray = Tray::new();
    tray.refresh();

    let _tray_icon = match TrayIconBuilder::new()
        .with_menu(Box::new(tray.menu.clone()))
        .with_tooltip(APP_NAME)
        .with_icon(tray_icon())
        .build()
    {
        Ok(t) => t,
        Err(e) => {
            toast::show("Failed to create the tray icon", &e.to_string());
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
                    let _ = tx.send(Event::Quit);
                    PostQuitMessage(0);
                } else {
                    tray.handle(&ev.id);
                }
            }
        }
    }
    info!("program exited");
    // Give the audio threads a moment to wind down, then hand the default device back
    std::thread::sleep(Duration::from_millis(300));
    default_device::restore_from_disk();
}
