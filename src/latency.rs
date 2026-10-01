//! Automatic latency: the smallest buffer that keeps the speakers from running dry, worked out
//! from how the streams actually deliver and take the sound.
//!
//! A speaker runs dry when it has to take a block while input it counted on hasn't arrived yet.
//! The drift control holds how long ago the sound it plays next was captured, so what is
//! actually in the buffer when a speaker takes a block is that time less how old the newest
//! input is. The latency must therefore cover the oldest the input gets before the next packet
//! arrives, plus the block (see [`needed_us`]). It is kept on top of the slower speaker's own
//! output latency; the other speaker keeps more, to play the sound at the same time.

use crate::engine::StreamKind;
use crate::meter::Stats;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Used until the first measurements are in
pub const START_US: u32 = 30_000;
/// How long a measurement counts: the latency rises as soon as more is needed, and comes down
/// only once nothing in this long has needed it
const MEMORY: Duration = Duration::from_secs(60);
/// Measurements are ignored for this long after starting, as the streams start up unevenly
/// (shared-mode output fills its whole buffer at once, for one)
const SETTLE: Duration = Duration::from_secs(2);
/// On top of what the measurements call for: the drift control holds the level near its target
/// rather than right at it
const MARGIN_US: u32 = 1_000;
/// Added for each window in which a speaker ran dry anyway, for what the measurements miss, and
/// taken off again one step per [`MEMORY`] without
const EXTRA_STEP_US: u32 = 1_000;
const EXTRA_MAX_US: u32 = 10_000;
const MIN_US: u32 = 5_000;
const MAX_US: u32 = 200_000;

/// The latency one window of measurements calls for, in µs; None if they don't show it (a
/// stream that delivered nothing)
///
/// It is the oldest the newest input got before the next packet arrived (`age_max` of the
/// input: the time between packets plus how late after capture they arrive), plus the longest
/// a speaker went between taking blocks (its block plus how unevenly it takes them), plus a
/// resampler step (`chunk_us`) and a margin.
pub fn needed_us(stats: &[Stats], chunk_us: u32) -> Option<u32> {
    let input = stats
        .iter()
        .find(|s| s.kind == StreamKind::Input && s.callbacks > 0)?;
    let age = u32::try_from(input.age_max_us?).unwrap_or(0);
    let outputs = stats
        .iter()
        .filter(|s| s.kind != StreamKind::Input && s.callbacks > 0);
    let output = outputs.map(|s| s.gap_max_us).max()?;
    Some(age + output + chunk_us + MARGIN_US)
}

/// Keeps the latency at what the recent measurements call for
pub struct AutoLatency {
    chunk_us: u32,
    started: Instant,
    target_us: u32,
    /// What each recent window called for, oldest first
    needs: VecDeque<(Instant, u32)>,
    extra_us: u32,
    /// When `extra_us` last went up or down
    extra_changed: Instant,
}

impl AutoLatency {
    pub fn new(now: Instant, chunk_us: u32) -> AutoLatency {
        AutoLatency {
            chunk_us,
            started: now,
            target_us: START_US,
            needs: VecDeque::new(),
            extra_us: 0,
            extra_changed: now,
        }
    }

    /// Take in one window of measurements. Returns the new latency in µs if it changed.
    pub fn update(&mut self, now: Instant, stats: &[Stats]) -> Option<u32> {
        if now.duration_since(self.started) < SETTLE {
            return None;
        }
        let dry = stats
            .iter()
            .any(|s| s.kind != StreamKind::Input && s.dry > 0);
        if dry {
            self.extra_us = (self.extra_us + EXTRA_STEP_US).min(EXTRA_MAX_US);
            self.extra_changed = now;
        } else if self.extra_us > 0 && now.duration_since(self.extra_changed) >= MEMORY {
            self.extra_us -= EXTRA_STEP_US;
            self.extra_changed = now;
        }
        if let Some(need) = needed_us(stats, self.chunk_us) {
            self.needs.push_back((now, need));
        }
        while let Some(&(t, _)) = self.needs.front() {
            if now.duration_since(t) <= MEMORY {
                break;
            }
            self.needs.pop_front();
        }
        let need = self.needs.iter().map(|&(_, n)| n).max()?;
        let target = (need + self.extra_us).clamp(MIN_US, MAX_US);
        if target == self.target_us {
            return None;
        }
        self.target_us = target;
        Some(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(kind: StreamKind, gap_max_us: u32, dry: u32) -> Stats {
        let input = kind == StreamKind::Input;
        Stats {
            kind,
            callbacks: 100,
            gap_max_us,
            block_max_us: 10_000,
            xruns: 0,
            cap_gap_us: input.then_some((10_000, 10_000)),
            age_max_us: input.then_some(13_000),
            dry,
            skips: 0,
            late_start_us: None,
            delay_us: None,
            out_lat_us: None,
            fill_min_us: None,
            drift_ppm: None,
        }
    }

    /// Loopback packets of 10 ms, arriving up to 3 ms late, and speakers taking 10 ms at a time,
    /// as in the logs
    fn shared() -> Vec<Stats> {
        vec![
            stats(StreamKind::Input, 13_000, 0),
            stats(StreamKind::Left, 10_100, 0),
            stats(StreamKind::Right, 10_000, 0),
        ]
    }

    #[test]
    fn covers_the_oldest_input_gets() {
        // The oldest the input gets (13 ms) + the slowest speaker (10.1 ms) + the step + the
        // margin
        assert_eq!(needed_us(&shared(), 700), Some(24_800));
    }

    #[test]
    fn packets_coming_in_pairs_need_more() {
        let mut s = shared();
        s[0].age_max_us = Some(20_800);
        assert_eq!(needed_us(&s, 700), Some(32_600));
    }

    #[test]
    fn nothing_is_known_without_input() {
        let mut s = shared();
        s[0].callbacks = 0;
        assert_eq!(needed_us(&s, 700), None);
    }

    #[test]
    fn rises_at_once_and_comes_down_after_a_minute() {
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        let mut auto = AutoLatency::new(t0, 700);
        assert_eq!(auto.update(at(1), &shared()), None, "still settling");
        assert_eq!(auto.update(at(3), &shared()), Some(24_800));

        let mut late = shared();
        late[0].age_max_us = Some(20_800);
        assert_eq!(auto.update(at(4), &late), Some(32_600));
        assert_eq!(
            auto.update(at(30), &shared()),
            None,
            "the late one still counts"
        );
        assert_eq!(auto.update(at(65), &shared()), Some(24_800));
    }

    #[test]
    fn running_dry_adds_a_little_for_a_while() {
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        let mut auto = AutoLatency::new(t0, 700);
        assert_eq!(auto.update(at(3), &shared()), Some(24_800));

        let mut dry = shared();
        dry[2].dry = 1;
        assert_eq!(auto.update(at(4), &dry), Some(25_800));
        assert_eq!(auto.update(at(10), &shared()), None);
        assert_eq!(auto.update(at(64), &shared()), Some(24_800));
    }
}
