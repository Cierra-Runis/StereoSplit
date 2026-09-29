//! Reads the Windows volume and mute state of a playback device (VB-CABLE's "CABLE Input"
//! by default) and converts it to a linear gain for the audio threads.
//!
//! VB-CABLE is a pass-through device: the Windows volume does not change its signal level.
//! So this program reads that volume and applies it itself, which lets the keyboard volume
//! keys control both speakers.

use crate::devices::{device_id, find_render_device, DeviceWatch};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use tracing::{debug, info, warn};

use windows::core::implement;
use windows::Win32::Media::Audio::Endpoints::{
    IAudioEndpointVolume, IAudioEndpointVolumeCallback, IAudioEndpointVolumeCallback_Impl,
};
use windows::Win32::Media::Audio::{IMMDevice, AUDIO_VOLUME_NOTIFICATION_DATA};
use windows::Win32::System::Com::{CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};

/// Shared gain value (the f32 bit pattern is stored in an AtomicU32)
#[derive(Clone)]
pub struct Gain(Arc<AtomicU32>);

impl Gain {
    pub fn new(v: f32) -> Self {
        Gain(Arc::new(AtomicU32::new(v.to_bits())))
    }
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
    pub fn set(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed)
    }
}

/// What the follower thread waits for
enum Msg {
    /// The volume or mute state of the followed device changed
    Volume,
    /// A device appeared, disappeared or changed state
    Devices,
    Stop,
}

/// Keeps a [`Gain`] at the Windows volume of the playback device matching `pattern`, for as
/// long as it is alive. Windows reports every change, so nothing is polled.
pub struct Follower {
    pattern: String,
    gain: Gain,
    tx: Sender<Msg>,
}

impl Follower {
    pub fn start(pattern: String) -> Follower {
        let gain = Gain::new(1.0);
        let (tx, rx) = mpsc::channel();
        {
            let (pattern, gain, tx) = (pattern.clone(), gain.clone(), tx.clone());
            std::thread::spawn(move || unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                let devices_tx = tx.clone();
                let _devices = DeviceWatch::new(move || {
                    let _ = devices_tx.send(Msg::Devices);
                })
                .inspect_err(|e| {
                    warn!(
                        error = %e,
                        "volume follow: can't watch for device changes; if the device goes \
                         away, the volume stops following it until a restart"
                    )
                })
                .ok();
                run(&pattern, &gain, &tx, &rx);
            });
        }
        Follower { pattern, gain, tx }
    }

    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    pub fn gain(&self) -> Gain {
        self.gain.clone()
    }
}

impl Drop for Follower {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
    }
}

/// A device whose volume changes are being reported
struct Followed {
    id: String,
    endpoint: IAudioEndpointVolume,
    callback: IAudioEndpointVolumeCallback,
}

impl Drop for Followed {
    fn drop(&mut self) {
        unsafe {
            let _ = self.endpoint.UnregisterControlChangeNotify(&self.callback);
        }
    }
}

#[implement(IAudioEndpointVolumeCallback)]
struct Callback(Sender<Msg>);

impl IAudioEndpointVolumeCallback_Impl for Callback_Impl {
    fn OnNotify(&self, _: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> windows::core::Result<()> {
        // Read on the follower thread: the notification has the slider position but not the
        // dB level that is applied
        let _ = self.0.send(Msg::Volume);
        Ok(())
    }
}

/// Follower thread body: look the device up at the start and after every device change,
/// and read the volume after every volume change
unsafe fn run(pattern: &str, gain: &Gain, tx: &Sender<Msg>, rx: &Receiver<Msg>) {
    let mut followed = start(pattern, lookup(pattern), tx);
    loop {
        match followed.as_ref().map(|f| read_gain(&f.endpoint)) {
            Some(Ok(g)) => gain.set(g),
            Some(Err(e)) => {
                // Probably being unplugged; the device change that follows looks it up again
                debug!(error = %e, "volume follow: reading the volume failed");
                followed = None;
            }
            None => gain.set(1.0),
        }

        match rx.recv() {
            Ok(Msg::Volume) => {}
            Ok(Msg::Devices) => {
                let found = lookup(pattern);
                // Only a different device (or none) is news; most device changes are elsewhere
                let found_id = found.as_ref().map(|(_, id)| id.as_str());
                if found_id != followed.as_ref().map(|f| f.id.as_str()) {
                    // Unregister the old one before registering again
                    drop(followed.take());
                    followed = start(pattern, found, tx);
                }
            }
            Ok(Msg::Stop) | Err(_) => break,
        }
    }
}

/// The playback device matching `pattern`, with its id
unsafe fn lookup(pattern: &str) -> Option<(IMMDevice, String)> {
    let dev = find_render_device(pattern)?;
    let id = device_id(&dev)?;
    Some((dev, id))
}

/// Follow `found` (from [`lookup`]), logging how that went
unsafe fn start(
    pattern: &str,
    found: Option<(IMMDevice, String)>,
    tx: &Sender<Msg>,
) -> Option<Followed> {
    let Some((dev, id)) = found else {
        warn!(
            device = %pattern,
            "volume follow: playback device not found, using 100% for now"
        );
        return None;
    };
    follow(&dev, id, tx)
        .inspect(|_| info!(device = %pattern, "volume follow: device found"))
        .inspect_err(|e| {
            warn!(
                device = %pattern,
                error = %e,
                "volume follow: can't read the device volume, using 100% for now"
            )
        })
        .ok()
}

unsafe fn follow(dev: &IMMDevice, id: String, tx: &Sender<Msg>) -> windows::core::Result<Followed> {
    let endpoint: IAudioEndpointVolume = dev.Activate(CLSCTX_ALL, None)?;
    let callback: IAudioEndpointVolumeCallback = Callback(tx.clone()).into();
    endpoint.RegisterControlChangeNotify(&callback)?;
    Ok(Followed {
        id,
        endpoint,
        callback,
    })
}

unsafe fn read_gain(ep: &IAudioEndpointVolume) -> windows::core::Result<f32> {
    let muted = ep.GetMute()?.as_bool();
    let scalar = ep.GetMasterVolumeLevelScalar()?;
    let db = ep.GetMasterVolumeLevel()?;
    // Use the Windows dB curve so it feels the same as a regular speaker
    Ok(if muted || scalar <= 0.0 {
        0.0
    } else {
        10f32.powf(db / 20.0)
    })
}
