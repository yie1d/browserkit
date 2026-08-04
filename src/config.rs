// Configuration: ~/.bk/config.toml support
//
// All fields have sensible defaults. The config file is optional —
// if missing or partially filled, defaults are used for omitted fields.

use std::path::PathBuf;

use serde::Deserialize;
use std::path::Path;

use crate::error::BkError;

const MAX_CLEANUP_INTERVAL_SECONDS: u64 = 3_600;
const MAX_JS_TIMEOUT_SECONDS: u64 = 3_600;
const MAX_SESSIONS: usize = 1_000;
const MAX_TABS_PER_SESSION: usize = 1_000;
const MAX_SESSION_TIMEOUT_HOURS: u64 = 8_760;
/// Top-level configuration for browserkit.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Daemon-related settings.
    pub daemon: DaemonConfig,
    /// Resource limit settings.
    pub limits: LimitsConfig,
}

/// Daemon behavior configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    /// Cleanup check interval in seconds.
    pub cleanup_interval_seconds: u64,
}

/// Resource limits to prevent runaway usage.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsConfig {
    /// JavaScript execution timeout in seconds (0 = no timeout).
    pub js_timeout_seconds: u64,
    /// Maximum number of sessions allowed (0 = unlimited).
    pub max_sessions: usize,
    /// Maximum number of tabs per session (0 = unlimited).
    pub max_tabs_per_session: usize,
    /// Session inactivity timeout in hours before auto-cleanup.
    pub session_timeout_hours: u64,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            js_timeout_seconds: 0,
            max_sessions: 10,
            max_tabs_per_session: 5,
            session_timeout_hours: 72,
        }
    }
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            cleanup_interval_seconds: 60,
        }
    }
}

/// Load configuration from `~/.bk/config.toml`.
///
/// Uses defaults only when the file does not exist. Existing invalid files are fatal.
pub fn load_config() -> Result<Config, BkError> {
    load_config_from_path(&config_file_path())
}

fn load_config_from_path(path: &Path) -> Result<Config, BkError> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(error) => {
            return Err(BkError::Other(format!(
                "failed to read config '{}': {error}",
                path.display()
            )))
        }
    };
    let config = toml::from_str::<Config>(&content).map_err(|error| {
        BkError::Other(format!(
            "failed to parse config '{}': {error}",
            path.display()
        ))
    })?;
    validate_config(&config)?;
    tracing::info!(?path, "loaded config");
    Ok(config)
}

fn validate_config(config: &Config) -> Result<(), BkError> {
    validate_range(
        "daemon.cleanup_interval_seconds",
        config.daemon.cleanup_interval_seconds,
        1,
        MAX_CLEANUP_INTERVAL_SECONDS,
    )?;
    validate_range(
        "limits.js_timeout_seconds",
        config.limits.js_timeout_seconds,
        0,
        MAX_JS_TIMEOUT_SECONDS,
    )?;
    validate_range(
        "limits.max_sessions",
        config.limits.max_sessions,
        0,
        MAX_SESSIONS,
    )?;
    validate_range(
        "limits.max_tabs_per_session",
        config.limits.max_tabs_per_session,
        0,
        MAX_TABS_PER_SESSION,
    )?;
    validate_range(
        "limits.session_timeout_hours",
        config.limits.session_timeout_hours,
        0,
        MAX_SESSION_TIMEOUT_HOURS,
    )
}

fn validate_range<T>(name: &str, value: T, min: T, max: T) -> Result<(), BkError>
where
    T: Copy + PartialOrd + std::fmt::Display,
{
    if value < min || value > max {
        return Err(BkError::Other(format!(
            "invalid config: {name} must be between {min} and {max}, got {value}"
        )));
    }
    Ok(())
}

/// Path to the config file: `~/.bk/config.toml`.
pub fn config_file_path() -> PathBuf {
    crate::daemon::bk_home().join("config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_sensible_values() {
        let c = Config::default();
        assert_eq!(c.daemon.cleanup_interval_seconds, 60);
        assert_eq!(c.limits.js_timeout_seconds, 0);
        assert_eq!(c.limits.max_sessions, 10);
        assert_eq!(c.limits.max_tabs_per_session, 5);
        assert_eq!(c.limits.session_timeout_hours, 72);
    }

    #[test]
    fn parse_full_config() {
        let toml = r#"
[daemon]
cleanup_interval_seconds = 120

[limits]
js_timeout_seconds = 30
max_sessions = 12
max_tabs_per_session = 7
session_timeout_hours = 96
"#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.daemon.cleanup_interval_seconds, 120);
        assert_eq!(c.limits.js_timeout_seconds, 30);
        assert_eq!(c.limits.max_sessions, 12);
        assert_eq!(c.limits.max_tabs_per_session, 7);
        assert_eq!(c.limits.session_timeout_hours, 96);
    }

    #[test]
    fn parse_partial_config_uses_defaults() {
        let toml = r#"
[daemon]
cleanup_interval_seconds = 45
"#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.daemon.cleanup_interval_seconds, 45);
        assert_eq!(c.limits.max_sessions, 10); // default
    }

    #[test]
    fn parse_empty_config_uses_all_defaults() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.daemon.cleanup_interval_seconds, 60);
        assert_eq!(c.limits.max_sessions, 10);
    }

    #[test]
    fn parse_limits_config() {
        let toml = r#"
[limits]
max_sessions = 10
max_tabs_per_session = 5
session_timeout_hours = 72
"#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.limits.max_sessions, 10);
        assert_eq!(c.limits.max_tabs_per_session, 5);
        assert_eq!(c.limits.session_timeout_hours, 72);
    }

    #[test]
    fn parse_limits_custom_values() {
        let toml = r#"
[limits]
max_sessions = 20
max_tabs_per_session = 10
session_timeout_hours = 168
"#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.limits.max_sessions, 20);
        assert_eq!(c.limits.max_tabs_per_session, 10);
        assert_eq!(c.limits.session_timeout_hours, 168);
    }

    #[test]
    fn unknown_runtime_config_keys_are_rejected() {
        let old_timeout = ["work", "space", "timeout", "minutes"].join("_");
        let old_max_units = ["max", "work", "spaces"].join("_");
        let old_max_targets = ["max", "tabs", "per", "work", "space"].join("_");
        let toml = format!(
            r#"
[daemon]
{old_timeout} = 45
cleanup_interval_seconds = 30

[limits]
{old_max_units} = 5
{old_max_targets} = 10
max_sessions = 8
max_tabs_per_session = 3
session_timeout_hours = 48
"#
        );
        assert!(toml::from_str::<Config>(&toml).is_err());
    }

    #[test]
    fn existing_invalid_config_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[limits]\nmax_sessions = 'many'\n").unwrap();

        let error = load_config_from_path(&path).unwrap_err();

        assert!(error.to_string().contains("failed to parse config"));
    }

    #[test]
    fn missing_config_uses_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config = load_config_from_path(&dir.path().join("missing.toml")).unwrap();
        assert_eq!(config.limits.max_sessions, 10);
    }

    #[test]
    fn config_values_are_validated_without_clamping() {
        for content in [
            "[daemon]\ncleanup_interval_seconds = 0\n",
            "[daemon]\ncleanup_interval_seconds = 3601\n",
            "[limits]\njs_timeout_seconds = 3601\n",
            "[limits]\nmax_sessions = 1001\n",
            "[limits]\nmax_tabs_per_session = 1001\n",
            "[limits]\nsession_timeout_hours = 8761\n",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.toml");
            std::fs::write(&path, content).unwrap();
            assert!(load_config_from_path(&path).is_err(), "accepted: {content}");
        }
    }
}
