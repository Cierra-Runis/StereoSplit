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
use crate::devices;
use crate::latency::{self, AutoLatency};
use crate::meter::{Meter, Reporter, Stats};
use crate::volume::Gain;
use anyhow::{anyhow, bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{ErrorKind, SampleFormat, Stream, StreamConfig};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Adjustable, Async, FixedAsync, Resampler, SincInterpolationParameters, WindowFunction,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

/// Source frames the resampler consumes per step
/// (a speaker needs this much buffered on top of what it plays in one callback, so it is kept
/// small)
const CHUNK_IN: usize = 32;

/// One of the engine's three streams
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Input,
    Left,
    Right,
}

/// Lowercase, as it appears in the log
impl std::fmt::Display for StreamKind {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(match self {
            StreamKind::Input => "input",
            StreamKind::Left => "left",
            StreamKind::Right => "right",
        })
    }
}

/// Called from an audio thread when a stream fails (e.g. a speaker was unplugged), with the
/// stream that failed
pub type OnError = Arc<dyn Fn(StreamKind) + Send + Sync>;

pub struct Engine {
    streams: Vec<Stream>,
    /// Logs how the streams are doing, until the engine is dropped
    _reporter: Reporter,
}

/// Dropping the engine stops it without waiting for it: dropping a stream joins cpal's stream
/// thread, which could hang forever on a wedged driver (e.g. a USB speaker pulled out
/// mid-call), and the caller must be able to go on reconnecting regardless. So `on_error` may
/// still be called for a while after the engine is dropped.
impl Drop for Engine {
    fn drop(&mut self) {
        let streams = std::mem::take(&mut self.streams);
        // If the thread can't be started, the streams are dropped right here instead
        let _ = std::thread::Builder::new()
            .name("engine-stop".into())
            .spawn(move || {
                let t = Instant::now();
                drop(streams);
                let elapsed_ms = t.elapsed().as_millis() as u64;
                if elapsed_ms > 1000 {
                    warn!(elapsed_ms, "engine was slow to stop");
                }
            });
    }
}

/// Error callback for the stream `meter` counts: logs the error, and reports it through
/// `on_error` unless the stream keeps running anyway.
fn on_stream_error(meter: Arc<Meter>, on_error: OnError) -> impl FnMut(cpal::Error) + Send {
    let stream = meter.kind();
    move |e| match e.kind() {
        // Some audio was dropped (e.g. after a brief system stall), but the stream goes on
        ErrorKind::Xrun => meter.xrun(),
        ErrorKind::DeviceNotAvailable => {
            warn!(%stream, "device disconnected");
            on_error(stream);
        }
        _ => {
            error!(%stream, error = %e, "stream error");
            on_error(stream);
        }
    }
}

/// The device's name, as Windows shows it (its FriendlyName, the same name
/// `default_device` reads). `Device`'s `Display` is not used: it fails when the name can't be
/// read, and `to_string()` panics on that.
fn device_name(dev: &cpal::Device) -> Option<String> {
    dev.description().ok().map(|d| d.name().to_owned())
}

/// The playback device (or recording device, if `output` is false) matching `pat`, chosen as
/// by [`devices::pick`].
///
/// cpal's `output_devices()` / `input_devices()` are not used: they probe the supported
/// formats of every device, which takes ~230 ms. Instead all devices are listed by name,
/// and only the ones whose name could match are checked for their direction (which fails
/// right away for a device of the other direction).
fn find_in(host: &cpal::Host, pat: &str, output: bool) -> Result<Option<cpal::Device>> {
    if pat.is_empty() {
        return Ok(None);
    }
    let devs: Vec<(String, cpal::Device)> = host
        .devices()?
        .filter_map(|d| device_name(&d).map(|n| (n, d)))
        // pick() only ever returns a name containing the pattern, so this changes nothing
        .filter(|(n, _)| devices::matches(n, pat))
        .filter(|(_, d)| {
            if output {
                d.default_output_config().is_ok()
            } else {
                d.default_input_config().is_ok()
            }
        })
        .collect();
    Ok(devices::pick_from(devs, pat))
}

/// The default format of `dev` for playback (or for recording, if `output` is false).
/// Only 32-bit float samples are supported.
fn f32_config(dev: &cpal::Device, output: bool) -> Result<StreamConfig> {
    let name = || device_name(dev).unwrap_or_default();
    let sup = if output {
        dev.default_output_config()
    } else {
        dev.default_input_config()
    }
    .with_context(|| format!("Failed to read the format of \"{}\"", name()))?;
    if sup.sample_format() != SampleFormat::F32 {
        bail!(
            "\"{}\" does not use 32-bit float samples, which is not supported yet",
            name()
        );
    }
    Ok(sup.config())
}

/// Sound source, its format and how it is read. A matching playback device is preferred and
/// read via loopback capture (in its playback format), so no recording device is opened and
/// Windows does not show "microphone in use". Only if no playback device matches does it
/// fall back to a recording device (the older "CABLE Output" config still works).
fn find_source(host: &cpal::Host, pat: &str) -> Result<(cpal::Device, StreamConfig, &'static str)> {
    for (output, mode) in [(true, "loopback capture"), (false, "recording device")] {
        if let Some(d) = find_in(host, pat, output)? {
            let config = f32_config(&d, output)?;
            return Ok((d, config, mode));
        }
    }
    bail!("Sound source \"{pat}\" not found. Make sure VB-CABLE is installed.")
}

fn find_output(host: &cpal::Host, pat: &str) -> Result<cpal::Device> {
    find_in(host, pat, true)?.ok_or_else(|| {
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
    const KP: f64 = 0.01;
    const KI: f64 = 0.00002;
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

    /// Hold the level near `target` from now on. The speed stays within its limit, so the level
    /// gets there gradually (at 0.5%, 10 ms takes 2 s).
    fn set_target(&mut self, target: usize) {
        self.target = target as f32;
    }

    /// Feed the current fill level (in source frames), get the relative ratio to use
    fn update(&mut self, fill: f32) -> f64 {
        self.avg += (fill - self.avg) * 0.02;
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

/// The latency both speakers keep, as source frames buffered. Shared, so left and right stay
/// aligned when it changes.
type Target = Arc<AtomicUsize>;

/// Most time since the last input packet that [`input_level`] counts, in case input stops
const INPUT_WAIT_MAX: Duration = Duration::from_millis(40);

/// The buffer level to steer by, in source frames: what is buffered (`occupied` in the ring,
/// `buffered` in the resampler), plus the input captured since the last packet arrived, which
/// comes with the next one. That is how long the sound takes to get through, whenever a
/// speaker looks.
///
/// Input arrives in packets (10 ms for loopback capture), so the plain level jumps by a packet
/// each time one arrives, and by two when two come at once. Steering by the plain level, the
/// drift control chased those jumps (wobbling the speed by ±1000 ppm every second when packets
/// came in pairs), and a speaker looking just before the packets arrive kept a different delay
/// from one looking just after, pulling left and right apart by several ms.
fn input_level(occupied: usize, buffered: f32, since_input: Duration, in_rate: u32) -> f32 {
    occupied as f32 + buffered + since_input.min(INPUT_WAIT_MAX).as_secs_f32() * in_rate as f32
}

/// Output stream for the speaker of `meter`'s side, playing input from the stream `input`
/// meters
#[allow(clippy::too_many_arguments)]
fn build_output(
    dev: &cpal::Device,
    in_rate: u32,
    mut cons: HeapCons<f32>,
    shared_target: Target,
    gain: Gain,
    meter: Arc<Meter>,
    input: Arc<Meter>,
    on_error: OnError,
) -> Result<Stream> {
    let config = f32_config(dev, true)?;
    let out_rate = config.sample_rate;
    let channels = config.channels as usize;
    let mut feeder = Feeder::new(in_rate, out_rate)?;
    let mut target = shared_target.load(Ordering::Relaxed);
    let mut drift = DriftControl::new(target);
    let mut primed = false;
    let mut smooth = gain.get();
    let side = meter.kind();
    let device = device_name(dev).unwrap_or_default();
    info!(%side, device, in_rate, out_rate, "output resampling");

    let errors = on_stream_error(meter.clone(), on_error);
    let stream = dev.build_output_stream(
        config,
        move |data: &mut [f32], _| {
            meter.callback(data.len() / channels, out_rate);
            let tgt = gain.get();
            let now = shared_target.load(Ordering::Relaxed);
            if now != target {
                target = now;
                drift.set_target(target);
            }
            let avail = cons.occupied_len();

            // Severe backlog (e.g. after a system stall) -> drop straight down to the target
            if avail > target * 3 {
                cons.skip(avail - target);
                drift.reset(target as f32);
                meter.skip();
            }
            // On startup or after running dry, fill up to the target before playing,
            // so left and right start from the same point
            let since_input = input.since_last();
            if !primed {
                if cons.occupied_len() >= target {
                    primed = true;
                    feeder.reset();
                    drift.reset(input_level(cons.occupied_len(), 0.0, since_input, in_rate));
                } else {
                    data.fill(0.0);
                    return;
                }
            }

            let fill = input_level(cons.occupied_len(), feeder.buffered(), since_input, in_rate);
            let rel = drift.update(fill);
            meter.drift(fill, in_rate, rel);
            feeder.set_relative_ratio(rel);

            for frame in data.chunks_mut(channels) {
                // Ramp the gain smoothly to avoid clicks when the volume changes
                smooth += (tgt - smooth) * 0.002;
                let v = if primed { feeder.next(&mut cons) } else { None };
                let s = match v {
                    Some(v) => v * smooth,
                    None => {
                        if primed {
                            meter.dry();
                        }
                        primed = false;
                        0.0
                    }
                };
                // Single-driver speaker: write the same channel to every output channel
                frame.fill(s);
            }
        },
        errors,
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
    let config = f32_config(&dev, true)?;
    let rate = config.sample_rate as f32;
    let channels = config.channels as usize;
    let mut n = 0u32;
    let stream = dev.build_output_stream(
        config,
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

        let (in_dev, in_cfg, mode) = find_source(&host, &cfg.source)?;
        let left_dev = find_output(&host, &cfg.left)?;
        let right_dev = find_output(&host, &cfg.right)?;
        let sample_rate = in_cfg.sample_rate;
        let in_ch = in_cfg.channels as usize;

        // At least two resampler steps must fit in the buffer, or it would never start playing
        let to_frames =
            move |us: u32| (sample_rate as usize * us as usize / 1_000_000).max(CHUNK_IN * 2);
        let auto = cfg.latency_ms == 0;
        let start_us = if auto {
            latency::START_US
        } else {
            cfg.latency_ms.max(5) * 1000
        };
        let target: Target = Arc::new(AtomicUsize::new(to_frames(start_us)));
        let cap = sample_rate as usize; // 1 second of capacity, enough to absorb any jitter

        let (mut lp, lc): (HeapProd<f32>, HeapCons<f32>) = HeapRb::<f32>::new(cap).split();
        let (mut rp, rc): (HeapProd<f32>, HeapCons<f32>) = HeapRb::<f32>::new(cap).split();

        let meters = [StreamKind::Input, StreamKind::Left, StreamKind::Right]
            .map(|kind| Arc::new(Meter::new(kind)));
        let [in_meter, left_meter, right_meter] = meters.clone();

        let errors = on_stream_error(in_meter.clone(), on_error.clone());
        let input = in_dev.build_input_stream(
            in_cfg,
            move |data: &[f32], _| {
                for frame in data.chunks(in_ch) {
                    let l = frame[0];
                    let r = if in_ch > 1 { frame[1] } else { l };
                    // Drop samples when the buffer is full (the output side catches up on its own)
                    let _ = lp.try_push(l);
                    let _ = rp.try_push(r);
                }
                // Only now, as the speakers take this as the time the input arrived
                in_meter.callback(data.len() / in_ch, sample_rate);
            },
            errors,
            None,
        )?;

        let left = build_output(
            &left_dev,
            sample_rate,
            lc,
            target.clone(),
            gain.clone(),
            left_meter,
            meters[0].clone(),
            on_error.clone(),
        )?;
        let right = build_output(
            &right_dev,
            sample_rate,
            rc,
            target.clone(),
            gain,
            right_meter,
            meters[0].clone(),
            on_error,
        )?;
        input
            .play()
            .map_err(|e| anyhow!("Failed to start capturing sound: {e}"))?;

        let latency_text = if auto {
            "auto".to_string()
        } else {
            format!("{} ms", cfg.latency_ms)
        };
        info!(
            mode,
            input = %device_name(&in_dev).unwrap_or_default(),
            left = %device_name(&left_dev).unwrap_or_default(),
            right = %device_name(&right_dev).unwrap_or_default(),
            sample_rate,
            latency = %latency_text,
            "engine started"
        );

        let chunk_us = (CHUNK_IN * 1_000_000 / sample_rate as usize) as u32;
        let mut control = auto.then(|| AutoLatency::new(Instant::now(), chunk_us));
        let on_window = move |stats: &[Stats]| {
            let Some(control) = &mut control else {
                return;
            };
            if let Some(us) = control.update(Instant::now(), stats) {
                target.store(to_frames(us), Ordering::Relaxed);
                info!(latency_ms = us as f32 / 1000.0, "latency set");
            }
        };
        Ok(Engine {
            streams: vec![input, left, right],
            _reporter: Reporter::start(meters.into(), on_window),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// How the input arrives and a speaker takes it, for [`simulate`]
    struct Timing {
        /// Every how many packets one comes a whole packet late, with the next; 0 for never
        pairs_every: u64,
        /// How much faster the speaker's clock runs; 100 ppm sweeps it through every phase
        /// against the input in 100 s, 0 keeps it at `phase_ms` after each packet
        clock_offset: f64,
        phase_ms: f64,
    }

    /// What [`simulate`] saw
    #[derive(Debug)]
    struct Run {
        /// Blocks that found less than they needed
        dry: u32,
        /// How much the speed varied in the second half (standard deviation), in ppm
        wobble_ppm: f64,
        /// How long after it was captured the sound was played in the second half, on
        /// average, in ms
        delay_ms: f64,
    }

    /// 200 s of one shared-mode speaker at `target_ms`: 10 ms input packets arriving up to
    /// 3 ms late (as in the logs), and 10 ms blocks
    fn simulate(target_ms: f64, timing: Timing) -> Run {
        let mut seed = 1u64;
        let mut random = move || {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (seed >> 11) as f64 / (1u64 << 53) as f64
        };
        let target = (target_ms * 48.0) as usize;
        let mut ctl = DriftControl::new(target);
        let mut level = target as f64;
        let (mut block, mut dry) = (0u64, 0);
        // Packet p (from 1) holds the frames captured until 10p ms, frame i at i / 48 ms. It
        // arrives up to 3 ms after that, or, if it is one to come late, together with the next.
        let (mut packet, mut count) = (1u64, 1u64);
        let mut next_packet = 10.0 + random() * 3.0;
        let (mut next_block, mut arrived) = (timing.phase_ms, 0.0f64);
        let (mut n, mut sum, mut sum2, mut delay) = (0.0, 0.0, 0.0, 0.0);
        // The buffer starts out holding frames captured before 0
        let mut played = -(target as f64);
        while next_block < 200_000.0 {
            if next_packet <= next_block {
                level += 480.0 * count as f64;
                arrived = next_packet;
                packet += count;
                let pair = timing.pairs_every > 0 && packet % timing.pairs_every == 0;
                count = if pair { 2 } else { 1 };
                let newest = packet + count - 1;
                next_packet = (newest as f64 * 10.0 + random() * 3.0).max(next_packet);
            } else {
                let since = Duration::from_secs_f64((next_block - arrived) / 1000.0);
                let fill = input_level(level as usize, 0.0, since, 48_000);
                let ratio = ctl.update(fill);
                // A block needs its 480 frames plus a resampler step on hand
                if next_block > 10_000.0 && level < (480 + CHUNK_IN) as f64 {
                    dry += 1;
                }
                if next_block > 100_000.0 {
                    n += 1.0;
                    sum += ratio;
                    sum2 += ratio * ratio;
                    delay += next_block - played / 48.0;
                }
                level = (level - 480.0 / ratio).max(0.0);
                played += 480.0 / ratio;
                block += 1;
                next_block = timing.phase_ms + block as f64 * 10.0 / (1.0 + timing.clock_offset);
            }
        }
        Run {
            dry,
            wobble_ppm: (sum2 / n - (sum / n).powi(2)).max(0.0).sqrt() * 1e6,
            delay_ms: delay / n,
        }
    }

    /// Packets up to 3 ms late, speakers sweeping through every phase against them
    const LATE: Timing = Timing {
        pairs_every: 0,
        clock_offset: 1e-4,
        phase_ms: 5.0,
    };

    /// 20 ms ran dry now and then, as in the logs; what Auto picks for such input
    /// (13 + 10.1 + 0.7 + 1 ms) doesn't
    #[test]
    fn auto_latency_holds_where_20_ms_ran_dry() {
        assert!(simulate(20.0, LATE).dry > 0);
        assert_eq!(simulate(24.8, LATE).dry, 0);
    }

    /// Packets coming in pairs every second, as in the later logs, used to wobble the speed by
    /// ±1000 ppm every second. (What is left comes from the packets' 3 ms jitter here; 150 ppm
    /// is 0.26 cents.)
    #[test]
    fn packets_in_pairs_keep_the_speed_steady() {
        let pairs = Timing {
            pairs_every: 97,
            ..LATE
        };
        // Waits of up to 23 ms for input
        let run = simulate(23.0 + 10.0 + 0.7 + 1.0, pairs);
        assert_eq!(run.dry, 0, "{run:?}");
        assert!(run.wobble_ppm < 150.0, "{run:?}");
    }

    /// With every packet coming in a pair, a speaker taking its blocks just after they arrive
    /// and one taking them just before get the same delay, so left and right stay together
    #[test]
    fn left_and_right_keep_the_same_delay() {
        let at = |phase_ms| Timing {
            pairs_every: 2,
            clock_offset: 0.0,
            phase_ms,
        };
        // Waits of up to 23 ms for input
        let target = 23.0 + 10.0 + 0.7 + 1.0;
        let (early, late) = (simulate(target, at(0.5)), simulate(target, at(9.5)));
        assert_eq!(early.dry + late.dry, 0, "{early:?} {late:?}");
        assert!(early.wobble_ppm < 150.0 && late.wobble_ppm < 150.0);
        assert!(
            (early.delay_ms - late.delay_ms).abs() < 1.0,
            "{early:?} {late:?}"
        );
    }
}
