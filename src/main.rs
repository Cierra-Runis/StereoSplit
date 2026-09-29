// Don't open a black console window in release builds
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod config;
mod default_device;
mod devices;
mod engine;
mod logging;
mod session;
mod supervisor;
mod toast;
mod volume;

use std::path::PathBuf;
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
use windows::Win32::System::Recovery::{RegisterApplicationRestart, RESTART_NO_REBOOT};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, GetSystemMetrics, PostQuitMessage, TranslateMessage, MSG,
    SM_CXSMICON,
};

/// Both defined in build.rs, which also writes them into the exe's resources
const APP_NAME: &str = env!("APP_NAME");
const ICON_RESOURCE: &str = env!("ICON_RESOURCE");
const LATENCIES: [u32; 4] = [15, 20, 30, 50];

/// Tray icon (left half blue, right half orange, for the left and right channels), embedded
/// by build.rs from assets/icon/icon.ico at the size the tray uses on this screen
fn tray_icon() -> Icon {
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) } as u32;
    Icon::from_resource_name(ICON_RESOURCE, Some((size, size))).expect("icon resource")
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

/// The menu items for one speaker
struct SideMenu {
    /// Lists the speakers in `Tray::devices`, one item each, in the same order
    menu: Submenu,
    items: Vec<CheckMenuItem>,
    test: MenuItem,
}

impl SideMenu {
    fn new(menu: &str, test: &str) -> SideMenu {
        SideMenu {
            menu: Submenu::new(menu, true),
            items: Vec::new(),
            test: MenuItem::new(test, true, None),
        }
    }
}

struct Tray {
    menu: Menu,
    status: MenuItem,
    /// Speakers currently listed in the Left/Right submenus
    devices: Vec<String>,
    /// Left and right, in the order of [`Config::speakers`]
    sides: [SideMenu; 2],
    swap: MenuItem,
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
                let item = CheckMenuItem::new(format!("{ms} ms"), true, false, None);
                let _ = latency_menu.append(&item);
                (ms, item)
            })
            .collect();

        let tray = Tray {
            menu: Menu::new(),
            status: MenuItem::new("Status: Starting", false, None),
            devices: Vec::new(),
            sides: [
                SideMenu::new("Left speaker", "Test left"),
                SideMenu::new("Right speaker", "Test right"),
            ],
            swap: MenuItem::new("Swap left / right", true, None),
            latency,
            auto: CheckMenuItem::new("Start with Windows", true, autostart::enabled(), None),
            open_config: MenuItem::new("Open config file", true, None),
            view_log: MenuItem::new("Open log folder", true, None),
            quit: MenuItem::new("Exit", true, None),
        };
        let [left, right] = &tray.sides;
        let _ = tray.menu.append_items(&[
            &tray.status,
            &PredefinedMenuItem::separator(),
            &left.menu,
            &right.menu,
            &tray.swap,
            &left.test,
            &right.test,
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
            [&cfg.source, &cfg.volume_endpoint]
                .iter()
                .any(|p| devices::matches(n, p))
        };
        let devices: Vec<String> = devices::render_device_names()
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
        for side in &mut self.sides {
            while side.menu.remove_at(0).is_some() {}
            side.items = devices
                .iter()
                .map(|name| {
                    let item = CheckMenuItem::new(menu_text(name), true, false, None);
                    let _ = side.menu.append(&item);
                    item
                })
                .collect();
            if devices.is_empty() {
                let _ = side
                    .menu
                    .append(&MenuItem::new("(No speakers found)", false, None));
            }
        }
        self.devices = devices;
    }

    /// Set every check mark from the config
    fn sync(&self, cfg: &Config) {
        for (side, speaker) in self.sides.iter().zip(cfg.speakers()) {
            let chosen = devices::pick(&self.devices, speaker);
            for (i, item) in side.items.iter().enumerate() {
                item.set_checked(chosen == Some(i));
            }
            side.test.set_enabled(chosen.is_some());
        }
        for (ms, item) in &self.latency {
            item.set_checked(*ms == cfg.latency_ms);
        }
    }

    fn handle(&mut self, id: &MenuId) {
        let devices = &self.devices;
        let speaker = self.sides.iter().enumerate().find_map(|(side, menu)| {
            let i = menu.items.iter().position(|item| item.id() == id)?;
            Some((side, i))
        });
        let test = self.sides.iter().position(|menu| menu.test.id() == id);
        let latency = self.latency.iter().find(|(_, item)| item.id() == id);

        let edited = if let Some((side, i)) = speaker {
            edit_config(|c| choose(c, side, &devices[i], devices))
        } else if let Some(&(ms, _)) = latency {
            edit_config(|c| c.latency_ms = ms)
        } else if id == self.swap.id() {
            edit_config(|c| std::mem::swap(&mut c.left, &mut c.right))
        } else if let Some(side) = test {
            if let Ok(cfg) = config::load() {
                if let Some(i) = devices::pick(devices, cfg.speakers()[side]) {
                    engine::play_test_tone(devices[i].clone());
                }
            }
            None
        } else if id == self.auto.id() {
            let want = self.auto.is_checked();
            if let Err(e) = autostart::set(want) {
                toast::show(
                    "Failed to enable/disable start with Windows",
                    &e.to_string(),
                );
                self.auto.set_checked(!want);
            }
            None
        } else if id == self.open_config.id() {
            let _ = std::process::Command::new("explorer")
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

    if session::already_running() {
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
    session::create_window();

    let (tx, rx) = mpsc::channel();
    let status = Arc::new(Mutex::new("Starting"));
    supervisor::spawn(tx.clone(), rx, status.clone());

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
                    let _ = tx.send(supervisor::Event::Quit);
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
