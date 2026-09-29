use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;
use std::time::SystemTime;

/// Config file contents. Device names are matched by case-insensitive substring,
/// so part of the name is enough.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Speaker for the left channel (empty until chosen in the tray menu)
    pub left: String,

    /// Speaker for the right channel (empty until chosen in the tray menu)
    pub right: String,

    /// Buffer latency in milliseconds. Raise it if you hear crackling, lower it for less delay.
    pub latency_ms: u32,

    /// Where to capture sound from. A playback device (e.g. "CABLE Input") is captured via
    /// loopback, which does not trigger the microphone indicator; a recording device
    /// (e.g. the older default "CABLE Output") is also accepted.
    pub source: String,

    /// Which playback device's Windows volume to follow (VB-CABLE's playback side is "CABLE Input")
    pub volume_endpoint: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            left: String::new(),
            right: String::new(),
            latency_ms: 20,
            source: "CABLE Input".into(),
            volume_endpoint: "CABLE Input".into(),
        }
    }
}

/// Config file text, with a comment above every setting for anyone editing it by hand
fn render(cfg: &Config) -> String {
    // Let toml do the quoting and escaping
    let q = |s: &str| toml::Value::String(s.into()).to_string();
    format!(
        r#"# Stereo Split config file
# Everything here can also be changed from the tray menu. Changes made in this file take
# effect as soon as it is saved.
# Device names are matched by case-insensitive substring, so part of the name is enough.

# Speaker for the left channel
left = {left}

# Speaker for the right channel
right = {right}

# Buffer latency in milliseconds. Raise to 50 if you hear crackling or dropouts;
# try 15 for less delay.
latency_ms = {latency}

# Where to capture sound from: VB-CABLE's playback side (reads what it is playing directly,
# without opening any recording device)
source = {source}

# Which device's Windows volume to follow: VB-CABLE's playback side
volume_endpoint = {volume}
"#,
        left = q(&cfg.left),
        right = q(&cfg.right),
        latency = cfg.latency_ms,
        source = q(&cfg.source),
        volume = q(&cfg.volume_endpoint),
    )
}

pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn config_path() -> PathBuf {
    exe_dir().join("config.toml")
}

/// Last modification time of the config file, used to pick up changes automatically
pub fn modified() -> Option<SystemTime> {
    std::fs::metadata(config_path())
        .and_then(|m| m.modified())
        .ok()
}

/// Load the config; if the file does not exist, write one with the defaults.
pub fn load() -> Result<Config> {
    let path = config_path();
    if !path.exists() {
        let cfg = Config::default();
        save(&cfg)?;
        return Ok(cfg);
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read config file {}", path.display()))?;
    toml::from_str(&text).context("Config file is malformed")
}

pub fn save(cfg: &Config) -> Result<()> {
    let path = config_path();
    std::fs::write(&path, render(cfg))
        .with_context(|| format!("Failed to write config file {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_round_trips() {
        let cfg = Config {
            left: r#"Speakers "L" (USB\Audio)"#.into(),
            right: "Speaker-Right".into(),
            latency_ms: 35,
            source: "CABLE Input".into(),
            volume_endpoint: "CABLE In".into(),
        };
        let parsed: Config = toml::from_str(&render(&cfg)).unwrap();
        assert_eq!(parsed, cfg);
    }

    #[test]
    fn missing_fields_use_defaults() {
        let parsed: Config = toml::from_str("").unwrap();
        assert_eq!(parsed, Config::default());
    }
}
