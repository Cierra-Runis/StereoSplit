//! Audio engine: captures stereo from VB-CABLE and sends the left channel to the left speaker
//! (on all of its channels) and the right channel to the right speaker.
//!
//! Each USB speaker runs on its own clock, so the two slowly drift apart over time.
//! Each speaker gets a ring buffer whose fill level is held near a target: drop a little
//! when too much builds up, refill when it runs dry, so left and right stay aligned
//! instead of drifting further and further apart.

use crate::config::Config;
use crate::volume::Gain;
use anyhow::{anyhow, bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, Stream, StreamConfig};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub struct Engine {
    _input: Stream,
    _left: Stream,
    _right: Stream,
}

fn matches(name: &str, pat: &str) -> bool {
    name.to_lowercase().contains(&pat.to_lowercase())
}

/// Sound source. A matching playback device is preferred and read via loopback capture,
/// so no recording device is opened and Windows does not show "microphone in use".
/// Only if no playback device matches does it fall back to a recording device
/// (the older "CABLE Output" config still works).
enum Source {
    Loopback(cpal::Device),
    Recording(cpal::Device),
}

fn find_source(host: &cpal::Host, pat: &str) -> Result<Source> {
    for d in host.output_devices()? {
        if let Ok(n) = d.name() {
            if matches(&n, pat) {
                return Ok(Source::Loopback(d));
            }
        }
    }
    for d in host.input_devices()? {
        if let Ok(n) = d.name() {
            if matches(&n, pat) {
                return Ok(Source::Recording(d));
            }
        }
    }
    bail!(
        "Sound source \"{pat}\" not found. Make sure VB-CABLE is installed, \
         and check the device names in devices.txt."
    )
}

fn find_output(host: &cpal::Host, pat: &str) -> Result<cpal::Device> {
    for d in host.output_devices()? {
        if let Ok(n) = d.name() {
            if matches(&n, pat) {
                return Ok(d);
            }
        }
    }
    bail!(
        "Playback device \"{pat}\" not found. Check the device names in devices.txt, \
         and make sure the speaker is connected via USB."
    )
}

/// Write all current device names to devices.txt to help fill in the config
pub fn dump_devices(path: &std::path::Path) {
    let host = cpal::default_host();
    let mut s = String::from(
        "===== Playback devices (pick left / right / source / volume_endpoint from here) =====\n",
    );
    if let Ok(devs) = host.output_devices() {
        for d in devs {
            if let Ok(n) = d.name() {
                s.push_str(&format!("{n}\n"));
            }
        }
    }
    s.push_str("\n===== Recording devices (usually not needed) =====\n");
    if let Ok(devs) = host.input_devices() {
        for d in devs {
            if let Ok(n) = d.name() {
                s.push_str(&format!("{n}\n"));
            }
        }
    }
    let _ = std::fs::write(path, s);
}

/// Output stream for one speaker
fn build_output(
    dev: &cpal::Device,
    label: &str,
    sample_rate: u32,
    mut cons: HeapCons<f32>,
    target: usize,
    gain: Gain,
    failed: Arc<AtomicBool>,
) -> Result<Stream> {
    let name = dev.name().unwrap_or_default();
    let sup = dev
        .default_output_config()
        .with_context(|| format!("Failed to read the output format of \"{name}\""))?;
    if sup.sample_format() != SampleFormat::F32 {
        bail!("\"{name}\" does not use 32-bit float samples, which is not supported yet");
    }
    if sup.sample_rate().0 != sample_rate {
        bail!(
            "Sample rate mismatch: the sound source is {sample_rate} Hz, \
             but the {label} \"{name}\" is {} Hz.\n\
             In \"Control Panel > Sound > Playback\", set \"Properties > Advanced > Default Format\" \
             of both speakers and CABLE Input to the same sample rate (48000 Hz recommended).",
            sup.sample_rate().0
        );
    }
    let channels = sup.channels() as usize;
    let mut cfg: StreamConfig = sup.config();
    cfg.buffer_size = BufferSize::Default;

    let mut primed = false;
    let mut smooth = gain.get();
    let mut avg = target as f32; // Smoothed fill level, to cancel measurement jitter from input chunking
    let mut last = 0.0f32;
    let band = (sample_rate / 1000) as f32; // Allow about 1 ms of deviation from the target

    let stream = dev.build_output_stream(
        &cfg,
        move |data: &mut [f32], _| {
            let tgt = gain.get();
            let avail = cons.occupied_len();

            // Severe backlog (e.g. after a system stall) -> drop straight down to the target
            if avail > target * 3 {
                cons.skip(avail - target);
                avg = target as f32;
            }
            // On startup or after running dry, fill up to the target before playing,
            // so left and right start from the same point
            if !primed {
                if cons.occupied_len() >= target {
                    primed = true;
                    avg = cons.occupied_len() as f32;
                } else {
                    data.fill(0.0);
                    return;
                }
            }

            // Fine adjustment: add or drop at most 1 sample per callback. Inaudible, but
            // enough to cancel the clock drift between the two speakers
            avg += (cons.occupied_len() as f32 - avg) * 0.05;
            let mut repeat_once = false;
            if avg > target as f32 + band {
                cons.skip(1);
                avg -= 1.0;
            } else if avg < target as f32 - band {
                repeat_once = true;
                avg += 1.0;
            }

            for frame in data.chunks_mut(channels) {
                // Ramp the gain smoothly to avoid clicks when the volume changes
                smooth += (tgt - smooth) * 0.002;
                let v = if repeat_once {
                    repeat_once = false;
                    last
                } else {
                    match cons.try_pop() {
                        Some(v) => v,
                        None => {
                            primed = false;
                            0.0
                        }
                    }
                };
                last = v;
                let s = v * smooth;
                // Single-driver speaker: write the same channel to every output channel
                for c in frame.iter_mut() {
                    *c = s;
                }
            }
        },
        move |e| {
            crate::log(&format!("Output stream error: {e}"));
            failed.store(true, Ordering::Relaxed);
        },
        None,
    )?;
    stream.play()?;
    Ok(stream)
}

impl Engine {
    pub fn start(cfg: &Config, gain: Gain, failed: Arc<AtomicBool>) -> Result<Engine> {
        let host = cpal::default_host();

        let source = find_source(&host, &cfg.source)?;
        let left_dev = find_output(&host, &cfg.left)?;
        let right_dev = find_output(&host, &cfg.right)?;

        // Loopback capture uses the playback device's own format; a recording device uses its input format
        let (in_dev, sup, mode) = match source {
            Source::Loopback(d) => {
                let sup = d.default_output_config();
                (d, sup, "loopback capture")
            }
            Source::Recording(d) => {
                let sup = d.default_input_config();
                (d, sup, "recording device")
            }
        };
        let in_name = in_dev.name().unwrap_or_default();
        let sup = sup.with_context(|| format!("Failed to read the format of \"{in_name}\""))?;
        if sup.sample_format() != SampleFormat::F32 {
            bail!("\"{in_name}\" does not use 32-bit float samples, which is not supported yet");
        }
        let sample_rate = sup.sample_rate().0;
        let in_ch = sup.channels() as usize;
        let mut in_cfg: StreamConfig = sup.config();
        in_cfg.buffer_size = BufferSize::Default;

        let target = (sample_rate as usize * cfg.latency_ms.max(5) as usize) / 1000;
        let cap = sample_rate as usize; // 1 second of capacity, enough to absorb any jitter

        let (mut lp, lc): (HeapProd<f32>, HeapCons<f32>) = HeapRb::<f32>::new(cap).split();
        let (mut rp, rc): (HeapProd<f32>, HeapCons<f32>) = HeapRb::<f32>::new(cap).split();

        let f_in = failed.clone();
        let input = in_dev.build_input_stream(
            &in_cfg,
            move |data: &[f32], _| {
                for frame in data.chunks(in_ch) {
                    let l = frame[0];
                    let r = if in_ch > 1 { frame[1] } else { l };
                    // Drop samples when the buffer is full (the output side catches up on its own)
                    let _ = lp.try_push(l);
                    let _ = rp.try_push(r);
                }
            },
            move |e| {
                crate::log(&format!("Input stream error: {e}"));
                f_in.store(true, Ordering::Relaxed);
            },
            None,
        )?;

        let left = build_output(
            &left_dev,
            "left speaker",
            sample_rate,
            lc,
            target,
            gain.clone(),
            failed.clone(),
        )?;
        let right = build_output(
            &right_dev,
            "right speaker",
            sample_rate,
            rc,
            target,
            gain,
            failed,
        )?;
        input
            .play()
            .map_err(|e| anyhow!("Failed to start capturing sound: {e}"))?;

        crate::log(&format!(
            "Started ({mode}): {in_name} -> left \"{}\" / right \"{}\", {sample_rate} Hz, buffer {} ms",
            left_dev.name().unwrap_or_default(),
            right_dev.name().unwrap_or_default(),
            cfg.latency_ms
        ));

        Ok(Engine {
            _input: input,
            _left: left,
            _right: right,
        })
    }
}
