use anyhow::{Context, Result};
use serde::Deserialize;
use smart_default::SmartDefault;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use toml_edit::DocumentMut;
use toml_example::TomlExample;

/// Name of the data folder, both next to the exe (portable) and under %LOCALAPPDATA%
const APP_DIR: &str = "StereoSplit";
pub const CONFIG_FILE: &str = "config.toml";
/// Folder in the data folder holding one log file per run
pub const LOG_DIR: &str = "logs";
pub const RESTORE_FILE: &str = "restore-device.txt";

// The doc comments below are also the comments in a new config file (via `TomlExample`),
// so they are written for whoever edits that file by hand. Defaults come only from
// `#[default]`; `#[toml_example(default)]` would only change the example file.

/// Stereo Split config file
/// Everything here can also be changed from the tray menu. Changes made in this file take
/// effect as soon as it is saved.
/// Device names are matched by case-insensitive substring, so part of the name is enough.
///
#[derive(Debug, Clone, PartialEq, Deserialize, SmartDefault, TomlExample)]
#[serde(default)]
pub struct Config {
    /// Speaker for the left channel
    pub left: String,

    /// Speaker for the right channel
    pub right: String,

    /// Buffer latency in milliseconds. Raise to 50 if you hear crackling or dropouts;
    /// try 15 for less delay.
    #[default(20)]
    pub latency_ms: u32,

    /// Where to capture sound from: VB-CABLE's playback side (reads what it is playing directly,
    /// without opening any recording device). A recording device such as "CABLE Output"
    /// also works.
    #[default("CABLE Input")]
    pub source: String,

    /// Which device's Windows volume to follow: VB-CABLE's playback side
    #[default("CABLE Input")]
    pub volume_endpoint: String,
}

/// `text` with the values from `cfg` put in. Everything else in it (comments, order, layout)
/// is kept as is.
fn update(text: &str, cfg: &Config) -> Result<String> {
    let mut doc: DocumentMut = text.parse()?;
    let values: [(&str, toml_edit::Value); 5] = [
        ("left", cfg.left.as_str().into()),
        ("right", cfg.right.as_str().into()),
        ("latency_ms", i64::from(cfg.latency_ms).into()),
        ("source", cfg.source.as_str().into()),
        ("volume_endpoint", cfg.volume_endpoint.as_str().into()),
    ];
    for (key, mut value) in values {
        // Comments above a key belong to the key and stay on their own; a comment after the
        // value belongs to the value, so carry it over
        if let Some(old) = doc.get(key).and_then(|item| item.as_value()) {
            *value.decor_mut() = old.decor().clone();
        }
        doc[key] = toml_edit::Item::Value(value);
    }
    Ok(doc.to_string())
}

fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Release builds are portable when the data folder next to the exe has a config in it;
/// otherwise they keep their data under %LOCALAPPDATA%.
fn choose_dir(exe_dir: &Path, local_appdata: Option<PathBuf>) -> PathBuf {
    let portable = exe_dir.join(APP_DIR);
    if portable.join(CONFIG_FILE).exists() {
        return portable;
    }
    local_appdata.map_or(portable, |d| d.join(APP_DIR))
}

/// Folder holding the config file, the logs and the saved default device. Debug builds always
/// use the one next to the exe (in target/), so they never touch an installed copy's data.
pub fn data_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let exe = exe_dir();
        let dir = if cfg!(debug_assertions) {
            exe.join(APP_DIR)
        } else {
            choose_dir(&exe, std::env::var_os("LOCALAPPDATA").map(PathBuf::from))
        };
        // Errors show up later, when a file in it is written
        let _ = std::fs::create_dir_all(&dir);
        dir
    })
}

pub fn config_path() -> PathBuf {
    data_dir().join(CONFIG_FILE)
}

pub fn log_dir() -> PathBuf {
    data_dir().join(LOG_DIR)
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

/// Save the config, changing only the values in the existing file so anything added to it by
/// hand stays. A missing or unreadable file is written fresh, with the doc comments on
/// `Config` as its comments.
pub fn save(cfg: &Config) -> Result<()> {
    let path = config_path();
    let text = match std::fs::read_to_string(&path)
        .map_err(anyhow::Error::from)
        .and_then(|old| update(&old, cfg))
    {
        Ok(text) => text,
        Err(_) => update(&Config::toml_example(), cfg)?,
    };
    std::fs::write(&path, text)
        .with_context(|| format!("Failed to write config file {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn new_file_has_every_key_commented() {
        let cfg = Config {
            left: "Desk L".into(),
            latency_ms: 35,
            ..Config::default()
        };
        let out = update(&Config::toml_example(), &cfg).unwrap();
        let doc: DocumentMut = out.parse().unwrap();
        // A key missing from the example would be appended without a comment
        for (key, _) in doc.iter() {
            let key = doc.as_table().key(key).unwrap();
            let prefix = key.leaf_decor().prefix().and_then(|p| p.as_str());
            assert!(
                prefix.is_some_and(|p| p.contains('#')),
                "{key} has no comment"
            );
        }
        assert_eq!(toml::from_str::<Config>(&out).unwrap(), cfg);
    }

    #[test]
    fn update_keeps_user_edits() {
        let text = "# mine\nlatency_ms = 20 # was crackling at 15\n\n# note\nleft = \"x\"\n";
        let cfg = Config {
            latency_ms: 50,
            ..Config::default()
        };
        let out = update(text, &cfg).unwrap();
        assert!(
            out.starts_with(
                "# mine\nlatency_ms = 50 # was crackling at 15\n\n# note\nleft = \"\"\n"
            ),
            "{out}"
        );
        assert_eq!(toml::from_str::<Config>(&out).unwrap(), cfg);
    }

    #[test]
    fn missing_keys_use_defaults() {
        let parsed: Config = toml::from_str("latency_ms = 30").unwrap();
        let expected = Config {
            latency_ms: 30,
            ..Config::default()
        };
        assert_eq!(parsed, expected);
    }

    #[test]
    fn choose_dir_is_portable_only_with_config() {
        let tmp = tempfile::tempdir().unwrap();
        let (exe, appdata) = (tmp.path().join("exe"), tmp.path().join("appdata"));
        let portable = exe.join(APP_DIR);

        assert_eq!(
            choose_dir(&exe, Some(appdata.clone())),
            appdata.join(APP_DIR)
        );
        assert_eq!(choose_dir(&exe, None), portable);

        fs::create_dir_all(&portable).unwrap();
        fs::write(portable.join(CONFIG_FILE), "").unwrap();
        assert_eq!(choose_dir(&exe, Some(appdata)), portable);
    }
}
