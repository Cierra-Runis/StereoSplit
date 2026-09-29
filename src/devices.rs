//! Audio devices as Windows lists them: finding a playback device by name, and hearing about
//! devices being plugged in, unplugged, enabled or disabled (so waiting for one needs no
//! polling).

use tracing::debug;
use windows::core::{implement, Result, PCWSTR, PWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    eRender, EDataFlow, ERole, IMMDevice, IMMDeviceEnumerator, IMMNotificationClient,
    IMMNotificationClient_Impl, MMDeviceEnumerator, DEVICE_STATE, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
    STGM_READ,
};
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;

/// Index of the name matching `pat`: an exact (case-insensitive) name match wins,
/// otherwise the first name containing it. An empty pattern matches nothing.
pub fn pick<S: AsRef<str>>(names: impl IntoIterator<Item = S>, pat: &str) -> Option<usize> {
    if pat.is_empty() {
        return None;
    }
    let pat = pat.to_lowercase();
    let lower: Vec<String> = names
        .into_iter()
        .map(|n| n.as_ref().to_lowercase())
        .collect();
    lower
        .iter()
        .position(|n| *n == pat)
        .or_else(|| lower.iter().position(|n| n.contains(&pat)))
}

/// The item whose name matches `pat`, chosen as by [`pick`]
pub fn pick_from<T>(mut items: Vec<(String, T)>, pat: &str) -> Option<T> {
    let i = pick(items.iter().map(|(name, _)| name), pat)?;
    Some(items.swap_remove(i).1)
}

/// Whether `name` contains `pat`, ignoring case. An empty pattern matches nothing.
pub fn matches(name: &str, pat: &str) -> bool {
    !pat.is_empty() && name.to_lowercase().contains(&pat.to_lowercase())
}

pub fn com_init() {
    // Fails harmlessly if COM is already initialized on this thread in another mode
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
}

pub unsafe fn enumerator() -> Result<IMMDeviceEnumerator> {
    CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
}

/// Copy a string Windows allocated, and free it
unsafe fn take_string(p: PWSTR) -> Option<String> {
    let s = p.to_string().ok();
    CoTaskMemFree(Some(p.0 as *const _));
    s
}

unsafe fn friendly_name(dev: &IMMDevice) -> Option<String> {
    let store = dev.OpenPropertyStore(STGM_READ).ok()?;
    let pv = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
    take_string(PropVariantToStringAlloc(&pv).ok()?)
}

pub unsafe fn device_id(dev: &IMMDevice) -> Option<String> {
    take_string(dev.GetId().ok()?)
}

/// All active playback devices with their names.
/// The caller must have initialized COM on this thread.
unsafe fn render_devices() -> Result<Vec<(String, IMMDevice)>> {
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

/// Active playback device matching `pattern`, chosen as by [`pick`].
/// The caller must have initialized COM on this thread.
pub unsafe fn find_render_device(pattern: &str) -> Option<IMMDevice> {
    pick_from(render_devices().ok()?, pattern)
}

/// Calls its closure while it is alive. Dropping it stops the calls; it must be dropped on
/// the thread that created it.
pub struct DeviceWatch {
    enumerator: IMMDeviceEnumerator,
    client: IMMNotificationClient,
}

impl DeviceWatch {
    /// Call `on_change` whenever an audio device (playback or recording) appears, disappears
    /// or changes state. It runs on a Windows thread and must return quickly.
    /// The caller must have initialized COM on this thread.
    pub unsafe fn new(on_change: impl Fn() + Send + Sync + 'static) -> Result<DeviceWatch> {
        let enumerator = enumerator()?;
        let client: IMMNotificationClient = Client(Box::new(on_change)).into();
        enumerator.RegisterEndpointNotificationCallback(&client)?;
        Ok(DeviceWatch { enumerator, client })
    }
}

impl Drop for DeviceWatch {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .enumerator
                .UnregisterEndpointNotificationCallback(&self.client);
        }
    }
}

#[implement(IMMNotificationClient)]
struct Client(Box<dyn Fn() + Send + Sync>);

impl Client {
    fn changed(&self, change: &str) -> Result<()> {
        debug!(change, "audio device changed");
        (self.0)();
        Ok(())
    }
}

impl IMMNotificationClient_Impl for Client_Impl {
    fn OnDeviceStateChanged(&self, _: &PCWSTR, _: DEVICE_STATE) -> Result<()> {
        self.changed("state")
    }

    fn OnDeviceAdded(&self, _: &PCWSTR) -> Result<()> {
        self.changed("added")
    }

    fn OnDeviceRemoved(&self, _: &PCWSTR) -> Result<()> {
        self.changed("removed")
    }

    // Which device is the default doesn't matter: devices are chosen by name
    fn OnDefaultDeviceChanged(&self, _: EDataFlow, _: ERole, _: &PCWSTR) -> Result<()> {
        Ok(())
    }

    // Arrives often (e.g. on format changes) and never makes a device appear or disappear
    fn OnPropertyValueChanged(&self, _: &PCWSTR, _: &PROPERTYKEY) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_prefers_exact_match() {
        let n = ["Speaker-Left 2", "speaker-left", "CABLE Input"];
        assert_eq!(pick(n, "Speaker-Left"), Some(1));
        assert_eq!(pick(n, "cable"), Some(2));
        assert_eq!(pick(n, "nothing"), None);
        assert_eq!(pick(n, ""), None);
    }
}
