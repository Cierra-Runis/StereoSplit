//! Switches the Windows default playback device to the source (VB-CABLE's "CABLE Input")
//! while the program runs, and back to a speaker when it stops.
//!
//! Windows has no public API for changing the default device; everyone (SoundSwitch,
//! EarTrumpet, ...) uses the undocumented `IPolicyConfig` interface, as done here.
//!
//! Restoring happens in several places so the PC never ends up silent:
//! - on normal exit, on logoff/shutdown, and from the panic hook (in-process)
//! - from a guard process (this same exe with `--guard <pid>`) that waits for the main
//!   process to end for any reason, including a crash or being killed from Task Manager

use crate::config::{self, Config};
use crate::engine;
use anyhow::{anyhow, Context, Result};
use std::path::Path;
use std::sync::Mutex;
use tracing::{error, info};

use policy::IPolicyConfig;
use windows::core::{GUID, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Media::Audio::{
    eConsole, eMultimedia, eRender, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
    STGM_READ,
};
use windows::Win32::System::Threading::{
    OpenProcess, WaitForSingleObject, INFINITE, PROCESS_SYNCHRONIZE,
};

// In its own module because the interface macro accepts no attributes to silence lints
#[allow(non_snake_case, dead_code)]
mod policy {
    use std::ffi::c_void;
    use windows::core::{interface, IUnknown, IUnknown_Vtbl, HRESULT, PCWSTR};
    use windows::Win32::Media::Audio::ERole;

    #[interface("f8679f50-850a-41cf-9c72-430f290290c8")]
    pub unsafe trait IPolicyConfig: IUnknown {
        // Only SetDefaultEndpoint is called; the others just fill the vtable in the right order
        fn GetMixFormat(&self, device: PCWSTR, format: *mut *mut c_void) -> HRESULT;
        fn GetDeviceFormat(
            &self,
            device: PCWSTR,
            default: i32,
            format: *mut *mut c_void,
        ) -> HRESULT;
        fn ResetDeviceFormat(&self, device: PCWSTR) -> HRESULT;
        fn SetDeviceFormat(
            &self,
            device: PCWSTR,
            endpoint: *mut c_void,
            mix: *mut c_void,
        ) -> HRESULT;
        fn GetProcessingPeriod(
            &self,
            device: PCWSTR,
            default: i32,
            def: *mut i64,
            min: *mut i64,
        ) -> HRESULT;
        fn SetProcessingPeriod(&self, device: PCWSTR, period: *mut i64) -> HRESULT;
        fn GetShareMode(&self, device: PCWSTR, mode: *mut c_void) -> HRESULT;
        fn SetShareMode(&self, device: PCWSTR, mode: *mut c_void) -> HRESULT;
        fn GetPropertyValue(
            &self,
            device: PCWSTR,
            key: *const c_void,
            value: *mut c_void,
        ) -> HRESULT;
        fn SetPropertyValue(
            &self,
            device: PCWSTR,
            key: *const c_void,
            value: *mut c_void,
        ) -> HRESULT;
        fn SetDefaultEndpoint(&self, device: PCWSTR, role: ERole) -> HRESULT;
        fn SetEndpointVisibility(&self, device: PCWSTR, visible: i32) -> HRESULT;
    }

    // The generated methods are private to this module
    pub unsafe fn set_default_endpoint(p: &IPolicyConfig, device: PCWSTR, role: ERole) -> HRESULT {
        p.SetDefaultEndpoint(device, role)
    }
}

const CLSID_POLICY_CONFIG_CLIENT: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

/// The device we last switched back to, so switching over again later does not record it
/// as the device to come back to (that should stay whatever the user had originally)
static RESTORED_TO: Mutex<Option<String>> = Mutex::new(None);

fn restore_file() -> std::path::PathBuf {
    config::data_dir().join(config::RESTORE_FILE)
}

fn com_init() {
    // Fails harmlessly if COM is already initialized on this thread in another mode
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
}

unsafe fn enumerator() -> windows::core::Result<IMMDeviceEnumerator> {
    CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
}

pub unsafe fn friendly_name(dev: &IMMDevice) -> Option<String> {
    let store = dev.OpenPropertyStore(STGM_READ).ok()?;
    let pv = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
    let p: PWSTR = PropVariantToStringAlloc(&pv).ok()?;
    let s = p.to_string().ok();
    CoTaskMemFree(Some(p.0 as *const _));
    s
}

/// All active playback devices with their names.
/// The caller must have initialized COM on this thread.
unsafe fn render_devices() -> windows::core::Result<Vec<(String, IMMDevice)>> {
    let coll = enumerator()?.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
    let mut devs = Vec::new();
    for i in 0..coll.GetCount()? {
        let dev = coll.Item(i)?;
        if let Some(name) = friendly_name(&dev) {
            devs.push((name, dev));
        }
    }
    Ok(devs)
}

/// Names of all playback devices. They are the same names cpal reports, but reading them
/// straight from Windows is fast: cpal also probes the supported formats of every device,
/// which took ~230 ms and stalled the tray on every hover.
#[tracing::instrument(level = "debug")]
pub fn render_device_names() -> Vec<String> {
    com_init();
    unsafe { render_devices() }
        .map(|devs| devs.into_iter().map(|(name, _)| name).collect())
        .unwrap_or_default()
}

/// Active playback device matching `pattern`, chosen the same way as [`engine::pick`].
/// The caller must have initialized COM on this thread.
pub unsafe fn find_render_device(pattern: &str) -> windows::core::Result<Option<IMMDevice>> {
    let mut devs = render_devices()?;
    let names: Vec<String> = devs.iter().map(|(n, _)| n.clone()).collect();
    Ok(engine::pick(&names, pattern).map(|i| devs.swap_remove(i).1))
}

unsafe fn device_id(dev: &IMMDevice) -> Result<String> {
    let p = dev.GetId()?;
    let s = p.to_string();
    CoTaskMemFree(Some(p.0 as *const _));
    Ok(s?)
}

unsafe fn render_id(pattern: &str) -> Option<String> {
    find_render_device(pattern)
        .ok()
        .flatten()
        .and_then(|d| device_id(&d).ok())
}

unsafe fn current_default() -> Option<String> {
    let dev = enumerator()
        .ok()?
        .GetDefaultAudioEndpoint(eRender, eConsole)
        .ok()?;
    device_id(&dev).ok()
}

unsafe fn is_active(id: &str) -> bool {
    enumerator()
        .and_then(|e| e.GetDevice(&HSTRING::from(id)))
        .and_then(|d| d.GetState())
        .map(|s| s == DEVICE_STATE_ACTIVE)
        .unwrap_or(false)
}

unsafe fn set_default(id: &str) -> Result<()> {
    let policy: IPolicyConfig = CoCreateInstance(&CLSID_POLICY_CONFIG_CLIENT, None, CLSCTX_ALL)
        .context("Failed to access the Windows audio policy")?;
    let id = HSTRING::from(id);
    // Communications devices are left alone so calling apps keep their own choice
    for role in [eConsole, eMultimedia] {
        policy::set_default_endpoint(&policy, PCWSTR(id.as_ptr()), role).ok()?;
    }
    Ok(())
}

/// The playback device that apps should play into: the source if it is a playback device,
/// otherwise VB-CABLE's playback side named by `volume_endpoint` (for older configs where
/// the source is the recording device "CABLE Output")
unsafe fn cable_id(cfg: &Config) -> Option<String> {
    render_id(&cfg.source).or_else(|| render_id(&cfg.volume_endpoint))
}

/// Make the source the default playback device, remembering the current default so it can
/// be restored later. Does nothing if the source already is the default.
pub fn take_over(cfg: &Config) -> Result<()> {
    com_init();
    unsafe {
        let cable = cable_id(cfg).ok_or_else(|| {
            anyhow!(
                "Can't switch the default device: \"{}\" not found",
                cfg.source
            )
        })?;
        let current = current_default();
        if current.as_deref() == Some(cable.as_str()) {
            return Ok(());
        }
        if let Some(cur) = current {
            let restored_to = RESTORED_TO.lock().unwrap().clone();
            if restored_to.as_deref() != Some(cur.as_str()) || !restore_file().exists() {
                let _ = std::fs::write(restore_file(), &cur);
            }
        }
        set_default(&cable)?;
        info!("default playback device switched to the source");
        Ok(())
    }
}

/// If the source is still the default playback device, switch back to the device that was
/// the default before, or failing that to the left or right speaker.
pub fn restore(cfg: &Config) -> Result<()> {
    com_init();
    unsafe {
        let Some(cable) = cable_id(cfg) else {
            return Ok(());
        };
        if current_default().as_deref() != Some(cable.as_str()) {
            return Ok(());
        }
        let saved = std::fs::read_to_string(restore_file())
            .map(|s| s.trim().to_string())
            .ok();
        let candidates = [saved, render_id(&cfg.left), render_id(&cfg.right)];
        for id in candidates.into_iter().flatten() {
            if !id.is_empty() && id != cable && is_active(&id) {
                set_default(&id)?;
                info!("default playback device switched back");
                *RESTORED_TO.lock().unwrap() = Some(id);
                return Ok(());
            }
        }
        Err(anyhow!(
            "Couldn't switch the default playback device back: no speaker is available"
        ))
    }
}

/// Restore using the config on disk. Used on exit, from the panic hook and from the guard,
/// where no config is at hand. Errors are only logged.
pub fn restore_from_disk() {
    let Ok(cfg) = config::load() else {
        return;
    };
    if let Err(e) = restore(&cfg) {
        error!(
            error = %format_args!("{e:#}"),
            "failed to switch the default playback device back"
        );
    }
}

/// Start the guard process for the current process. It logs to `log_file` too.
pub fn spawn_guard(log_file: Option<&Path>) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let spawned = std::env::current_exe().and_then(|exe| {
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("--guard").arg(std::process::id().to_string());
        if let Some(f) = log_file {
            cmd.arg(f);
        }
        cmd.creation_flags(CREATE_NO_WINDOW).spawn()
    });
    if let Err(e) = spawned {
        error!(error = %e, "failed to start the guard process");
    }
}

/// Guard process body: wait for the main process to end, however it ends, then restore
pub fn run_guard(pid: u32) {
    unsafe {
        // If it can't be opened, it has already exited
        if let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            WaitForSingleObject(h, INFINITE);
            let _ = CloseHandle(h);
        }
    }
    restore_from_disk();
}
