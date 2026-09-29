//! Reads the Windows volume and mute state of a playback device (VB-CABLE's "CABLE Input"
//! by default) and converts it to a linear gain for the audio threads.
//!
//! VB-CABLE is a pass-through device: the Windows volume does not change its signal level.
//! So this program reads that volume and applies it itself, which lets the keyboard volume
//! keys control both speakers.

use crate::default_device::find_render_device;
use crate::log;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
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

unsafe fn find_endpoint(pattern: &str) -> windows::core::Result<Option<IAudioEndpointVolume>> {
    match find_render_device(pattern)? {
        Some(dev) => Ok(Some(dev.Activate(CLSCTX_ALL, None)?)),
        None => Ok(None),
    }
}

/// Background thread: reads the volume every 30 ms and writes it to `gain`.
/// When `enabled` is false the gain is fixed at 1. The thread exits once `stop` is set.
pub fn spawn_watcher(pattern: String, gain: Gain, stop: Arc<AtomicBool>) {
    std::thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let mut endpoint: Option<IAudioEndpointVolume> = None;
        let mut warned = false;

        while !stop.load(Ordering::Relaxed) {
            if endpoint.is_none() {
                match find_endpoint(&pattern) {
                    Ok(Some(ep)) => {
                        log(&format!("Volume follow: found device \"{pattern}\""));
                        endpoint = Some(ep);
                        warned = false;
                    }
                    _ => {
                        if !warned {
                            log(&format!(
                                "Volume follow: playback device \"{pattern}\" not found, using 100% for now"
                            ));
                            warned = true;
                        }
                        gain.set(1.0);
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                }
            }

            let ep = endpoint.as_ref().unwrap();
            let muted = ep.GetMute().map(|b| b.as_bool());
            let scalar = ep.GetMasterVolumeLevelScalar();
            let db = ep.GetMasterVolumeLevel();
            match (muted, scalar, db) {
                (Ok(m), Ok(s), Ok(db)) => {
                    // Use the Windows dB curve so it feels the same as a regular speaker
                    let g = if m || s <= 0.0 {
                        0.0
                    } else {
                        10f32.powf(db / 20.0)
                    };
                    gain.set(g);
                }
                _ => {
                    // The device may have been unplugged; look it up again next round
                    endpoint = None;
                }
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    });
}
