//! Health counters for the engine's streams, updated from the audio callbacks and logged every
//! [`WINDOW`]: how often a speaker ran dry, how low its buffer got, how hard the drift control
//! is pushing, and how regularly each stream's callbacks arrive.
//!
//! A window with a dropout is logged as a warning, so crackling leaves a trace in the log
//! along with what ran late. Windows without one are logged only at debug level
//! (`RUST_LOG=stereo_split=debug`).

use crate::engine::StreamKind;
use std::fmt;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering::Relaxed};
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
    /// Output only: times the buffer ran dry, so the speaker played silence until it refilled
    dry: AtomicU32,
    /// Output only: times a backlog was skipped
    skips: AtomicU32,
    /// Output only: lowest buffer level the drift control saw, in µs; u32::MAX if none
    fill_min_us: AtomicU32,
    /// Output only: range of the drift control's speed adjustment, in ppm; empty if min > max
    drift_min_ppm: AtomicI32,
    drift_max_ppm: AtomicI32,
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
            dry: AtomicU32::new(0),
            skips: AtomicU32::new(0),
            fill_min_us: AtomicU32::new(u32::MAX),
            drift_min_ppm: AtomicI32::new(i32::MAX),
            drift_max_ppm: AtomicI32::new(i32::MIN),
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

    pub fn dry(&self) {
        self.dry.fetch_add(1, Relaxed);
    }

    pub fn skip(&self) {
        self.skips.fetch_add(1, Relaxed);
    }

    /// The buffer level (`fill` frames at `rate`) and the speed ratio the drift control just
    /// worked out from it
    pub fn drift(&self, fill: f32, rate: u32, ratio: f64) {
        self.fill_min_us
            .fetch_min(frames_us(fill as f64, rate), Relaxed);
        let ppm = ((ratio - 1.0) * 1e6).round() as i32;
        self.drift_min_ppm.fetch_min(ppm, Relaxed);
        self.drift_max_ppm.fetch_max(ppm, Relaxed);
    }

    /// What was counted since the last call, starting over
    fn take(&self) -> Stats {
        let fill_min_us = self.fill_min_us.swap(u32::MAX, Relaxed);
        let drift = (
            self.drift_min_ppm.swap(i32::MAX, Relaxed),
            self.drift_max_ppm.swap(i32::MIN, Relaxed),
        );
        Stats {
            kind: self.kind,
            callbacks: self.callbacks.swap(0, Relaxed),
            gap_max_us: self.gap_max_us.swap(0, Relaxed),
            block_max_us: self.block_max_us.swap(0, Relaxed),
            xruns: self.xruns.swap(0, Relaxed),
            dry: self.dry.swap(0, Relaxed),
            skips: self.skips.swap(0, Relaxed),
            fill_min_us: (fill_min_us != u32::MAX).then_some(fill_min_us),
            drift_ppm: (drift.0 <= drift.1).then_some(drift),
        }
    }
}

/// What a [`Meter`] counted over one window
#[derive(Debug, PartialEq)]
struct Stats {
    kind: StreamKind,
    callbacks: u32,
    gap_max_us: u32,
    block_max_us: u32,
    xruns: u32,
    dry: u32,
    skips: u32,
    fill_min_us: Option<u32>,
    drift_ppm: Option<(i32, i32)>,
}

impl Stats {
    /// Whether something audible went wrong: a gap in the sound, or a stream that stalled
    fn dropout(&self) -> bool {
        self.dry > 0 || self.skips > 0 || self.xruns > 0 || self.callbacks == 0
    }
}

/// e.g. `left[dry=2 skips=0 fill_min=6.2ms drift=-5000..-1200ppm callbacks=1000 gap_max=10.7ms
/// block_max=10.0ms xruns=0]`
impl fmt::Display for Stats {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let ms = |us: u32| us as f32 / 1000.0;
        write!(f, "{}[", self.kind)?;
        if self.kind != StreamKind::Input {
            write!(f, "dry={} skips={} ", self.dry, self.skips)?;
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
            ms(self.gap_max_us),
            ms(self.block_max_us),
            self.xruns
        )
    }
}

/// Logs a set of meters every [`WINDOW`] on its own thread, until dropped
pub struct Reporter {
    /// Never sent on: dropping it disconnects the channel, which ends the thread
    _stop: Sender<()>,
}

impl Reporter {
    pub fn start(meters: Vec<Arc<Meter>>) -> Reporter {
        let (stop, rx) = mpsc::channel::<()>();
        let spawned = std::thread::Builder::new()
            .name("meter".into())
            .spawn(move || {
                // The window cut short by stopping is dropped: the streams stop one by one, so
                // the speakers run dry then, which is no dropout
                while let Err(RecvTimeoutError::Timeout) = rx.recv_timeout(WINDOW) {
                    let stats: Vec<Stats> = meters.iter().map(|m| m.take()).collect();
                    let line = stats
                        .iter()
                        .map(Stats::to_string)
                        .collect::<Vec<_>>()
                        .join(" ");
                    if stats.iter().any(Stats::dropout) {
                        warn!("audio dropouts: {line}");
                    } else {
                        debug!("audio: {line}");
                    }
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
        m.drift(480.0, 48_000, 0.995);
        m.drift(960.0, 48_000, 1.0);

        let s = m.take();
        assert_eq!(
            (s.callbacks, s.block_max_us, s.dry, s.skips),
            (2, 20_000, 1, 0)
        );
        assert_eq!(s.fill_min_us, Some(10_000));
        assert_eq!(s.drift_ppm, Some((-5000, 0)));
        assert!(s.dropout());

        let s = m.take();
        assert_eq!((s.callbacks, s.dry), (0, 0));
        assert_eq!((s.fill_min_us, s.drift_ppm), (None, None));
    }

    #[test]
    fn log_format() {
        let s = Stats {
            kind: StreamKind::Right,
            callbacks: 1000,
            gap_max_us: 10_700,
            block_max_us: 10_000,
            xruns: 0,
            dry: 2,
            skips: 0,
            fill_min_us: Some(6_240),
            drift_ppm: Some((-5000, -1200)),
        };
        assert_eq!(
            s.to_string(),
            "right[dry=2 skips=0 fill_min=6.2ms drift=-5000..-1200ppm callbacks=1000 \
             gap_max=10.7ms block_max=10.0ms xruns=0]"
        );
        let input = Stats {
            kind: StreamKind::Input,
            fill_min_us: None,
            drift_ppm: None,
            ..s
        };
        assert_eq!(
            input.to_string(),
            "input[callbacks=1000 gap_max=10.7ms block_max=10.0ms xruns=0]"
        );
    }
}
