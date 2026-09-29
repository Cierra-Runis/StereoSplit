//! Audio supervisor: starts the engine, reconnects after a device goes away, and restarts with
//! the new config whenever config.toml changes (from the tray menu or edited by hand).
//!
//! What to do is decided by [`State`], which does no I/O, so it can be tested on its own;
//! [`spawn`] runs it on a thread and carries out what it decides.

use crate::config::{self, Config};
use crate::engine::{self, Engine, StreamKind};
use crate::{default_device, devices, toast, volume};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{error, warn};

/// How long to wait before retrying a failed start when no device change comes first.
/// Device changes retry right away; this covers failures they don't announce, such as a
/// speaker held by another app in exclusive mode.
const RETRY_FALLBACK: Duration = Duration::from_secs(30);
/// How long to wait before reconnecting after the audio is interrupted
const RECONNECT_DELAY: Duration = Duration::from_secs(2);
/// Quiet time that ends a burst of config saves or of device changes: one save often arrives
/// as several events (e.g. truncate, then write), and so does plugging in a device
const CONFIG_QUIET: Duration = Duration::from_millis(150);
const DEVICES_QUIET: Duration = Duration::from_millis(500);

/// What the supervisor thread waits for
pub enum Event {
    /// The program is exiting (Exit in the tray menu, Ctrl+C or the console closing)
    Quit,
    /// A running audio stream failed (e.g. a speaker was unplugged)
    Failed {
        /// Of the engine it came from, since a stopped engine can still report failures
        /// for a while
        generation: u64,
        stream: StreamKind,
    },
    /// config.toml was saved, from the tray menu or by hand
    ConfigChanged,
    /// An audio device was plugged in, unplugged, enabled or disabled
    DevicesChanged,
}

/// What the supervisor is doing
#[derive(Debug, Clone, PartialEq)]
enum State {
    /// Before the first start, and while a config change settles
    Starting,
    /// config.toml can't be loaded; waits for it to change
    BadConfig(String),
    /// The left or right speaker isn't chosen yet; waits for the config to change
    NoSpeakers,
    Running {
        cfg: Config,
        generation: u64,
    },
    /// The engine lost the device of its `lost` stream and is started again with the same
    /// config; `error` is why the last try failed
    Reconnecting {
        lost: StreamKind,
        error: Option<String>,
    },
    /// Failed to start; tries again when a device changes
    Failed(String),
}

impl State {
    /// Shown in the tray menu
    fn status(&self) -> &'static str {
        match self {
            State::Starting => "Starting",
            State::BadConfig(_) => "Config error",
            State::NoSpeakers => "Choose speakers",
            State::Running { .. } => "Running",
            State::Reconnecting { .. } => "Reconnecting",
            State::Failed(_) => "Failed to start (see log)",
        }
    }

    /// Take in how an attempt to start ended: `Running`, `BadConfig`, `NoSpeakers` or
    /// `Failed`. Returns how long to wait before trying again, or None to wait for a change.
    fn attempted(&mut self, new: State) -> Option<Duration> {
        *self = match (&*self, new) {
            // Failing to start the same config again is still reconnecting
            (State::Reconnecting { lost, .. }, State::Failed(e)) => State::Reconnecting {
                lost: *lost,
                error: Some(e),
            },
            (_, new) => new,
        };
        matches!(self, State::Reconnecting { .. } | State::Failed(_)).then_some(RETRY_FALLBACK)
    }

    /// Take in an event other than `Quit`. Returns how long to let things settle before
    /// trying to start again, or None to go on waiting as before.
    fn on_event(&mut self, ev: Event) -> Option<Duration> {
        match (&*self, ev) {
            (
                State::Running { generation, .. },
                Event::Failed {
                    generation: g,
                    stream,
                },
            ) if g == *generation => {
                *self = State::Reconnecting {
                    lost: stream,
                    error: None,
                };
                Some(RECONNECT_DELAY)
            }
            (_, Event::ConfigChanged) => {
                // The engine stops, and failing to start after this is no longer reconnecting
                if matches!(self, State::Running { .. } | State::Reconnecting { .. }) {
                    *self = State::Starting;
                }
                Some(CONFIG_QUIET)
            }
            (State::Reconnecting { .. } | State::Failed(_), Event::DevicesChanged) => {
                Some(DEVICES_QUIET)
            }
            // A running engine hears about its own devices going away, and failures from an
            // engine that was already stopped don't matter
            _ => None,
        }
    }

    /// Log the move from `old` to this state, and return the toast to show about it (title
    /// and text), if any. Only changes are reported, so retries that end the same way stay
    /// quiet.
    fn report(&self, old: &State) -> Option<(String, String)> {
        if self == old {
            return None;
        }
        match (old, self) {
            (_, State::BadConfig(e)) => {
                error!(error = %e, "failed to load the config");
                Some(("Config error".into(), e.clone()))
            }
            (_, State::NoSpeakers) => Some((
                "Choose speakers".into(),
                "Right-click the tray icon (the blue and orange dot) and choose your speakers \
                 under \"Left speaker\" and \"Right speaker\"."
                    .into(),
            )),
            (State::Running { cfg, .. }, State::Reconnecting { lost, .. }) => {
                warn!(stream = %lost, "audio interrupted, reconnecting");
                let (what, name) = match lost {
                    StreamKind::Input => ("Sound source", &cfg.source),
                    StreamKind::Left => ("Left speaker", &cfg.left),
                    StreamKind::Right => ("Right speaker", &cfg.right),
                };
                Some((
                    format!("{what} disconnected"),
                    format!("Lost \"{name}\". Reconnecting automatically once it's back."),
                ))
            }
            (_, State::Reconnecting { error: Some(e), .. } | State::Failed(e)) => {
                error!(error = %e, "failed to start the engine");
                // While reconnecting, the disconnect was shown already
                matches!(self, State::Failed(_)).then(|| {
                    (
                        "Failed to start".into(),
                        format!("{e}\n\nRetrying automatically when a device is plugged in."),
                    )
                })
            }
            _ => None,
        }
    }
}

/// Start the supervisor thread. It shows what it's doing in `status`, calls `on_change` when
/// config.toml or the audio devices change, and ends on [`Event::Quit`].
pub fn spawn(
    tx: Sender<Event>,
    rx: Receiver<Event>,
    status: Arc<Mutex<&'static str>>,
    on_change: impl Fn() + Send + 'static,
) {
    std::thread::spawn(move || {
        let _watcher = watch_config(tx.clone());
        devices::com_init();
        let devices_tx = tx.clone();
        let _devices = unsafe {
            devices::DeviceWatch::new(move || {
                let _ = devices_tx.send(Event::DevicesChanged);
            })
        }
        .inspect_err(|e| {
            warn!(
                error = %e,
                retry_in_secs = RETRY_FALLBACK.as_secs(),
                "can't watch for device changes; reconnecting only retries on a timer"
            )
        })
        .ok();

        let mut audio = Audio {
            tx,
            engine: None,
            volume: None,
            generation: 0,
            managed: false,
        };
        let mut state = State::Starting;
        // When to try starting again unless an event says otherwise; right away, to begin with
        let mut retry_at = Some(Instant::now());
        loop {
            let ev = match retry_at {
                Some(t) => rx.recv_timeout(t.saturating_duration_since(Instant::now())),
                None => rx.recv().map_err(RecvTimeoutError::from),
            };
            let old = state.clone();
            match ev {
                Ok(Event::Quit) | Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {
                    retry_at = state.attempted(audio.start()).map(|d| Instant::now() + d)
                }
                Ok(ev) => {
                    if matches!(ev, Event::ConfigChanged | Event::DevicesChanged) {
                        on_change();
                    }
                    if let Some(d) = state.on_event(ev) {
                        retry_at = Some(Instant::now() + d);
                    }
                }
            }
            if !matches!(state, State::Running { .. }) {
                // Stops the engine without waiting for it (see `engine::Engine`)
                audio.engine = None;
            }
            if let Some((title, text)) = state.report(&old) {
                toast::show(&title, &text);
            }
            *status.lock().unwrap() = state.status();
        }
    });
}

/// What the supervisor thread holds while it runs the engine
struct Audio {
    tx: Sender<Event>,
    engine: Option<Engine>,
    /// Follows the volume of `volume_endpoint`; replaced only when that setting changes
    volume: Option<volume::Follower>,
    /// Counts engine starts, so failures from an earlier engine can be told apart
    generation: u64,
    /// Whether this process has made the source the default playback device
    managed: bool,
}

impl Audio {
    /// Load the config and start the engine with it, making the source the default playback
    /// device only while the engine runs. Returns the state that ends in.
    fn start(&mut self) -> State {
        let cfg = match config::load() {
            Ok(c) => c,
            Err(e) => return State::BadConfig(format!("{e:#}")),
        };
        if cfg.left.is_empty() || cfg.right.is_empty() {
            self.release(&cfg);
            return State::NoSpeakers;
        }

        let gain = match &self.volume {
            Some(v) if v.pattern() == cfg.volume_endpoint => v.gain(),
            _ => self
                .volume
                .insert(volume::Follower::start(cfg.volume_endpoint.clone()))
                .gain(),
        };
        self.generation += 1;
        let generation = self.generation;
        let tx = self.tx.clone();
        let on_error: engine::OnError = Arc::new(move |stream| {
            let _ = tx.send(Event::Failed { generation, stream });
        });

        match Engine::start(&cfg, gain, on_error) {
            Ok(engine) => {
                self.engine = Some(engine);
                match default_device::take_over(&cfg) {
                    Ok(()) => self.managed = true,
                    Err(e) => error!(
                        error = %format_args!("{e:#}"),
                        "failed to switch the default playback device"
                    ),
                }
                State::Running { cfg, generation }
            }
            Err(e) => {
                // Nothing is playing through the source now, so let Windows play straight to
                // a speaker until the engine is back
                self.release(&cfg);
                State::Failed(format!("{e:#}"))
            }
        }
    }

    /// Switch the default playback device back if this process took it over
    fn release(&mut self, cfg: &Config) {
        if std::mem::take(&mut self.managed) {
            default_device::restore(cfg);
        }
    }
}

/// Report saves of config.toml. The folder is watched rather than the file, since some
/// editors save by writing a new file and renaming it over the old one.
fn watch_config(tx: Sender<Event>) -> Option<notify::RecommendedWatcher> {
    use notify::{EventKind, RecursiveMode, Watcher};
    let handler = move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else { return };
        let saved = matches!(
            ev.kind,
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
        ) && ev
            .paths
            .iter()
            .any(|p| p.file_name().is_some_and(|n| n == config::CONFIG_FILE));
        if saved {
            let _ = tx.send(Event::ConfigChanged);
        }
    };
    let watcher = notify::recommended_watcher(handler).and_then(|mut w| {
        w.watch(config::data_dir(), RecursiveMode::NonRecursive)?;
        Ok(w)
    });
    match watcher {
        Ok(w) => Some(w),
        Err(e) => {
            warn!(
                error = %e,
                "can't watch the config file; changes made by hand need a restart"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running() -> State {
        let cfg = Config {
            left: "Desk L".into(),
            right: "Desk R".into(),
            ..Config::default()
        };
        State::Running { cfg, generation: 1 }
    }

    /// Move `s` on with `f`, returning the title of the toast that shows, if any
    fn step(s: &mut State, f: impl FnOnce(&mut State) -> Option<Duration>) -> Option<String> {
        let old = s.clone();
        f(s);
        s.report(&old).map(|(title, _)| title)
    }

    #[test]
    fn reconnecting_stays_quiet_until_the_config_changes() {
        let mut s = running();
        let failed = |s: &mut State| s.attempted(State::Failed("not found".into()));
        let lost = step(&mut s, |s| {
            s.on_event(Event::Failed {
                generation: 1,
                stream: StreamKind::Left,
            })
        });
        assert_eq!(lost.as_deref(), Some("Left speaker disconnected"));
        for _ in 0..2 {
            assert_eq!(step(&mut s, failed), None);
            assert_eq!(s.status(), "Reconnecting");
        }
        s.on_event(Event::ConfigChanged);
        assert_eq!(step(&mut s, failed).as_deref(), Some("Failed to start"));
    }

    #[test]
    fn a_failure_shows_once_until_it_changes() {
        let mut s = State::Starting;
        let fail = |e: &str| {
            let e = e.to_string();
            move |s: &mut State| s.attempted(State::Failed(e))
        };
        assert!(step(&mut s, fail("a")).is_some());
        assert!(step(&mut s, fail("a")).is_none());
        assert!(step(&mut s, fail("b")).is_some());
    }

    #[test]
    fn failures_from_a_stopped_engine_are_ignored() {
        let mut s = running();
        let stale = Event::Failed {
            generation: 0,
            stream: StreamKind::Left,
        };
        assert_eq!(s.on_event(stale), None);
        assert_eq!(s, running());
    }

    #[test]
    fn device_changes_only_matter_while_retrying() {
        assert_eq!(running().on_event(Event::DevicesChanged), None);
        assert_eq!(State::NoSpeakers.on_event(Event::DevicesChanged), None);
        let mut s = State::Failed("x".into());
        assert_eq!(s.on_event(Event::DevicesChanged), Some(DEVICES_QUIET));
    }

    #[test]
    fn a_config_change_stops_the_engine() {
        let mut s = running();
        assert_eq!(s.on_event(Event::ConfigChanged), Some(CONFIG_QUIET));
        assert_eq!(s, State::Starting);
    }
}
