use std::{fs, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
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
#[serde(default, deny_unknown_fields)]
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
        let config: Self = toml::from_str(&input)
            .with_context(|| format!("failed to parse {}", paths.config_file.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.autosave.debounce.is_zero()
            || self.autosave.min_interval.is_zero()
            || self.autosave.poll_interval.is_zero()
            || self.autosave.process_checkpoint_interval.is_zero()
        {
            bail!(
                "autosave debounce, min_interval, poll_interval, and process_checkpoint_interval must be greater than zero"
            );
        }
        // The checkpoint interval is compared against timestamps as a
        // `chrono::TimeDelta`, which has a narrower range than `Duration`.
        // Reject it here rather than letting the conversion fail at the point
        // where the daemon can only log and carry on.
        if let Err(error) = chrono::TimeDelta::from_std(self.autosave.process_checkpoint_interval) {
            bail!("autosave process_checkpoint_interval is too large: {error}");
        }
        if self.retention.hourly_days < 0 || self.retention.daily_days < 0 {
            bail!("retention hourly_days and daily_days must not be negative");
        }
        if self.retention.daily_days < self.retention.hourly_days {
            bail!("retention daily_days must be greater than or equal to hourly_days");
        }
        if self.restore.auto_bootstrap_max_age_seconds < 0 {
            bail!("restore auto_bootstrap_max_age_seconds must not be negative");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AutosaveConfig {
    #[serde(with = "duration_seconds")]
    pub debounce: Duration,
    #[serde(with = "duration_seconds")]
    pub min_interval: Duration,
    #[serde(with = "duration_seconds")]
    pub poll_interval: Duration,
    /// Minimum time between process checkpoint sidecar rewrites when
    /// structural state is unchanged. Process restore is already best-effort,
    /// so this does not need to be as tight as `poll_interval`; overwriting
    /// in place (rather than adding a history entry) is what makes a shorter
    /// interval affordable at all.
    #[serde(with = "duration_seconds")]
    pub process_checkpoint_interval: Duration,
    /// Indexed tmux hook slot used by the daemon. The persistent event command
    /// is installed atomically only when the slot is empty; an identical hook
    /// from an earlier daemon is reused and every other command is preserved.
    /// An occupied third-party slot disables hook events but polling continues.
    pub hook_slot: u16,
}

impl Default for AutosaveConfig {
    fn default() -> Self {
        Self {
            debounce: Duration::from_secs(5),
            min_interval: Duration::from_secs(30),
            poll_interval: Duration::from_secs(60),
            process_checkpoint_interval: Duration::from_secs(300),
            hook_slot: 901,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionConfig {
    pub recent: usize,
    pub hourly_days: i64,
    pub daily_days: i64,
    pub safety_snapshots: usize,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            recent: 100,
            hourly_days: 30,
            daily_days: 180,
            safety_snapshots: 10,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RestoreConfig {
    pub auto: bool,
    pub auto_bootstrap_max_age_seconds: i64,
    pub process_allowlist: Vec<String>,
}

impl RestoreConfig {
    pub fn processes_enabled(&self) -> bool {
        !self.process_allowlist.is_empty()
    }
}

impl Default for RestoreConfig {
    fn default() -> Self {
        Self {
            auto: false,
            auto_bootstrap_max_age_seconds: 30,
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
#[serde(default, deny_unknown_fields)]
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
        assert_eq!(
            config.autosave.process_checkpoint_interval,
            Duration::from_secs(300)
        );
        assert_eq!(config.autosave.hook_slot, 901);
        assert_eq!(config.retention.recent, 100);
        assert_eq!(config.retention.safety_snapshots, 10);
        assert!(!config.restore.auto);
        assert!(config.restore.processes_enabled());
        config.validate().unwrap();
    }

    #[test]
    fn an_empty_process_allowlist_disables_process_capture() {
        let mut config = RestoreConfig::default();
        config.process_allowlist.clear();
        assert!(!config.processes_enabled());
    }

    #[test]
    fn rejects_invalid_intervals() {
        let mut config = Config::default();
        config.autosave.poll_interval = Duration::ZERO;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_zero_process_checkpoint_interval() {
        let mut config = Config::default();
        config.autosave.process_checkpoint_interval = Duration::ZERO;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_a_daily_retention_window_shorter_than_hourly() {
        let mut config = Config::default();
        config.retention.hourly_days = 31;
        config.retention.daily_days = 30;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_a_process_checkpoint_interval_chrono_cannot_represent() {
        // Duration outruns chrono::TimeDelta, and the daemon compares the
        // interval as a TimeDelta. Caught here rather than degrading into a
        // zero interval that rewrites the sidecar on every tick.
        let mut config = Config::default();
        config.autosave.process_checkpoint_interval = Duration::MAX;
        let error = format!("{:#}", config.validate().unwrap_err());
        assert!(
            error.contains("process_checkpoint_interval is too large"),
            "{error}"
        );
    }
}
