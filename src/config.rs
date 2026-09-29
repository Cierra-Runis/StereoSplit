use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

/// Config file contents. Device names are matched by case-insensitive substring,
/// so part of the name is enough.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Where to capture sound from. A playback device (e.g. "CABLE Input") is captured via
    /// loopback, which does not trigger the microphone indicator; a recording device
    /// (e.g. the older default "CABLE Output") is also accepted.
    #[serde(default = "default_source")]
    pub source: String,

    /// Which playback device's Windows volume to follow (VB-CABLE's playback side is "CABLE Input")
    #[serde(default = "default_volume_endpoint")]
    pub volume_endpoint: String,

    /// Speaker for the left channel
    pub left: String,

    /// Speaker for the right channel
    pub right: String,

    /// Buffer latency in milliseconds. Raise it if you hear crackling, lower it for less delay.
    #[serde(default = "default_latency")]
    pub latency_ms: u32,

    /// Whether to follow the Windows volume keys
    #[serde(default = "default_true")]
    pub follow_windows_volume: bool,
}

fn default_source() -> String {
    "CABLE Input".into()
}
fn default_volume_endpoint() -> String {
    "CABLE Input".into()
}
fn default_latency() -> u32 {
    20
}
fn default_true() -> bool {
    true
}

const TEMPLATE: &str = r#"# Stereo Split config file
# Device names are matched by case-insensitive substring, so part of the name is enough.
# The full names of every device on this PC are listed in devices.txt in the same folder
# (refreshed on every start).
# After editing, right-click the tray icon and choose "Reload config" to apply.

# Speaker for the left channel (tip: rename both speakers in the Windows sound settings
# first so they are easy to tell apart)
left = "Speaker-Left"

# Speaker for the right channel
right = "Speaker-Right"

# Where to capture sound from: VB-CABLE's playback side (reads what it is playing directly,
# without opening any recording device)
source = "CABLE Input"

# Which device's Windows volume to follow: VB-CABLE's playback side (i.e. your default
# playback device)
volume_endpoint = "CABLE Input"

# Buffer latency in milliseconds. Raise to 50 if you hear crackling or dropouts;
# try 15 for less delay.
latency_ms = 20

# Whether the keyboard volume keys control both speakers
follow_windows_volume = true
"#;

pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn config_path() -> PathBuf {
    exe_dir().join("config.toml")
}

/// Load the config; if the file does not exist, write the template and return None.
pub fn load() -> Result<Option<Config>> {
    let path = config_path();
    if !path.exists() {
        std::fs::write(&path, TEMPLATE)
            .with_context(|| format!("Failed to create config file {}", path.display()))?;
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read config file {}", path.display()))?;
    let cfg: Config = toml::from_str(&text).context("Config file is malformed")?;
    Ok(Some(cfg))
}
