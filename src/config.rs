use std::{fs, path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub data_dir: PathBuf,
    pub config_file: PathBuf,
    pub state_dir: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Result<Self> {
        let dirs = ProjectDirs::from("dev", "tmux-recover", "tmux-recover")
            .context("could not determine the platform data directory")?;
        Ok(Self {
            data_dir: dirs.data_dir().to_path_buf(),
            config_file: dirs.config_dir().join("config.toml"),
            state_dir: dirs
                .state_dir()
                .unwrap_or_else(|| dirs.data_local_dir())
                .to_path_buf(),
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub autosave: AutosaveConfig,
    pub retention: RetentionConfig,
    pub restore: RestoreConfig,
    pub storage: StorageConfig,
}

impl Config {
    pub fn load(paths: &AppPaths) -> Result<Self> {
        if !paths.config_file.exists() {
            return Ok(Self::default());
        }
        let input = fs::read_to_string(&paths.config_file)
            .with_context(|| format!("failed to read {}", paths.config_file.display()))?;
        toml::from_str(&input)
            .with_context(|| format!("failed to parse {}", paths.config_file.display()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AutosaveConfig {
    #[serde(with = "duration_seconds")]
    pub debounce: Duration,
    #[serde(with = "duration_seconds")]
    pub min_interval: Duration,
    #[serde(with = "duration_seconds")]
    pub poll_interval: Duration,
}

impl Default for AutosaveConfig {
    fn default() -> Self {
        Self {
            debounce: Duration::from_secs(5),
            min_interval: Duration::from_secs(30),
            poll_interval: Duration::from_secs(60),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RetentionConfig {
    pub recent: usize,
    pub hourly_days: i64,
    pub daily_days: i64,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            recent: 100,
            hourly_days: 30,
            daily_days: 180,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RestoreConfig {
    pub auto: bool,
    pub auto_bootstrap_max_age_seconds: i64,
    pub process_restore: bool,
    pub process_allowlist: Vec<String>,
}

impl Default for RestoreConfig {
    fn default() -> Self {
        Self {
            auto: false,
            auto_bootstrap_max_age_seconds: 30,
            process_restore: false,
            process_allowlist: [
                "vi", "vim", "view", "nvim", "emacs", "man", "less", "more", "tail", "top", "htop",
                "irssi", "weechat", "mutt",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    pub zstd: bool,
}

mod duration_seconds {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(value.as_secs())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        u64::deserialize(deserializer).map(Duration::from_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_documented_cadence() {
        let config = Config::default();
        assert_eq!(config.autosave.debounce, Duration::from_secs(5));
        assert_eq!(config.autosave.min_interval, Duration::from_secs(30));
        assert_eq!(config.autosave.poll_interval, Duration::from_secs(60));
        assert_eq!(config.retention.recent, 100);
        assert!(!config.restore.auto);
    }
}
