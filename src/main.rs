// Don't open a black console window in release builds
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod config;
mod default_device;
mod devices;
mod engine;
mod latency;
mod logging;
mod meter;
mod session;
mod supervisor;
mod toast;
mod volume;

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use config::Config;
use tracing::{error, info, warn};
use tray_icon::menu::{
    CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{Icon, TrayIconBuilder, TrayIconEvent};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Recovery::{RegisterApplicationRestart, RESTART_NO_REBOOT};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, GetSystemMetrics, PostQuitMessage, PostThreadMessageW,
    TranslateMessage, MSG, SM_CXSMICON, WM_NULL,
};

/// Both defined in build.rs, which also writes them into the exe's resources
const APP_NAME: &str = env!("APP_NAME");
const ICON_RESOURCE: &str = env!("ICON_RESOURCE");

/// Tray icon (left half blue, right half orange, for the left and right channels), embedded
/// by build.rs from assets/icon/icon.ico at the size the tray uses on this screen
fn tray_icon() -> Icon {
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) } as u32;
    Icon::from_resource_name(ICON_RESOURCE, Some((size, size))).expect("icon resource")
}

/// Load the config, change it and save it. On a broken config file nothing is changed.
fn edit_config(f: impl FnOnce(&mut Config)) {
    match config::load() {
        Ok(mut cfg) => {
            f(&mut cfg);
            if let Err(e) = config::save(&cfg) {
                toast::show("Failed to save the config", &format!("{e:#}"));
            }
        }
        Err(e) => toast::show(
            "Config error",
            &format!("{e:#}\n\nFix config.toml (or delete it to start over) first."),
        ),
    }
}

/// Put `name` on `side` (0 = left, 1 = right). If it was on the other side, the two sides swap.
fn choose(cfg: &mut Config, side: usize, name: &str, devices: &[String]) {
    let (this, other) = if side == 0 {
        (&mut cfg.left, &mut cfg.right)
    } else {
        (&mut cfg.right, &mut cfg.left)
    };
    if devices::pick(devices, other) == devices::pick(devices, name) {
        *other = this.clone();
    }
    *this = name.to_string();
}

/// Menu text: '&' would otherwise mark a keyboard shortcut
fn menu_text(s: &str) -> String {
    s.replace('&', "&&")
}

/// Play the test tone on the speaker chosen for `side`
fn test_tone(devices: &[String], side: usize) {
    let Ok(cfg) = config::load() else {
        return;
    };
    if let Some(i) = devices::pick(devices, cfg.speakers()[side]) {
        engine::play_test_tone(devices[i].clone());
    }
}

fn toggle_autostart() {
    if let Err(e) = autostart::set(!autostart::enabled()) {
        toast::show(
            "Failed to enable/disable start with Windows",
            &e.to_string(),
        );
    }
}

/// Show `path` in Explorer
fn open(path: &Path) {
    let _ = std::process::Command::new("explorer").arg(path).spawn();
}

/// What clicking a menu item does
#[derive(Clone, Copy)]
enum Action {
    /// `Speaker(side, i)`: put `Tray::devices[i]` on `side` (0 = left, 1 = right)
    Speaker(usize, usize),
    /// Play the test tone on a side
    Test(usize),
    Swap,
    Autostart,
    OpenConfig,
    OpenLog,
    Quit,
}

/// Menu items being built, and what clicking each one does
#[derive(Default)]
struct Actions(HashMap<MenuId, Action>);

impl Actions {
    fn item(&mut self, text: &str, enabled: bool, action: Action) -> MenuItem {
        let item = MenuItem::new(text, enabled, None);
        self.0.insert(item.id().clone(), action);
        item
    }

    fn check(&mut self, text: &str, checked: bool, action: Action) -> CheckMenuItem {
        let item = CheckMenuItem::new(text, true, checked, None);
        self.0.insert(item.id().clone(), action);
        item
    }
}

struct Tray {
    menu: Menu,
    status: MenuItem,
    /// Speakers listed in the Left/Right submenus, which [`Action::Speaker`] indexes into
    devices: Vec<String>,
    /// What clicking each item in `menu` does
    actions: HashMap<MenuId, Action>,
}

impl Tray {
    fn new() -> Tray {
        let mut tray = Tray {
            menu: Menu::new(),
            status: MenuItem::new("Status: Starting", false, None),
            devices: Vec::new(),
            actions: HashMap::new(),
        };
        tray.refresh();
        tray
    }

    /// Rebuild the menu from the config and the device list.
    /// Runs on the UI thread, so it is timed to see whether it ever stalls the tray.
    #[tracing::instrument(level = "debug", skip_all)]
    fn refresh(&mut self) {
        let started = Instant::now();
        let cfg = match config::load() {
            Ok(cfg) => cfg,
            // Keep showing the last config that loaded; the first menu has to show something
            Err(_) if !self.actions.is_empty() => return,
            Err(_) => Config::default(),
        };
        let hide = |n: &str| {
            [&cfg.source, &cfg.volume_endpoint]
                .iter()
                .any(|p| devices::matches(n, p))
        };
        let devices: Vec<String> = devices::render_device_names()
            .into_iter()
            .filter(|n| !hide(n))
            .collect();
        let chosen = cfg.speakers().map(|s| devices::pick(&devices, s));

        let mut a = Actions::default();
        let titles = [(0, "Left speaker"), (1, "Right speaker")];
        let [left, right] = titles.map(|(side, title)| {
            let menu = Submenu::new(title, true);
            for (i, name) in devices.iter().enumerate() {
                let checked = chosen[side] == Some(i);
                let item = a.check(&menu_text(name), checked, Action::Speaker(side, i));
                let _ = menu.append(&item);
            }
            if devices.is_empty() {
                let _ = menu.append(&MenuItem::new("(No speakers found)", false, None));
            }
            menu
        });
        let separator = PredefinedMenuItem::separator;
        while self.menu.remove_at(0).is_some() {}
        let _ = self.menu.append_items(&[
            &self.status,
            &separator(),
            &left,
            &right,
            &a.item("Swap left / right", true, Action::Swap),
            &a.item("Test left", chosen[0].is_some(), Action::Test(0)),
            &a.item("Test right", chosen[1].is_some(), Action::Test(1)),
            &separator(),
            &a.check(
                "Start with Windows",
                autostart::enabled(),
                Action::Autostart,
            ),
            &separator(),
            &a.item("Open config file", true, Action::OpenConfig),
            &a.item("Open log folder", true, Action::OpenLog),
            &separator(),
            &a.item("Exit", true, Action::Quit),
        ]);
        self.actions = a.0;
        self.devices = devices;

        let elapsed_ms = started.elapsed().as_millis() as u64;
        if elapsed_ms > 200 {
            warn!(elapsed_ms, "tray refresh was slow");
        }
    }

    /// Do what the clicked item says. Breaks when Exit was clicked.
    fn handle(&mut self, id: &MenuId) -> ControlFlow<()> {
        let Some(&action) = self.actions.get(id) else {
            return ControlFlow::Continue(());
        };
        let devices = &self.devices;
        match action {
            Action::Speaker(side, i) => edit_config(|c| choose(c, side, &devices[i], devices)),
            Action::Swap => edit_config(|c| std::mem::swap(&mut c.left, &mut c.right)),
            Action::Test(side) => test_tone(devices, side),
            Action::Autostart => toggle_autostart(),
            Action::OpenConfig => open(&config::config_path()),
            Action::OpenLog => open(&config::log_dir()),
            Action::Quit => return ControlFlow::Break(()),
        }
        // Clicks toggle check marks on their own; rebuild the menu from what was saved
        self.refresh();
        ControlFlow::Continue(())
    }
}

fn main() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    if session::already_running() {
        toast::show(
            "Already running",
            "Stereo Split is already running (see the system tray).",
        );
        return;
    }
    // Only now, so a second copy that exits right away leaves no log file behind
    logging::init();
    info!(version = env!("CARGO_PKG_VERSION"), "program started");

    // Never leave the PC silent: switch the default device back on a panic, and restart
    // automatically after any other crash
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        error!(panic = %info, "panic");
        default_device::restore_from_disk();
        default_hook(info);
    }));
    unsafe {
        let _ = RegisterApplicationRestart(PCWSTR::null(), RESTART_NO_REBOOT);
    }
    session::create_window();

    let (tx, rx) = mpsc::channel();
    let status = Arc::new(Mutex::new("Starting"));
    // Set when the config or the devices change, for the message loop below to rebuild the
    // menu. tray-icon opens the menu right on the right-click, before we hear of the click,
    // so the menu has to be up to date before then.
    let stale = Arc::new(AtomicBool::new(false));
    let ui_thread = unsafe { GetCurrentThreadId() };
    supervisor::spawn(tx.clone(), rx, status.clone(), {
        let stale = stale.clone();
        move || {
            stale.store(true, Ordering::Relaxed);
            // Wake the message loop. If the open menu's own loop takes this message, the flag
            // is still seen on the next one.
            unsafe {
                let _ = PostThreadMessageW(ui_thread, WM_NULL, WPARAM(0), LPARAM(0));
            }
        }
    });

    let mut tray = Tray::new();

    // Tray icon events aren't used; without a handler they would pile up unread
    TrayIconEvent::set_event_handler(Some(|_| {}));
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
    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);

            // Refresh the status text whenever a message is processed
            tray.status
                .set_text(format!("Status: {}", status.lock().unwrap()));

            if stale.swap(false, Ordering::Relaxed) {
                tray.refresh();
            }

            while let Ok(ev) = menu_rx.try_recv() {
                if tray.handle(&ev.id).is_break() {
                    PostQuitMessage(0);
                }
            }
        }
    }
    // However the loop ended, stop the audio
    let _ = tx.send(supervisor::Event::Quit);
    info!("program exited");
    // Give the audio threads a moment to wind down, then hand the default device back
    std::thread::sleep(Duration::from_millis(300));
    default_device::restore_from_disk();
}
