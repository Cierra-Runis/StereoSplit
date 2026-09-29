//! Logging: one file per run in the logs folder, named by the UTC start time in ISO 8601
//! basic format (e.g. `20260929T160043Z.log`), since ':' is not allowed in Windows file names.
//! Lines inside carry the local time with its offset.

use crate::config;
use std::fs::OpenOptions;
use std::sync::Mutex;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::fmt::time::ChronoLocal;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, EnvFilter};

/// Local time with offset, always to the microsecond so every line's timestamp has the same
/// width (`rfc_3339()` drops trailing zeros), e.g. `2026-09-30T01:00:43.794373+09:00`
const TIME_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.6f%:z";

fn timer() -> ChronoLocal {
    ChronoLocal::new(TIME_FORMAT.into())
}

/// Log to a new file for this run. Failures are ignored: the program works the same without
/// a log.
pub fn init() {
    let name = chrono::Utc::now().format("%Y%m%dT%H%M%SZ.log").to_string();
    let path = config::log_dir().join(name);
    let opened = std::fs::create_dir_all(config::log_dir())
        .and_then(|()| OpenOptions::new().create(true).append(true).open(&path))
        .ok();

    // `File` is unbuffered, so every line is on disk before a panic aborts the process
    let file_layer = opened.map(|f| {
        fmt::layer()
            .with_writer(Mutex::new(f))
            .with_ansi(false)
            .with_timer(timer())
            .with_span_events(FmtSpan::CLOSE)
    });
    // Debug builds have a console
    let stderr_layer = cfg!(debug_assertions).then(|| {
        fmt::layer()
            .with_writer(std::io::stderr)
            .with_timer(timer())
            .with_span_events(FmtSpan::CLOSE)
    });
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn,stereo_split=info"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stderr_layer)
        .try_init();
}
