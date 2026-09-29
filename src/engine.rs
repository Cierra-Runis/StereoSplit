//! Audio engine: captures stereo from VB-CABLE and sends the left channel to the left speaker
//! (on all of its channels) and the right channel to the right speaker.
//!
//! Each speaker gets a ring buffer holding the source's samples, and a resampler that
//! converts them to the speaker's own sample rate, so the devices don't have to match.
//!
//! Each USB speaker also runs on its own clock, so the two slowly drift apart over time.
//! The fill level of each ring buffer is held near a target by nudging that speaker's
//! resampling ratio up or down by a tiny amount, so left and right stay aligned instead of
//! drifting further and further apart.

use crate::config::Config;
use crate::volume::Gain;
use anyhow::{anyhow, bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, Stream, StreamConfig};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Adjustable, Async, FixedAsync, Resampler, SincInterpolationParameters, WindowFunction,
};
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info};

/// Source frames the resampler consumes per step
const CHUNK_IN: usize = 128;

/// Called from an audio thread when a stream fails (e.g. a speaker was unplugged)
pub type OnError = Arc<dyn Fn() + Send + Sync>;

pub struct Engine {
    _input: Stream,
    _left: Stream,
    _right: Stream,
}

/// Index of the device matching `pat`: an exact (case-insensitive) name match wins,
/// otherwise the first name containing it. An empty pattern matches nothing.
pub fn pick(names: &[String], pat: &str) -> Option<usize> {
    if pat.is_empty() {
        return None;
    }
    let pat = pat.to_lowercase();
    let lower: Vec<String> = names.iter().map(|n| n.to_lowercase()).collect();
    lower
        .iter()
        .position(|n| *n == pat)
        .or_else(|| lower.iter().position(|n| n.contains(&pat)))
}

fn find_in(devs: impl Iterator<Item = cpal::Device>, pat: &str) -> Option<cpal::Device> {
    let mut devs: Vec<(String, cpal::Device)> =
        devs.filter_map(|d| d.name().ok().map(|n| (n, d))).collect();
    let names: Vec<String> = devs.iter().map(|(n, _)| n.clone()).collect();
    pick(&names, pat).map(|i| devs.swap_remove(i).1)
}

/// Names of all playback devices
#[tracing::instrument(level = "debug")]
pub fn output_device_names() -> Vec<String> {
    cpal::default_host()
        .output_devices()
        .map(|devs| devs.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default()
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
    if let Some(d) = find_in(host.output_devices()?, pat) {
        return Ok(Source::Loopback(d));
    }
    if let Some(d) = find_in(host.input_devices()?, pat) {
        return Ok(Source::Recording(d));
    }
    bail!("Sound source \"{pat}\" not found. Make sure VB-CABLE is installed.")
}

fn find_output(host: &cpal::Host, pat: &str) -> Result<cpal::Device> {
    find_in(host.output_devices()?, pat).ok_or_else(|| {
        anyhow!(
            "Playback device \"{pat}\" not found. Make sure the speaker is connected via USB, \
             or choose another one in the tray menu."
        )
    })
}

/// Holds one speaker's ring buffer level near the target by returning a resampling ratio
/// slightly above or below 1 (proportional + integral control). The integral term settles
/// on the speaker's clock offset, so the level ends up right at the target, keeping left
/// and right aligned.
struct DriftControl {
    target: f32,
    /// Smoothed fill level, to cancel measurement jitter from input chunking
    avg: f32,
    integral: f64,
}

impl DriftControl {
    const KP: f64 = 0.02;
    const KI: f64 = 0.00005;
    /// Never change the speed by more than 0.5% (real clock offsets are ~0.01%)
    const LIMIT: f64 = 0.005;

    fn new(target: usize) -> Self {
        DriftControl {
            target: target as f32,
            avg: target as f32,
            integral: 0.0,
        }
    }

    /// Restart smoothing from `fill`. The integral is kept: the clock offset hasn't changed.
    fn reset(&mut self, fill: f32) {
        self.avg = fill;
    }

    /// Feed the current fill level (in source frames), get the relative ratio to use
    fn update(&mut self, fill: f32) -> f64 {
        self.avg += (fill - self.avg) * 0.05;
        let err = ((self.avg - self.target) / self.target) as f64;
        self.integral = (self.integral + Self::KI * err).clamp(-Self::LIMIT, Self::LIMIT);
        // Too full -> consume faster -> fewer output frames per input frame -> ratio below 1
        (1.0 - Self::KP * err - self.integral).clamp(1.0 - Self::LIMIT, 1.0 + Self::LIMIT)
    }
}

/// Pulls source samples out of a ring buffer and hands them out one at a time at the
/// speaker's sample rate. All buffers are allocated up front, so it is safe to use in the
/// audio callback.
struct Feeder {
    rs: Async<f32>,
    ratio: f64,
    input: Vec<f32>,
    output: Vec<f32>,
    pos: usize,
    len: usize,
}

impl Feeder {
    fn new(in_rate: u32, out_rate: u32) -> Result<Self> {
        let ratio = out_rate as f64 / in_rate as f64;
        let chunk_out = ((CHUNK_IN as f64 * ratio).round() as usize).max(16);
        let params = SincInterpolationParameters::new(128, WindowFunction::BlackmanHarris2);
        let rs = Async::<f32>::new_sinc(
            ratio,
            1.0 + DriftControl::LIMIT * 2.0,
            &params,
            chunk_out,
            1,
            FixedAsync::Output,
        )
        .map_err(|e| anyhow!("Failed to create the resampler: {e}"))?;
        Ok(Feeder {
            input: vec![0.0; rs.input_frames_max()],
            output: vec![0.0; rs.output_frames_max()],
            rs,
            ratio,
            pos: 0,
            len: 0,
        })
    }

    /// Output frames still waiting to be played, in source frames
    fn buffered(&self) -> f32 {
        ((self.len - self.pos) as f64 / self.ratio) as f32
    }

    fn reset(&mut self) {
        self.rs.reset();
        self.pos = 0;
        self.len = 0;
    }

    fn set_relative_ratio(&mut self, rel: f64) {
        let _ = self.rs.set_resample_ratio_relative(rel, true);
    }

    /// Next output sample, or None if the ring buffer has run dry
    fn next(&mut self, cons: &mut HeapCons<f32>) -> Option<f32> {
        if self.pos == self.len {
            let need = self.rs.input_frames_next();
            if cons.occupied_len() < need {
                return None;
            }
            cons.pop_slice(&mut self.input[..need]);
            let input = InterleavedSlice::new(&self.input[..need], 1, need).ok()?;
            let frames = self.output.len();
            let mut output = InterleavedSlice::new_mut(&mut self.output[..], 1, frames).ok()?;
            let (_, n) = self
                .rs
                .process_into_buffer(&input, &mut output, None)
                .ok()?;
            self.pos = 0;
            self.len = n;
        }
        let v = self.output.get(self.pos).copied()?;
        self.pos += 1;
        Some(v)
    }
}

/// Output stream for one speaker. `side` is "left" or "right".
fn build_output(
    dev: &cpal::Device,
    side: &'static str,
    in_rate: u32,
    mut cons: HeapCons<f32>,
    target: usize,
    gain: Gain,
    on_error: OnError,
) -> Result<Stream> {
    let name = dev.name().unwrap_or_default();
    let sup = dev
        .default_output_config()
        .with_context(|| format!("Failed to read the output format of \"{name}\""))?;
    if sup.sample_format() != SampleFormat::F32 {
        bail!("\"{name}\" does not use 32-bit float samples, which is not supported yet");
    }
    let out_rate = sup.sample_rate().0;
    let channels = sup.channels() as usize;
    let mut cfg: StreamConfig = sup.config();
    cfg.buffer_size = BufferSize::Default;

    let mut feeder = Feeder::new(in_rate, out_rate)?;
    let mut drift = DriftControl::new(target);
    let mut primed = false;
    let mut smooth = gain.get();
    info!(side, device = %name, in_rate, out_rate, "output resampling");

    let stream = dev.build_output_stream(
        &cfg,
        move |data: &mut [f32], _| {
            let tgt = gain.get();
            let avail = cons.occupied_len();

            // Severe backlog (e.g. after a system stall) -> drop straight down to the target
            if avail > target * 3 {
                cons.skip(avail - target);
                drift.reset(target as f32);
            }
            // On startup or after running dry, fill up to the target before playing,
            // so left and right start from the same point
            if !primed {
                if cons.occupied_len() >= target {
                    primed = true;
                    feeder.reset();
                    drift.reset(cons.occupied_len() as f32);
                } else {
                    data.fill(0.0);
                    return;
                }
            }

            let rel = drift.update(cons.occupied_len() as f32 + feeder.buffered());
            feeder.set_relative_ratio(rel);

            for frame in data.chunks_mut(channels) {
                // Ramp the gain smoothly to avoid clicks when the volume changes
                smooth += (tgt - smooth) * 0.002;
                let v = if primed { feeder.next(&mut cons) } else { None };
                let s = match v {
                    Some(v) => v * smooth,
                    None => {
                        primed = false;
                        0.0
                    }
                };
                // Single-driver speaker: write the same channel to every output channel
                frame.fill(s);
            }
        },
        move |e| {
            error!(side, error = %e, "output stream error");
            on_error();
        },
        None,
    )?;
    stream.play()?;
    Ok(stream)
}

/// Play a short beep on the named playback device, in the background. It mixes with
/// whatever the engine is playing there, so the engine keeps running.
pub fn play_test_tone(device: String) {
    std::thread::spawn(move || {
        if let Err(e) = test_tone(&device) {
            error!(device = %device, error = %format_args!("{e:#}"), "test tone failed");
        }
    });
}

fn test_tone(device: &str) -> Result<()> {
    const SECONDS: f32 = 0.8;
    const FADE: f32 = 0.02;
    const FREQ: f32 = 660.0;
    const LEVEL: f32 = 0.1;

    let host = cpal::default_host();
    let dev = find_output(&host, device)?;
    let sup = dev.default_output_config()?;
    if sup.sample_format() != SampleFormat::F32 {
        bail!("the device does not use 32-bit float samples");
    }
    let rate = sup.sample_rate().0 as f32;
    let channels = sup.channels() as usize;
    let mut n = 0u32;
    let stream = dev.build_output_stream(
        &sup.config(),
        move |data: &mut [f32], _| {
            for frame in data.chunks_mut(channels) {
                let t = n as f32 / rate;
                let env = (t / FADE).min((SECONDS - t) / FADE).clamp(0.0, 1.0);
                let s = LEVEL * env * (std::f32::consts::TAU * FREQ * t).sin();
                frame.fill(s);
                n = n.saturating_add(1);
            }
        },
        |e| error!(error = %e, "test tone stream error"),
        None,
    )?;
    stream.play()?;
    std::thread::sleep(Duration::from_secs_f32(SECONDS + 0.2));
    Ok(())
}

impl Engine {
    #[tracing::instrument(level = "info", skip_all)]
    pub fn start(cfg: &Config, gain: Gain, on_error: OnError) -> Result<Engine> {
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

        // At least two resampler steps must fit in the buffer, or it would never start playing
        let target =
            ((sample_rate as usize * cfg.latency_ms.max(5) as usize) / 1000).max(CHUNK_IN * 2);
        let cap = sample_rate as usize; // 1 second of capacity, enough to absorb any jitter

        let (mut lp, lc): (HeapProd<f32>, HeapCons<f32>) = HeapRb::<f32>::new(cap).split();
        let (mut rp, rc): (HeapProd<f32>, HeapCons<f32>) = HeapRb::<f32>::new(cap).split();

        let on_input_error = on_error.clone();
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
                error!(error = %e, "input stream error");
                on_input_error();
            },
            None,
        )?;

        let left = build_output(
            &left_dev,
            "left",
            sample_rate,
            lc,
            target,
            gain.clone(),
            on_error.clone(),
        )?;
        let right = build_output(
            &right_dev,
            "right",
            sample_rate,
            rc,
            target,
            gain,
            on_error,
        )?;
        input
            .play()
            .map_err(|e| anyhow!("Failed to start capturing sound: {e}"))?;

        info!(
            mode,
            input = %in_name,
            left = %left_dev.name().unwrap_or_default(),
            right = %right_dev.name().unwrap_or_default(),
            sample_rate,
            latency_ms = cfg.latency_ms,
            "engine started"
        );

        Ok(Engine {
            _input: input,
            _left: left,
            _right: right,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn pick_prefers_exact_match() {
        let n = names(&["Speaker-Left 2", "speaker-left", "CABLE Input"]);
        assert_eq!(pick(&n, "Speaker-Left"), Some(1));
        assert_eq!(pick(&n, "cable"), Some(2));
        assert_eq!(pick(&n, "nothing"), None);
        assert_eq!(pick(&n, ""), None);
    }

    /// Resampling 48 kHz to 44.1 kHz yields 44.1/48 as many samples
    #[test]
    fn feeder_converts_rate() {
        let (mut p, mut c): (HeapProd<f32>, HeapCons<f32>) = HeapRb::<f32>::new(48_000).split();
        for i in 0..48_000 {
            let _ = p.try_push((i as f32 * 0.01).sin());
        }
        let mut f = Feeder::new(48_000, 44_100).unwrap();
        let mut out = 0usize;
        while f.next(&mut c).is_some() {
            out += 1;
        }
        let consumed = 48_000 - c.occupied_len();
        let expected = consumed as f64 * 44_100.0 / 48_000.0;
        assert!(
            (out as f64 - expected).abs() < 200.0,
            "{out} samples out for {consumed} in, expected about {expected}"
        );
    }

    /// With the speaker clock 200 ppm off, the buffer level still settles at the target
    #[test]
    fn drift_control_settles_on_target() {
        let target = 960.0f64;
        let per_callback = 480.0f64;
        let clock_offset = 1.0002;
        let mut ctl = DriftControl::new(target as usize);
        let mut fill = target;
        for _ in 0..20_000 {
            let rel = ctl.update(fill as f32);
            fill += per_callback * clock_offset - per_callback / rel;
        }
        assert!((fill - target).abs() < 2.0, "fill settled at {fill}");
    }
}
