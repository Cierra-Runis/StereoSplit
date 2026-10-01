//! Health counters for the engine's streams, updated from the audio callbacks and logged every
//! [`WINDOW`]: how often a speaker ran dry, how long the sound takes to get through to it, how
//! far apart left and right are, how hard the drift control is pushing, and how regularly each
//! stream's callbacks arrive.
//!
//! A window with a dropout is logged as a warning, so crackling leaves a trace in the log
//! along with what ran late. Windows without one are logged only at debug level
//! (`RUST_LOG=stereo_split=debug`).

use crate::engine::StreamKind;
use std::fmt;
use std::sync::atomic::{AtomicI32, AtomicI64, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, warn};

/// How much time each log line covers
const WINDOW: Duration = Duration::from_secs(1);

/// `x` in a u32, or u32::MAX if it doesn't fit
fn saturate(x: impl TryInto<u32>) -> u32 {
    x.try_into().unwrap_or(u32::MAX)
}

/// `frames` at `rate` as microseconds
fn frames_us(frames: f64, rate: u32) -> u32 {
    (frames * 1e6 / rate as f64) as u32
}

/// `secs` as whole microseconds, saturating
fn secs_us(secs: f64) -> i32 {
    (secs * 1e6).round() as i32
}

/// Lowest and highest of the values recorded since the last [`MinMax::take`]
struct MinMax {
    min: AtomicI32,
    max: AtomicI32,
}

impl MinMax {
    fn new() -> MinMax {
        MinMax {
            min: AtomicI32::new(i32::MAX),
            max: AtomicI32::new(i32::MIN),
        }
    }

    fn record(&self, v: i32) {
        self.min.fetch_min(v, Relaxed);
        self.max.fetch_max(v, Relaxed);
    }

    /// (min, max), or None if nothing was recorded; starts over
    fn take(&self) -> Option<(i32, i32)> {
        let min = self.min.swap(i32::MAX, Relaxed);
        let max = self.max.swap(i32::MIN, Relaxed);
        (min <= max).then_some((min, max))
    }
}

/// Counters for one stream. Written by that stream's callbacks, read and reset by the reporter
/// thread. Only atomics, so it is safe to use in an audio callback.
pub struct Meter {
    kind: StreamKind,
    epoch: Instant,
    /// When the last callback came, in µs since `epoch`; u64::MAX before the first one
    last_us: AtomicU64,
    callbacks: AtomicU32,
    /// Longest time from one callback to the next, in µs
    gap_max_us: AtomicU32,
    /// Most audio passed in one callback, in µs
    block_max_us: AtomicU32,
    /// Discontinuities the device reported (cpal's `ErrorKind::Xrun`, only reported for input)
    xruns: AtomicU32,
    /// Input only: time from the capture of one packet to the next, in µs: tells whether a
    /// packet that arrived late was also captured late, or only handed over late
    cap_gap_us: MinMax,
    /// Input only: how old the newest input had got when the next packet arrived, in µs
    age_us: MinMax,
    /// Output only: times the buffer ran dry, so the speaker played silence until it refilled
    dry: AtomicU32,
    /// Output only: times a backlog was skipped
    skips: AtomicU32,
    /// Output only: how far past the goal it started playing (after starting up or running
    /// dry), in µs: a start can only skip ahead to sound that has arrived
    late_start_us: MinMax,
    /// Output only: how long from capture until the speaker plays the sound, in µs
    delay_us: MinMax,
    delay_sum_us: AtomicI64,
    delay_count: AtomicU32,
    /// Output only: how long from the callback until the speaker plays what it writes, in µs
    out_lat_us: MinMax,
    /// Output only: how much was buffered on our side (the delay minus the output's), in µs
    fill_us: MinMax,
    /// Output only: the drift control's speed adjustment, in ppm
    drift_ppm: MinMax,
}

impl Meter {
    pub fn new(kind: StreamKind) -> Meter {
        Meter {
            kind,
            epoch: Instant::now(),
            last_us: AtomicU64::new(u64::MAX),
            callbacks: AtomicU32::new(0),
            gap_max_us: AtomicU32::new(0),
            block_max_us: AtomicU32::new(0),
            xruns: AtomicU32::new(0),
            cap_gap_us: MinMax::new(),
            age_us: MinMax::new(),
            dry: AtomicU32::new(0),
            skips: AtomicU32::new(0),
            late_start_us: MinMax::new(),
            delay_us: MinMax::new(),
            delay_sum_us: AtomicI64::new(0),
            delay_count: AtomicU32::new(0),
            out_lat_us: MinMax::new(),
            fill_us: MinMax::new(),
            drift_ppm: MinMax::new(),
        }
    }

    pub fn kind(&self) -> StreamKind {
        self.kind
    }

    /// Call at the start of every data callback, with the number of frames it passes at `rate`
    pub fn callback(&self, frames: usize, rate: u32) {
        let now = saturate(self.epoch.elapsed().as_micros()) as u64;
        if let Some(gap) = now.checked_sub(self.last_us.swap(now, Relaxed)) {
            self.gap_max_us.fetch_max(saturate(gap), Relaxed);
        }
        self.callbacks.fetch_add(1, Relaxed);
        self.block_max_us
            .fetch_max(frames_us(frames as f64, rate), Relaxed);
    }

    pub fn xrun(&self) {
        self.xruns.fetch_add(1, Relaxed);
    }

    /// An input packet was captured `cap_gap` s after the one before, and arrived when the
    /// newest input before it was `age` s old (both None for the first packet)
    pub fn packet(&self, cap_gap: Option<f64>, age: Option<f64>) {
        if let Some(gap) = cap_gap {
            self.cap_gap_us.record(secs_us(gap));
        }
        if let Some(age) = age {
            self.age_us.record(secs_us(age));
        }
    }

    pub fn dry(&self) {
        self.dry.fetch_add(1, Relaxed);
    }

    pub fn skip(&self) {
        self.skips.fetch_add(1, Relaxed);
    }

    /// Started playing `late` s past the goal
    pub fn started(&self, late: f64) {
        self.late_start_us.record(secs_us(late));
    }

    /// What an output callback measured: how long from capture until the speaker plays the
    /// sound (`delay`, in s), how much of that is the output's own (`out_lat`), and the speed
    /// ratio the drift control worked out
    pub fn output(&self, delay: f64, out_lat: f64, ratio: f64) {
        let delay_us = secs_us(delay);
        self.delay_us.record(delay_us);
        self.delay_sum_us.fetch_add(delay_us as i64, Relaxed);
        self.delay_count.fetch_add(1, Relaxed);
        self.out_lat_us.record(secs_us(out_lat));
        self.fill_us.record(secs_us(delay - out_lat));
        self.drift_ppm.record(((ratio - 1.0) * 1e6).round() as i32);
    }

    /// What was counted since the last call, starting over
    fn take(&self) -> Stats {
        let delay = self.delay_us.take();
        let sum = self.delay_sum_us.swap(0, Relaxed);
        let count = self.delay_count.swap(0, Relaxed);
        Stats {
            kind: self.kind,
            callbacks: self.callbacks.swap(0, Relaxed),
            gap_max_us: self.gap_max_us.swap(0, Relaxed),
            block_max_us: self.block_max_us.swap(0, Relaxed),
            xruns: self.xruns.swap(0, Relaxed),
            cap_gap_us: self.cap_gap_us.take(),
            age_max_us: self.age_us.take().map(|(_, max)| max),
            dry: self.dry.swap(0, Relaxed),
            skips: self.skips.swap(0, Relaxed),
            late_start_us: self.late_start_us.take().map(|(_, max)| max),
            delay_us: delay
                .filter(|_| count > 0)
                .map(|(min, max)| (min, (sum / count as i64) as i32, max)),
            out_lat_us: self.out_lat_us.take(),
            fill_min_us: self.fill_us.take().map(|(min, _)| min),
            drift_ppm: self.drift_ppm.take(),
        }
    }
}

/// What a [`Meter`] counted over one window
#[derive(Debug, Clone, PartialEq)]
pub struct Stats {
    pub kind: StreamKind,
    pub callbacks: u32,
    pub gap_max_us: u32,
    pub block_max_us: u32,
    pub xruns: u32,
    /// Input only: (min, max) of the time from the capture of one packet to the next
    pub cap_gap_us: Option<(i32, i32)>,
    /// Input only: the oldest the newest input got before the next packet arrived
    pub age_max_us: Option<i32>,
    pub dry: u32,
    pub skips: u32,
    /// Output only: the furthest past the goal it started playing
    pub late_start_us: Option<i32>,
    /// Output only: (min, mean, max) of the delay from capture to playback
    pub delay_us: Option<(i32, i32, i32)>,
    pub out_lat_us: Option<(i32, i32)>,
    pub fill_min_us: Option<i32>,
    pub drift_ppm: Option<(i32, i32)>,
}

impl Stats {
    /// Whether something audible went wrong: a gap in the sound, or a stream that stalled
    fn dropout(&self) -> bool {
        self.dry > 0 || self.skips > 0 || self.xruns > 0 || self.callbacks == 0
    }
}

/// How much later the left speaker plays the sound than the right one, on average over the
/// window, in µs; None unless both played
pub fn skew_us(stats: &[Stats]) -> Option<i32> {
    let mean = |kind| {
        stats
            .iter()
            .find(|s| s.kind == kind)
            .and_then(|s| s.delay_us)
            .map(|(_, mean, _)| mean)
    };
    Some(mean(StreamKind::Left)? - mean(StreamKind::Right)?)
}

/// e.g. `left[dry=2 skips=0 delay=31.20..31.31ms out_lat=20.1..21.0ms fill_min=10.2ms
/// drift=-50..12ppm callbacks=100 gap_max=10.7ms block_max=10.0ms xruns=0]`
impl fmt::Display for Stats {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let ms = |us: i32| us as f32 / 1000.0;
        write!(f, "{}[", self.kind)?;
        if let Some((lo, hi)) = self.cap_gap_us {
            write!(f, "cap_gap={:.2}..{:.2}ms ", ms(lo), ms(hi))?;
        }
        if let Some(us) = self.age_max_us {
            write!(f, "age_max={:.1}ms ", ms(us))?;
        }
        if self.kind != StreamKind::Input {
            write!(f, "dry={} skips={} ", self.dry, self.skips)?;
            if let Some(us) = self.late_start_us {
                write!(f, "late_start={:.2}ms ", ms(us))?;
            }
            if let Some((lo, _, hi)) = self.delay_us {
                write!(f, "delay={:.2}..{:.2}ms ", ms(lo), ms(hi))?;
            }
            if let Some((lo, hi)) = self.out_lat_us {
                write!(f, "out_lat={:.1}..{:.1}ms ", ms(lo), ms(hi))?;
            }
            if let Some(us) = self.fill_min_us {
                write!(f, "fill_min={:.1}ms ", ms(us))?;
            }
            if let Some((lo, hi)) = self.drift_ppm {
                write!(f, "drift={lo}..{hi}ppm ")?;
            }
        }
        write!(
            f,
            "callbacks={} gap_max={:.1}ms block_max={:.1}ms xruns={}]",
            self.callbacks,
            self.gap_max_us as f32 / 1000.0,
            self.block_max_us as f32 / 1000.0,
            self.xruns
        )
    }
}

/// The log line for one window: every stream, then how far apart left and right are
fn line(stats: &[Stats]) -> String {
    let mut line = stats
        .iter()
        .map(Stats::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    if let Some(us) = skew_us(stats) {
        line += &format!(" skew={:+.3}ms", us as f32 / 1000.0);
    }
    line
}

/// Logs a set of meters every [`WINDOW`] on its own thread, and hands what they counted to
/// `on_window`, until dropped
pub struct Reporter {
    /// Never sent on: dropping it disconnects the channel, which ends the thread
    _stop: Sender<()>,
}

impl Reporter {
    pub fn start(
        meters: Vec<Arc<Meter>>,
        mut on_window: impl FnMut(&[Stats]) + Send + 'static,
    ) -> Reporter {
        let (stop, rx) = mpsc::channel::<()>();
        let spawned = std::thread::Builder::new()
            .name("meter".into())
            .spawn(move || {
                // The window cut short by stopping is dropped: the streams stop one by one, so
                // the speakers run dry then, which is no dropout
                while let Err(RecvTimeoutError::Timeout) = rx.recv_timeout(WINDOW) {
                    let stats: Vec<Stats> = meters.iter().map(|m| m.take()).collect();
                    if stats.iter().any(Stats::dropout) {
                        warn!("audio dropouts: {}", line(&stats));
                    } else {
                        debug!("audio: {}", line(&stats));
                    }
                    on_window(&stats);
                }
            });
        if let Err(e) = spawned {
            warn!(error = %e, "can't start the audio meter; dropouts won't be logged");
        }
        Reporter { _stop: stop }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_reports_the_window_and_starts_over() {
        let m = Meter::new(StreamKind::Left);
        m.callback(480, 48_000);
        m.callback(960, 48_000);
        m.dry();
        m.output(0.031, 0.021, 0.995);
        m.output(0.033, 0.020, 1.0);

        let s = m.take();
        assert_eq!(
            (s.callbacks, s.block_max_us, s.dry, s.skips),
            (2, 20_000, 1, 0)
        );
        assert_eq!(s.delay_us, Some((31_000, 32_000, 33_000)));
        assert_eq!(s.out_lat_us, Some((20_000, 21_000)));
        assert_eq!(s.fill_min_us, Some(10_000));
        assert_eq!(s.drift_ppm, Some((-5000, 0)));
        assert!(s.dropout());

        let s = m.take();
        assert_eq!((s.callbacks, s.dry), (0, 0));
        assert_eq!((s.delay_us, s.fill_min_us, s.drift_ppm), (None, None, None));
    }

    #[test]
    fn input_packets() {
        let m = Meter::new(StreamKind::Input);
        m.packet(None, None);
        m.packet(Some(0.010), Some(0.0105));
        m.packet(Some(0.0201), Some(0.013));
        let s = m.take();
        assert_eq!(s.cap_gap_us, Some((10_000, 20_100)));
        assert_eq!(s.age_max_us, Some(13_000));
    }

    fn output(kind: StreamKind, mean_us: i32) -> Stats {
        Stats {
            kind,
            callbacks: 100,
            gap_max_us: 10_700,
            block_max_us: 10_000,
            xruns: 0,
            cap_gap_us: None,
            age_max_us: None,
            dry: 2,
            skips: 0,
            late_start_us: None,
            delay_us: Some((mean_us - 50, mean_us, mean_us + 60)),
            out_lat_us: Some((20_100, 21_000)),
            fill_min_us: Some(10_240),
            drift_ppm: Some((-50, 12)),
        }
    }

    #[test]
    fn log_format() {
        let left = output(StreamKind::Left, 31_250);
        assert_eq!(
            left.to_string(),
            "left[dry=2 skips=0 delay=31.20..31.31ms out_lat=20.1..21.0ms fill_min=10.2ms \
             drift=-50..12ppm callbacks=100 gap_max=10.7ms block_max=10.0ms xruns=0]"
        );
        let input = Stats {
            kind: StreamKind::Input,
            cap_gap_us: Some((9_980, 10_020)),
            age_max_us: Some(13_000),
            delay_us: None,
            out_lat_us: None,
            fill_min_us: None,
            drift_ppm: None,
            ..left.clone()
        };
        assert_eq!(
            input.to_string(),
            "input[cap_gap=9.98..10.02ms age_max=13.0ms callbacks=100 gap_max=10.7ms \
             block_max=10.0ms xruns=0]"
        );
        let right = output(StreamKind::Right, 31_238);
        assert!(line(&[input, left, right]).ends_with("xruns=0] skew=+0.012ms"));
    }
}
