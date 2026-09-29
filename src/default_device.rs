//! Switches the Windows default playback device to the source (VB-CABLE's "CABLE Input")
//! while the program runs, and back to a speaker when it stops.
//!
//! Windows has no public API for changing the default device; everyone (SoundSwitch,
//! EarTrumpet, ...) uses the undocumented `IPolicyConfig` interface, as done here.
//!
//! Restoring happens on normal exit, on logoff/shutdown, and from the panic hook. After any
//! other crash Windows restarts the program, which brings the sound back. Being killed from
//! Task Manager leaves the source as the default until the program is started again.

use crate::config::{self, Config};
use crate::devices::{com_init, device_id, enumerator, find_render_device};
use anyhow::{anyhow, Context, Result};
use std::sync::Mutex;
use tracing::{error, info};

use policy::IPolicyConfig;
use windows::core::{GUID, HSTRING, PCWSTR};
use windows::Win32::Media::Audio::{eConsole, eMultimedia, eRender, DEVICE_STATE_ACTIVE};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

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

unsafe fn render_id(pattern: &str) -> Option<String> {
    device_id(&find_render_device(pattern)?)
}

unsafe fn current_default() -> Option<String> {
    let dev = enumerator()
        .ok()?
        .GetDefaultAudioEndpoint(eRender, eConsole)
        .ok()?;
    device_id(&dev)
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
/// the default before, or failing that to the left or right speaker. Errors are only logged.
pub fn restore(cfg: &Config) {
    if let Err(e) = try_restore(cfg) {
        error!(
            error = %format_args!("{e:#}"),
            "failed to switch the default playback device back"
        );
    }
}

fn try_restore(cfg: &Config) -> Result<()> {
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

/// Restore using the config on disk. Used on exit and from the panic hook, where no config
/// is at hand.
pub fn restore_from_disk() {
    if let Ok(cfg) = config::load() {
        restore(&cfg);
    }
}
