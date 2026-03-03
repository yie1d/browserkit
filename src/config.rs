// Configuration: ~/.bk/config.toml support
//
// All fields have sensible defaults. The config file is optional —
// if missing or partially filled, defaults are used for omitted fields.

use std::path::PathBuf;

use serde::Deserialize;

/// Top-level configuration for browserkit.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    /// Daemon-related settings.
    pub daemon: DaemonConfig,
    /// Resource limit settings.
    pub limits: LimitsConfig,
}

/// Daemon behavior configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    /// Workspace inactivity timeout in minutes before auto-cleanup.
    pub workspace_timeout_minutes: u64,
    /// Cleanup check interval in seconds.
    pub cleanup_interval_seconds: u64,
    /// Custom Chrome executable path (overrides auto-discovery).
    pub chrome_path: Option<String>,
    /// Whether to pass `--ignore-certificate-errors` and `--disable-web-security`
    /// to Chrome. Defaults to `true` for backward compatibility.
    pub disable_security: bool,
    /// Whether to launch Chrome in headless mode.
    /// Set to `false` to show the browser window. Defaults to `true`.
    pub headless: bool,
}

/// Resource limits to prevent runaway usage.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct LimitsConfig {
    /// Maximum number of workspaces allowed (0 = unlimited).
    pub max_workspaces: usize,
    /// Maximum number of tabs per workspace (0 = unlimited).
    pub max_tabs_per_workspace: usize,
    /// JavaScript execution timeout in seconds (0 = no timeout).
    pub js_timeout_seconds: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            workspace_timeout_minutes: 30,
            cleanup_interval_seconds: 60,
            chrome_path: None,
            disable_security: true,
            headless: true,
        }
    }
}

/// Load configuration from `~/.bk/config.toml`.
///
/// Returns default config if the file doesn't exist or can't be parsed.
/// Logs a warning on parse errors but never fails.
pub fn load_config() -> Config {
    let path = config_file_path();
    match std::fs::read_to_string(&path) {
        Ok(content) => match toml::from_str(&content) {
            Ok(config) => {
                tracing::info!(?path, "loaded config");
                config
            }
            Err(e) => {
                tracing::warn!(?path, %e, "failed to parse config, using defaults");
                Config::default()
            }
        },
        Err(_) => Config::default(),
    }
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
        assert_eq!(c.daemon.workspace_timeout_minutes, 30);
        assert_eq!(c.daemon.cleanup_interval_seconds, 60);
        assert!(c.daemon.chrome_path.is_none());
        assert!(c.daemon.disable_security); // default true for backward compat
        assert!(c.daemon.headless);         // default true        assert_eq!(c.limits.max_workspaces, 0);
        assert_eq!(c.limits.max_tabs_per_workspace, 0);
        assert_eq!(c.limits.js_timeout_seconds, 0);
    }

    #[test]
    fn parse_full_config() {
        let toml = r#"
[daemon]
workspace_timeout_minutes = 60
cleanup_interval_seconds = 120
chrome_path = "/usr/bin/chromium"
disable_security = false
headless = false

[limits]
max_workspaces = 10
max_tabs_per_workspace = 20
js_timeout_seconds = 30
"#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.daemon.workspace_timeout_minutes, 60);
        assert_eq!(c.daemon.cleanup_interval_seconds, 120);
        assert_eq!(c.daemon.chrome_path.as_deref(), Some("/usr/bin/chromium"));
        assert!(!c.daemon.disable_security);
        assert!(!c.daemon.headless);
        assert_eq!(c.limits.max_workspaces, 10);
        assert_eq!(c.limits.max_tabs_per_workspace, 20);
        assert_eq!(c.limits.js_timeout_seconds, 30);
    }

    #[test]
    fn parse_partial_config_uses_defaults() {
        let toml = r#"
[daemon]
workspace_timeout_minutes = 45
"#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.daemon.workspace_timeout_minutes, 45);
        assert_eq!(c.daemon.cleanup_interval_seconds, 60); // default
        assert_eq!(c.limits.max_workspaces, 0); // default
    }

    #[test]
    fn parse_empty_config_uses_all_defaults() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.daemon.workspace_timeout_minutes, 30);
        assert_eq!(c.limits.max_workspaces, 0);
    }

    #[test]
    fn load_config_returns_default_when_file_missing() {
        // load_config should not panic even if file doesn't exist
        let c = load_config();
        assert_eq!(c.daemon.workspace_timeout_minutes, 30);
    }
}
