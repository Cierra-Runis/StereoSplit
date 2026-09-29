//! Tells when audio devices are plugged in, unplugged, enabled or disabled, so waiting for a
//! device to come back needs no polling.

use windows::core::{implement, PCWSTR};
use windows::Win32::Media::Audio::{
    EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient, IMMNotificationClient_Impl,
    MMDeviceEnumerator, DEVICE_STATE,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;

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
    pub unsafe fn new(
        on_change: impl Fn() + Send + Sync + 'static,
    ) -> windows::core::Result<DeviceWatch> {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
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

impl IMMNotificationClient_Impl for Client_Impl {
    fn OnDeviceStateChanged(&self, _: &PCWSTR, _: DEVICE_STATE) -> windows::core::Result<()> {
        tracing::debug!("audio device state changed");
        (self.0)();
        Ok(())
    }

    fn OnDeviceAdded(&self, _: &PCWSTR) -> windows::core::Result<()> {
        tracing::debug!("audio device added");
        (self.0)();
        Ok(())
    }

    fn OnDeviceRemoved(&self, _: &PCWSTR) -> windows::core::Result<()> {
        tracing::debug!("audio device removed");
        (self.0)();
        Ok(())
    }

    // Which device is the default doesn't matter: devices are chosen by name
    fn OnDefaultDeviceChanged(
        &self,
        _: EDataFlow,
        _: ERole,
        _: &PCWSTR,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    // Arrives often (e.g. on format changes) and never makes a device appear or disappear
    fn OnPropertyValueChanged(&self, _: &PCWSTR, _: &PROPERTYKEY) -> windows::core::Result<()> {
        Ok(())
    }
}
