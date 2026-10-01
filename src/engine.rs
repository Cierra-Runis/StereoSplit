//! Audio engine: captures stereo from VB-CABLE and sends the left channel to the left speaker
//! (on all of its channels) and the right channel to the right speaker.
//!
//! Each speaker gets a ring buffer holding the source's samples, and a resampler that
//! converts them to the speaker's own sample rate, so the devices don't have to match.
//!
//! Each USB speaker also runs on its own clock, so the two slowly drift apart over time, and
//! each has its own output buffering. So every speaker measures how long the sound actually
//! takes from capture until it plays it (from the time stamps Windows gives each packet), and
//! holds that delay at the same value as the other by nudging its resampling ratio up or down
//! by a tiny amount. Ears notice a difference of a few tens of µs between left and right as
//! the sound moving to one side, so the delay itself is held, not a buffer level that only
//! stands for it.

use crate::config::Config;
use crate::devices;
use crate::latency::{self, AutoLatency};
use crate::meter::{Meter, Reporter, Stats};
use crate::volume::Gain;
use anyhow::{anyhow, bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{ErrorKind, SampleFormat, Stream, StreamConfig, StreamInstant};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Adjustable, Async, FixedAsync, Resampler, SincInterpolationParameters, WindowFunction,
};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
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

/// Holds one speaker's delay (from capture until it plays the sound) at a goal by returning a
/// resampling ratio slightly above or below 1 (proportional + integral control). The integral
/// term settles on the speaker's clock offset, so the delay ends up right at the goal.
///
/// When the goal changes, the delay held moves to it at a fixed [`DriftControl::RAMP`], so
/// both speakers, which get the same goal at the same time, move together and stay aligned all
/// the way. (Each chasing a jump on its own, they took different paths, so the sound swayed to
/// one side and back whenever Auto changed the latency.)
struct DriftControl {
    /// The delay held now, in s: on its way to the goal
    target: f64,
    /// Smoothed difference between the measured delay and `target`, in s
    err: f64,
    integral: f64,
    /// How long, in s, the integral is still left alone after starting
    hold: f64,
}

impl DriftControl {
    /// How long after (re)starting the integral is left alone: a speaker that ran dry often
    /// can't start right at the goal (the sound to skip to it hasn't arrived), and the integral
    /// took that for a clock offset, built up to -600 ppm and drained it dry again and again,
    /// holding it ~0.8 ms behind the other (as logged at 30 ms)
    const HOLD: f64 = 3.0;
    /// Speed change per second of delay off target (300 ppm per ms)
    const KP: f64 = 0.33;
    /// Integral gain per callback, per second of delay off target
    const KI: f64 = 0.00067;
    /// Share of each new measurement taken into the smoothed error, against measurement jitter
    const SMOOTHING: f64 = 0.02;
    /// How fast the delay held moves to a new goal: 1 ms per second plays 0.1% slower or
    /// faster, 1.7 cents, which nobody hears
    const RAMP: f64 = 0.001;
    /// Never change the speed by more than 0.5% (real clock offsets are ~0.01%)
    const LIMIT: f64 = 0.005;

    fn new() -> Self {
        DriftControl {
            target: 0.0,
            err: 0.0,
            integral: 0.0,
            hold: 0.0,
        }
    }

    /// Start (or start over, after running dry) right at `goal`. The integral is kept: the
    /// clock offset hasn't changed.
    fn start(&mut self, goal: f64) {
        self.target = goal;
        self.err = 0.0;
        self.hold = Self::HOLD;
    }

    /// Feed the delay measured now and the goal (in s) and how long the block about to be
    /// played lasts, get the relative ratio to use
    fn update(&mut self, delay: f64, goal: f64, block: f64) -> f64 {
        let step = Self::RAMP * block;
        let moved = (goal - self.target).clamp(-step, step);
        self.target += moved;
        self.err += (delay - self.target - self.err) * Self::SMOOTHING;
        // While the target moves, the error comes from it moving, not from the clock; right
        // after starting, from where it started
        self.hold = (self.hold - block).max(0.0);
        if self.target == goal && self.hold == 0.0 {
            self.integral = (self.integral + Self::KI * self.err).clamp(-Self::LIMIT, Self::LIMIT);
        }
        // Moving the target: play slower (ratio above 1) to build up delay as fast as it moves.
        // Only while it ramps: the goal also wobbles by a µs or so as the output latency is
        // averaged, and following that at once swung the speed by ±100 ppm every block (as
        // logged). Such small steps are left to the error terms.
        let ramp = if moved.abs() >= step && block > 0.0 {
            moved / block
        } else {
            0.0
        };
        // Too much delay -> consume faster -> fewer output frames per input frame -> below 1
        (1.0 + ramp - Self::KP * self.err - self.integral)
            .clamp(1.0 - Self::LIMIT, 1.0 + Self::LIMIT)
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
    /// Source frames taken out of the ring buffer so far
    popped: u64,
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
            popped: 0,
        })
    }

    /// The source frame (counting from the first one ever pushed, with fractions) the next
    /// output sample stands for: the frames popped, less the output still waiting to be played
    /// and the resampler's own delay, in source frames
    fn next_frame(&self) -> f64 {
        let waiting = self.len - self.pos + self.rs.output_delay();
        self.popped as f64 - waiting as f64 / self.ratio
    }

    /// Throw away up to `frames` source frames; returns how many were
    fn skip(&mut self, cons: &mut HeapCons<f32>, frames: usize) -> usize {
        let skipped = cons.skip(frames);
        self.popped += skipped as u64;
        skipped
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
            self.popped += need as u64;
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

/// `t` in the 100 ns ticks Windows time stamps audio with
fn ticks(t: StreamInstant) -> u64 {
    (t.as_nanos() / 100) as u64
}

/// `t` in seconds (since boot)
fn secs(t: StreamInstant) -> f64 {
    t.as_nanos() as f64 * 1e-9
}

/// When each source frame was captured: the input publishes one frame (counting from the first
/// one it pushed) and when it was captured, from which the time of every other frame follows
/// at the sample rate. Both are packed into one atomic, so a speaker never reads one packet's
/// frame with another's time: the frame's lowest [`CaptureClock::FRAME_BITS`] bits and the
/// time's lowest 40 bits (in 100 ns ticks). Everything it is used for lies within a second of
/// the published frame, so the bits cut off (349 s of frames at 48 kHz, 30 h of time) never
/// matter.
struct CaptureClock(AtomicU64);

impl CaptureClock {
    const FRAME_BITS: u32 = 24;
    const FRAME_MASK: u64 = (1 << Self::FRAME_BITS) - 1;
    const TIME_BITS: u32 = 64 - Self::FRAME_BITS;
    /// Before the first packet
    const UNSET: u64 = u64::MAX;

    fn new() -> Self {
        CaptureClock(AtomicU64::new(Self::UNSET))
    }

    /// Source frame `frame` was captured at `at` (in 100 ns ticks)
    fn publish(&self, frame: u64, at: u64) {
        let packed = (at << Self::FRAME_BITS) | (frame & Self::FRAME_MASK);
        self.0.store(packed, Ordering::Relaxed);
    }

    /// How long after source frame `frame` was captured `at` comes (in s, from 100 ns ticks);
    /// None before the first packet
    fn delay(&self, frame: f64, at: u64, rate: u32) -> Option<f64> {
        let packed = self.0.load(Ordering::Relaxed);
        if packed == Self::UNSET {
            return None;
        }
        // The difference of two values of which only the lowest `bits` bits are known, if it
        // is small
        let wrapped = |diff: u64, bits: u32| ((diff << (64 - bits)) as i64 >> (64 - bits)) as f64;
        let whole = frame.floor();
        let frames = wrapped(
            (whole as i64 as u64).wrapping_sub(packed & Self::FRAME_MASK),
            Self::FRAME_BITS,
        );
        let time = wrapped(at.wrapping_sub(packed >> Self::FRAME_BITS), Self::TIME_BITS);
        Some(time * 1e-7 - (frames + frame - whole) / rate as f64)
    }
}

/// What the input and both speakers share
struct Shared {
    /// How much sound to keep buffered (in source frames), on top of the slower output's own
    /// latency; set by Auto
    target: AtomicUsize,
    /// Each speaker's own output latency (left, right), smoothed, in µs; 0 until it has played
    out_lat_us: [AtomicU32; 2],
    clock: CaptureClock,
}

impl Shared {
    /// The delay both speakers keep, from capture until they play the sound, in s: the target
    /// on top of the slower output, so that one keeps the target buffered and the other keeps
    /// more, to play the sound at the same time. None until both speakers have played.
    fn goal(&self, in_rate: u32) -> Option<f64> {
        let [l, r] = self
            .out_lat_us
            .each_ref()
            .map(|a| a.load(Ordering::Relaxed));
        if l == 0 || r == 0 {
            return None;
        }
        let target = self.target.load(Ordering::Relaxed) as f64 / in_rate as f64;
        Some(target + l.max(r) as f64 * 1e-6)
    }
}

/// How much of each callback's output latency goes into the smoothed one (about 2 s)
const OUT_LAT_SMOOTHING: f64 = 0.005;
/// Callbacks whose output latency is left out at the start: the first ones find the output's
/// buffer still empty and report too little (13 ms instead of 33, going by the log), which held
/// the speakers 20 ms short at first, so they ran dry over and over and took 20 s to get there
const OUT_LAT_SETTLE: u32 = 10;

/// Output stream for the speaker of `meter`'s side
fn build_output(
    dev: &cpal::Device,
    in_rate: u32,
    mut cons: HeapCons<f32>,
    shared: Arc<Shared>,
    gain: Gain,
    meter: Arc<Meter>,
    on_error: OnError,
) -> Result<Stream> {
    let config = f32_config(dev, true)?;
    let out_rate = config.sample_rate;
    let channels = config.channels as usize;
    let mut feeder = Feeder::new(in_rate, out_rate)?;
    let mut drift = DriftControl::new();
    let mut primed = false;
    let mut smooth = gain.get();
    let mut out_lat_avg: Option<f64> = None;
    let mut callbacks = 0u32;
    let side = meter.kind();
    let slot = (side == StreamKind::Right) as usize;
    let device = device_name(dev).unwrap_or_default();
    info!(%side, device, in_rate, out_rate, "output resampling");

    let errors = on_stream_error(meter.clone(), on_error);
    let stream = dev.build_output_stream(
        config,
        move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
            let frames = data.len() / channels;
            meter.callback(frames, out_rate);
            let tgt = gain.get();

            // When the first frame written now will be heard, and how long that is from now
            let ts = info.timestamp();
            let playback = ticks(ts.playback);
            let out_lat = ts.playback.duration_since(ts.callback).as_secs_f64();
            if callbacks < OUT_LAT_SETTLE {
                callbacks += 1;
            } else {
                let avg = out_lat_avg.map_or(out_lat, |a| a + (out_lat - a) * OUT_LAT_SMOOTHING);
                out_lat_avg = Some(avg);
                shared.out_lat_us[slot].store(((avg * 1e6) as u32).max(1), Ordering::Relaxed);
            }
            let Some(goal) = shared.goal(in_rate) else {
                data.fill(0.0);
                return;
            };

            // Severe backlog (e.g. after a system stall) -> start over at the goal
            if primed && cons.occupied_len() as f64 > goal * in_rate as f64 * 3.0 {
                primed = false;
                meter.skip();
            }
            // On startup or after running dry, wait until the oldest sound buffered is as old
            // as the goal, and skip what is older, so both speakers start out together. The
            // block also needs to be buffered (if input stopped, the sound is old but not there).
            if !primed {
                feeder.reset();
                let delay = shared.clock.delay(feeder.next_frame(), playback, in_rate);
                let block = frames * in_rate as usize / out_rate as usize + CHUNK_IN;
                match delay {
                    Some(delay) if delay >= goal && cons.occupied_len() >= block => {
                        let excess = ((delay - goal) * in_rate as f64) as usize;
                        let spare = cons.occupied_len() - block;
                        let skipped = feeder.skip(&mut cons, excess.min(spare));
                        meter.started(delay - goal - skipped as f64 / in_rate as f64);
                        drift.start(goal);
                        primed = true;
                    }
                    _ => {
                        data.fill(0.0);
                        return;
                    }
                }
            }

            let Some(delay) = shared.clock.delay(feeder.next_frame(), playback, in_rate) else {
                data.fill(0.0);
                return;
            };
            let rel = drift.update(delay, goal, frames as f64 / out_rate as f64);
            meter.output(delay, out_lat, rel);
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
        let shared = Arc::new(Shared {
            target: AtomicUsize::new(to_frames(latency::START_US)),
            out_lat_us: [AtomicU32::new(0), AtomicU32::new(0)],
            clock: CaptureClock::new(),
        });
        let cap = sample_rate as usize; // 1 second of capacity, enough to absorb any jitter

        let (mut lp, lc): (HeapProd<f32>, HeapCons<f32>) = HeapRb::<f32>::new(cap).split();
        let (mut rp, rc): (HeapProd<f32>, HeapCons<f32>) = HeapRb::<f32>::new(cap).split();

        let meters = [StreamKind::Input, StreamKind::Left, StreamKind::Right]
            .map(|kind| Arc::new(Meter::new(kind)));
        let [in_meter, left_meter, right_meter] = meters.clone();

        let errors = on_stream_error(in_meter.clone(), on_error.clone());
        let input_shared = shared.clone();
        // Frames pushed so far, and when the first and the last frame of the previous packet
        // were captured
        let mut pushed = 0u64;
        let mut prev: Option<(f64, f64)> = None;
        let input = in_dev.build_input_stream(
            in_cfg,
            move |data: &[f32], info: &cpal::InputCallbackInfo| {
                let frames = data.len() / in_ch;
                in_meter.callback(frames, sample_rate);
                for frame in data.chunks(in_ch) {
                    let l = frame[0];
                    let r = if in_ch > 1 { frame[1] } else { l };
                    // Drop samples when the buffer is full (the output side catches up on its
                    // own). The speakers' frame counts are then off from `pushed`, but a full
                    // second of backlog means a speaker has stopped anyway.
                    let _ = lp.try_push(l);
                    let _ = rp.try_push(r);
                }
                let ts = info.timestamp();
                input_shared.clock.publish(pushed, ticks(ts.capture));
                pushed += frames as u64;
                let start = secs(ts.capture);
                let end = start + frames as f64 / sample_rate as f64;
                let now = secs(ts.callback);
                in_meter.packet(prev.map(|(s, _)| start - s), prev.map(|(_, e)| now - e));
                prev = Some((start, end));
            },
            errors,
            None,
        )?;

        let left = build_output(
            &left_dev,
            sample_rate,
            lc,
            shared.clone(),
            gain.clone(),
            left_meter,
            on_error.clone(),
        )?;
        let right = build_output(
            &right_dev,
            sample_rate,
            rc,
            shared.clone(),
            gain,
            right_meter,
            on_error,
        )?;
        input
            .play()
            .map_err(|e| anyhow!("Failed to start capturing sound: {e}"))?;

        info!(
            mode,
            input = %device_name(&in_dev).unwrap_or_default(),
            left = %device_name(&left_dev).unwrap_or_default(),
            right = %device_name(&right_dev).unwrap_or_default(),
            sample_rate,
            "engine started"
        );

        let chunk_us = (CHUNK_IN * 1_000_000 / sample_rate as usize) as u32;
        let mut control = AutoLatency::new(Instant::now(), chunk_us);
        let on_window = move |stats: &[Stats]| {
            if let Some(us) = control.update(Instant::now(), stats) {
                shared.target.store(to_frames(us), Ordering::Relaxed);
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

    /// With the speaker clock 200 ppm off, the delay still settles at the goal
    #[test]
    fn drift_control_settles_on_target() {
        let (goal, block, clock_offset) = (0.030, 0.010, 1.0002);
        let mut ctl = DriftControl::new();
        ctl.start(goal);
        let mut delay = goal;
        for _ in 0..20_000 {
            let rel = ctl.update(delay, goal, block);
            delay += block * clock_offset - block / rel;
        }
        assert!((delay - goal).abs() < 20e-6, "delay settled at {delay}");
    }

    /// The goal wobbling by a µs from one block to the next (the output latency is stored in
    /// whole µs) barely moves the speed
    #[test]
    fn goal_wobble_keeps_the_speed_steady() {
        let (goal, block) = (0.063, 0.010);
        let mut ctl = DriftControl::new();
        ctl.start(goal);
        let mut delay = goal;
        let (mut lo, mut hi) = (f64::MAX, f64::MIN);
        for i in 0..2_000 {
            let wobble = if i % 2 == 0 { 1e-6 } else { 0.0 };
            let rel = ctl.update(delay, goal + wobble, block);
            delay += block - block / rel;
            (lo, hi) = (lo.min(rel), hi.max(rel));
        }
        let ppm = (hi - lo) * 1e6;
        assert!(ppm < 5.0, "speed swung by {ppm} ppm");
    }

    /// Frame counts and times are worked out right across the points where the bits kept of
    /// them start over
    #[test]
    fn capture_clock_counts_across_wraparound() {
        let clock = CaptureClock::new();
        assert_eq!(clock.delay(0.0, 0, 48_000), None);
        // A frame captured 5 ms before the time bits start over, 10 frames before the frame
        // bits do (as if after days of running)
        let frame = (7 << 24) + (1 << 24) - 10;
        let at = (3 << 40) + (1 << 40) - 50_000;
        clock.publish(frame, at);
        // Half a frame past 10 ms of frames later, played 30 ms after the published one was
        // captured
        let delay = clock.delay((frame + 480) as f64 + 0.5, at + 300_000, 48_000);
        let expected = 0.030 - 480.5 / 48_000.0;
        assert!((delay.unwrap() - expected).abs() < 1e-9, "{delay:?}");
        // And a frame from before the published one
        let delay = clock.delay(frame as f64 - 48.0, at + 100_000, 48_000);
        assert!((delay.unwrap() - 0.011).abs() < 1e-9, "{delay:?}");
    }

    /// How the input arrives and a speaker takes it, for [`simulate`]
    #[derive(Debug, Clone, Copy)]
    struct Timing {
        /// Every how many packets one comes a whole packet late, with the next; 0 for never
        pairs_every: u64,
        /// How much faster the speaker's clock runs; 100 ppm sweeps it through every phase
        /// against the input in 100 s, 0 keeps it at `phase_ms` after each packet
        clock_offset: f64,
        phase_ms: f64,
        /// How long after a block is handed over the speaker plays it
        out_lat_ms: f64,
    }

    /// What [`simulate`] saw
    #[derive(Debug)]
    struct Run {
        /// Blocks that found less than they needed
        dry: u32,
        /// How much the speed varied in the second half (standard deviation), in ppm
        wobble_ppm: f64,
        /// How long after it was captured the sound was played, on average over each second,
        /// in ms
        delay_ms: Vec<f64>,
    }

    /// A random number generator in [0, 1)
    fn random(mut seed: u64) -> impl FnMut() -> f64 {
        move || {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (seed >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    /// 200 s of one shared-mode speaker keeping `ring_ms(t)` buffered on top of the slower
    /// output (`max_out_lat_ms`): 10 ms input packets arriving up to 3 ms late (as in the
    /// logs, the same packets for every speaker), 10 ms blocks, and playback times that Windows
    /// reports up to 0.3 ms off
    fn simulate(ring_ms: impl Fn(f64) -> f64, max_out_lat_ms: f64, timing: Timing) -> Run {
        let mut input_random = random(1);
        let mut noise = random(2 + (timing.phase_ms * 1000.0) as u64);
        let clock = CaptureClock::new();
        let mut ctl = DriftControl::new();
        let (mut primed, mut dry) = (false, 0);
        // Source frames taken out (frame i is captured at i / 48 ms), and arrived
        let (mut consumed, mut arrived) = (0.0f64, 0.0f64);
        // Packet p (from 1) holds the frames captured from 10(p - 1) until 10p ms. It arrives
        // up to 3 ms after that, or, if it is one to come late, together with the next.
        let (mut packet, mut count) = (1u64, 1u64);
        let mut next_packet = 10.0 + input_random() * 3.0;
        let (mut block, mut next_block) = (0u64, timing.phase_ms);
        let mut windows = vec![(0.0, 0.0); 200];
        let (mut n, mut sum, mut sum2) = (0.0, 0.0, 0.0);
        while next_block < 200_000.0 {
            if next_packet <= next_block {
                for p in packet..packet + count {
                    clock.publish(480 * (p - 1), (p - 1) * 100_000);
                }
                packet += count;
                arrived = 480.0 * (packet - 1) as f64;
                let pair = timing.pairs_every > 0 && packet % timing.pairs_every == 0;
                count = if pair { 2 } else { 1 };
                let newest = packet + count - 1;
                next_packet = (newest as f64 * 10.0 + input_random() * 3.0).max(next_packet);
                continue;
            }
            let t = next_block;
            let playback = t + timing.out_lat_ms;
            let reported = ((playback + (noise() - 0.5) * 0.6) * 10_000.0) as u64;
            let goal = (ring_ms(t) + max_out_lat_ms) / 1000.0;
            // A block needs its 480 frames plus a resampler step on hand
            let needed = (480 + CHUNK_IN) as f64;
            if !primed {
                if let Some(delay) = clock.delay(consumed, reported, 48_000) {
                    if delay >= goal && arrived - consumed >= needed {
                        consumed += ((delay - goal) * 48_000.0).min(arrived - consumed - needed);
                        ctl.start(goal);
                        primed = true;
                    }
                }
            }
            if primed {
                let delay = clock.delay(consumed, reported, 48_000).unwrap();
                let ratio = ctl.update(delay, goal, 0.010);
                if arrived - consumed < needed {
                    if t > 10_000.0 {
                        dry += 1;
                    }
                    primed = false;
                } else {
                    let w = &mut windows[(t / 1000.0) as usize];
                    w.0 += playback - consumed / 48.0;
                    w.1 += 1.0;
                    if t > 100_000.0 {
                        n += 1.0;
                        sum += ratio;
                        sum2 += ratio * ratio;
                    }
                    consumed += 480.0 / ratio;
                }
            }
            block += 1;
            next_block = timing.phase_ms + block as f64 * 10.0 / (1.0 + timing.clock_offset);
        }
        Run {
            dry,
            wobble_ppm: (sum2 / n - (sum / n).powi(2)).max(0.0).sqrt() * 1e6,
            delay_ms: windows.iter().map(|(sum, n)| sum / n).collect(),
        }
    }

    /// Packets up to 3 ms late, a speaker sweeping through every phase against them
    const LATE: Timing = Timing {
        pairs_every: 0,
        clock_offset: 1e-4,
        phase_ms: 5.0,
        out_lat_ms: 20.0,
    };

    /// 20 ms ran dry now and then, as in the logs; what Auto picks for such input
    /// (13 + 10.1 + 0.7 + 1 ms) doesn't
    #[test]
    fn auto_latency_holds_where_20_ms_ran_dry() {
        assert!(simulate(|_| 20.0, 20.0, LATE).dry > 0);
        assert_eq!(simulate(|_| 24.8, 20.0, LATE).dry, 0);
    }

    /// Packets coming in pairs every second, as in the later logs, used to wobble the speed by
    /// ±1000 ppm every second. Steering by when the sound was captured, they don't show at all.
    #[test]
    fn packets_in_pairs_keep_the_speed_steady() {
        let pairs = Timing {
            pairs_every: 97,
            ..LATE
        };
        // Waits of up to 23 ms for input
        let run = simulate(|_| 23.0 + 10.0 + 0.7 + 1.0, 20.0, pairs);
        assert_eq!(run.dry, 0, "{run:?}");
        assert!(run.wobble_ppm < 30.0, "{run:?}");
    }

    /// The most left and right were apart over any second from 30 s on (by then the drift
    /// control has learned the clock offsets), in ms. Without the noise in the reported
    /// playback times it is 0; with it, a few tens of µs.
    fn skew_ms(left: &Run, right: &Run) -> f64 {
        let skew = left
            .delay_ms
            .iter()
            .zip(&right.delay_ms)
            .map(|(l, r)| l - r);
        skew.skip(30).fold(0.0, |max: f64, s| max.max(s.abs()))
    }

    /// Speakers with different output latencies and clocks, taking their blocks just after
    /// the packets arrive and just before, still play the sound at the same time
    #[test]
    fn left_and_right_play_together() {
        let left = Timing {
            pairs_every: 97,
            clock_offset: 1e-4,
            phase_ms: 0.5,
            out_lat_ms: 12.0,
        };
        let right = Timing {
            clock_offset: -0.8e-4,
            phase_ms: 9.5,
            out_lat_ms: 22.0,
            ..left
        };
        let ring = |_| 23.0 + 10.0 + 0.7 + 1.0;
        let (l, r) = (simulate(ring, 22.0, left), simulate(ring, 22.0, right));
        assert_eq!(l.dry + r.dry, 0, "{l:?} {r:?}");
        let skew = skew_ms(&l, &r);
        assert!(skew < 0.05, "left and right up to {skew} ms apart");
    }

    /// When Auto changes the latency, both speakers get there together
    #[test]
    fn latency_changes_keep_left_and_right_together() {
        let left = Timing {
            clock_offset: 1e-4,
            phase_ms: 0.5,
            out_lat_ms: 12.0,
            ..LATE
        };
        let right = Timing {
            clock_offset: -0.8e-4,
            phase_ms: 9.5,
            out_lat_ms: 22.0,
            ..LATE
        };
        let ring = |t| {
            if (100_000.0..160_000.0).contains(&t) {
                32.6
            } else {
                24.8
            }
        };
        let (l, r) = (simulate(ring, 22.0, left), simulate(ring, 22.0, right));
        assert_eq!(l.dry + r.dry, 0, "{l:?} {r:?}");
        let skew = skew_ms(&l, &r);
        assert!(skew < 0.05, "left and right up to {skew} ms apart");
        // 7.8 ms at 1 ms/s
        for (second, ring) in [(99, 24.8), (112, 32.6), (159, 32.6), (172, 24.8)] {
            let delay = l.delay_ms[second];
            assert!(
                (delay - ring - 22.0).abs() < 0.1,
                "{delay} ms at {second} s, wanted {ring} + 22"
            );
        }
    }
}
