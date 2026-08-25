use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use cdpkit::CDP;
use tempfile::TempDir;
use tokio::process::{Child, Command};
use tokio::time::{sleep, Instant};

use crate::browser::finder::{parse_devtools_active_port, BrowserFinder};
use crate::runtime::{BrowserError, CleanupFailure, ConfigurationFailure, ProxyOptions};

const DEFAULT_LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
/// Configuration for launching a browser owned by [`BrowserRuntime`](super::BrowserRuntime).
///
/// The executable is auto-discovered when omitted. A temporary private profile
/// is created when `user_data_dir` is omitted.
pub struct LaunchOptions {
    executable: Option<PathBuf>,
    user_data_dir: Option<PathBuf>,
    headless: bool,
    proxy: Option<ProxyOptions>,
    args: Vec<OsString>,
    timeout: Duration,
}

impl Default for LaunchOptions {
    fn default() -> Self {
        Self {
            executable: None,
            user_data_dir: None,
            headless: false,
            proxy: None,
            args: Vec::new(),
            timeout: DEFAULT_LAUNCH_TIMEOUT,
        }
    }
}

impl LaunchOptions {
    /// Uses an explicit Chromium-family executable instead of auto-discovery.
    pub fn executable(mut self, executable: impl Into<PathBuf>) -> Self {
        self.executable = Some(executable.into());
        self
    }

    /// Uses an explicit profile directory instead of a temporary one.
    pub fn user_data_dir(mut self, user_data_dir: impl Into<PathBuf>) -> Self {
        self.user_data_dir = Some(user_data_dir.into());
        self
    }

    /// Selects headed or Chromium's new headless mode.
    pub fn headless(mut self, headless: bool) -> Self {
        self.headless = headless;
        self
    }

    /// Configures the browser process proxy through typed launch arguments.
    pub fn proxy(mut self, proxy: ProxyOptions) -> Self {
        self.proxy = Some(proxy);
        self
    }

    /// Appends one browser process argument. Raw flags that can override typed
    /// headless or proxy configuration are always rejected before discovery or spawn.
    pub fn arg(mut self, argument: impl Into<OsString>) -> Self {
        self.args.push(argument.into());
        self
    }

    /// Sets the launch, endpoint discovery, and connection timeout.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn executable_path(&self) -> Option<&Path> {
        self.executable.as_deref()
    }

    pub fn user_data_dir_path(&self) -> Option<&Path> {
        self.user_data_dir.as_deref()
    }

    pub fn is_headless(&self) -> bool {
        self.headless
    }

    pub fn proxy_options(&self) -> Option<&ProxyOptions> {
        self.proxy.as_ref()
    }

    pub fn launch_timeout(&self) -> Duration {
        self.timeout
    }
}

#[derive(Debug)]
pub(crate) struct LaunchCommand {
    executable: PathBuf,
    args: Vec<OsString>,
}

impl LaunchCommand {
    pub(crate) fn build(
        executable: &Path,
        options: &LaunchOptions,
        user_data_dir: &Path,
    ) -> Result<Self, ConfigurationFailure> {
        validate_raw_arguments(options)?;

        let mut args = options.args.clone();
        if options.headless {
            args.push("--headless=new".into());
        }
        if let Some(proxy) = &options.proxy {
            args.push(format!("--proxy-server={}", proxy.server()).into());
            if !proxy.bypass_list().is_empty() {
                args.push(format!("--proxy-bypass-list={}", proxy.bypass_list().join(",")).into());
            }
        }
        args.extend([
            "--no-first-run".into(),
            "--no-default-browser-check".into(),
            "--remote-debugging-port=0".into(),
            format!("--user-data-dir={}", user_data_dir.display()).into(),
            "about:blank".into(),
        ]);
        Ok(Self {
            executable: executable.to_owned(),
            args,
        })
    }

    #[cfg(test)]
    fn has_arg(&self, expected: &str) -> bool {
        self.args
            .iter()
            .any(|argument| argument == std::ffi::OsStr::new(expected))
    }

    fn spawn(self) -> Result<Child, std::io::Error> {
        let mut command = Command::new(self.executable);
        command
            .args(self.args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        command.spawn()
    }
}

fn validate_raw_arguments(options: &LaunchOptions) -> Result<(), ConfigurationFailure> {
    if options
        .args
        .iter()
        .any(|argument| changes_typed_launch_truth(argument))
    {
        Err(ConfigurationFailure::ConflictingTypedLaunchArgument)
    } else {
        Ok(())
    }
}

fn changes_typed_launch_truth(argument: &std::ffi::OsStr) -> bool {
    let argument = argument.to_string_lossy();
    [
        "--headless",
        "--no-headless",
        "--proxy-server",
        "--proxy-bypass-list",
        "--no-proxy-server",
        "--proxy-pac-url",
        "--proxy-auto-detect",
    ]
    .iter()
    .any(|flag| argument == *flag || argument.starts_with(&format!("{flag}=")))
}

#[derive(Debug)]
pub(crate) struct LaunchedBrowser {
    pub(crate) child: Child,
    _temporary_profile: Option<TempDir>,
    pub(crate) profile_path: PathBuf,
}

pub(crate) async fn launch_browser(
    options: LaunchOptions,
) -> Result<(CDP, LaunchedBrowser), BrowserError> {
    validate_raw_arguments(&options)
        .map_err(|failure| BrowserError::configuration("launch browser", failure))?;

    let executable = match options.executable_path() {
        Some(path) => path.to_owned(),
        None => BrowserFinder::find().map_err(|error| {
            BrowserError::operation(
                "find browser executable",
                super::OperationPhase::Preparation,
            )
            .with_message(error.to_string())
        })?,
    };

    let (profile_path, temporary_profile) = match options.user_data_dir_path() {
        Some(path) => {
            std::fs::create_dir_all(path)?;
            (path.to_owned(), None)
        }
        None => {
            let directory = tempfile::Builder::new().prefix("browserkit-").tempdir()?;
            (directory.path().to_owned(), Some(directory))
        }
    };

    let command = LaunchCommand::build(&executable, &options, &profile_path)
        .map_err(|failure| BrowserError::configuration("launch browser", failure))?;
    let child = command.spawn()?;
    let mut launched = LaunchedBrowser {
        child,
        _temporary_profile: temporary_profile,
        profile_path: profile_path.clone(),
    };

    let endpoint =
        match wait_for_debug_endpoint(&mut launched.child, &profile_path, options.timeout).await {
            Ok(endpoint) => endpoint,
            Err(mut error) => {
                if let Err(cleanup) = terminate_child(&mut launched.child).await {
                    error = error.with_cleanup_failure(CleanupFailure::new(
                        "launched browser process",
                        cleanup.to_string(),
                    ));
                }
                return Err(error);
            }
        };

    let cdp = if endpoint.starts_with("ws://") {
        CDP::connect_ws_with_timeout(&endpoint, options.timeout).await
    } else {
        CDP::connect_with_timeout(&endpoint, options.timeout).await
    }
    .map_err(BrowserError::from);
    let cdp = match cdp {
        Ok(cdp) => cdp,
        Err(mut error) => {
            if let Err(cleanup) = terminate_child(&mut launched.child).await {
                error = error.with_cleanup_failure(CleanupFailure::new(
                    "launched browser process",
                    cleanup.to_string(),
                ));
            }
            return Err(error);
        }
    };

    Ok((cdp, launched))
}

async fn terminate_child(child: &mut Child) -> Result<(), std::io::Error> {
    match child.kill().await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => Ok(()),
        Err(error) => Err(error),
    }
}

async fn wait_for_debug_endpoint(
    child: &mut Child,
    profile_path: &Path,
    timeout: Duration,
) -> Result<String, BrowserError> {
    let port_file = profile_path.join("DevToolsActivePort");
    let deadline = Instant::now() + timeout;
    let mut last_parse_error = None;

    loop {
        if let Some(status) = child.try_wait()? {
            return Err(BrowserError::operation(
                "launch browser",
                super::OperationPhase::Preparation,
            )
            .with_message(format!(
                "browser exited before publishing DevToolsActivePort: {status}"
            )));
        }

        if port_file.exists() {
            match parse_devtools_active_port(&port_file) {
                Ok(info) => {
                    if info.ws_path.is_empty() {
                        return Ok(format!("127.0.0.1:{}", info.port));
                    }
                    return Ok(format!("ws://127.0.0.1:{}{}", info.port, info.ws_path));
                }
                Err(error) => last_parse_error = Some(error),
            }
        }

        if Instant::now() >= deadline {
            let detail = last_parse_error
                .map(|error| format!("; last parse error: {error}"))
                .unwrap_or_default();
            return Err(BrowserError::operation(
                "launch browser",
                super::OperationPhase::Preparation,
            )
            .with_message(format!(
                "timed out waiting for {}{}",
                port_file.display(),
                detail
            )));
        }
        sleep(Duration::from_millis(25)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn launch_uses_private_debug_endpoint_and_explicit_user_data_dir() {
        let command = LaunchCommand::build(
            Path::new("chrome.exe"),
            &LaunchOptions::default(),
            Path::new("C:/tmp/browserkit-profile"),
        )
        .unwrap();

        assert!(command.has_arg("--remote-debugging-port=0"));
        assert!(command.has_arg("--user-data-dir=C:/tmp/browserkit-profile"));
        assert!(command.has_arg("--no-first-run"));
        assert!(command.has_arg("--no-default-browser-check"));
    }

    #[test]
    fn launch_options_preserve_explicit_executable_profile_and_arguments() {
        let options = LaunchOptions::default()
            .executable("C:/Chrome/chrome.exe")
            .user_data_dir("C:/Profiles/browserkit")
            .headless(true)
            .arg("--disable-extensions");

        assert_eq!(
            options.executable_path(),
            Some(Path::new("C:/Chrome/chrome.exe"))
        );
        assert_eq!(
            options.user_data_dir_path(),
            Some(Path::new("C:/Profiles/browserkit"))
        );
        assert!(options.is_headless());

        let command = LaunchCommand::build(
            options.executable_path().unwrap(),
            &options,
            options.user_data_dir_path().unwrap(),
        )
        .unwrap();
        assert!(command.has_arg("--headless=new"));
        assert!(command.has_arg("--disable-extensions"));
    }

    #[test]
    fn typed_proxy_generates_canonical_launch_arguments() {
        let proxy = ProxyOptions::new("http://proxy.test:8080")
            .unwrap()
            .bypass(["localhost", "*.internal.test"])
            .unwrap();
        let options = LaunchOptions::default().proxy(proxy);
        let command = LaunchCommand::build(
            Path::new("chrome.exe"),
            &options,
            Path::new("C:/tmp/browserkit-profile"),
        )
        .unwrap();

        assert!(command.has_arg("--proxy-server=http://proxy.test:8080"));
        assert!(command.has_arg("--proxy-bypass-list=localhost,*.internal.test"));
    }

    fn raw_truth_changing_argument_cases() -> Vec<Vec<&'static str>> {
        vec![
            vec!["--headless"],
            vec!["--headless=old"],
            vec!["--no-headless"],
            vec!["--proxy-server=http://proxy.test:8080"],
            vec!["--proxy-server", "http://proxy.test:8080"],
            vec!["--proxy-bypass-list=localhost"],
            vec!["--proxy-bypass-list", "localhost"],
            vec!["--no-proxy-server"],
            vec!["--proxy-pac-url=http://proxy.test/proxy.pac"],
            vec!["--proxy-pac-url", "http://proxy.test/proxy.pac"],
            vec!["--proxy-auto-detect"],
            vec!["--proxy-auto-detect=true"],
        ]
    }

    #[tokio::test]
    async fn raw_truth_changing_arguments_are_rejected_before_discovery_or_spawn() {
        for arguments in raw_truth_changing_argument_cases() {
            let mut options =
                LaunchOptions::default().executable("Z:/browserkit-must-not-exist/chrome.exe");
            for argument in &arguments {
                options = options.arg(argument);
            }
            let error = match launch_browser(options).await {
                Err(error) => error,
                Ok(_) => panic!("raw typed-truth argument must fail preflight"),
            };
            assert_eq!(
                error.configuration_failure(),
                Some(&ConfigurationFailure::ConflictingTypedLaunchArgument),
                "arguments {arguments:?}"
            );
            assert_eq!(error.phase(), super::super::OperationPhase::Preparation);
            assert_eq!(
                error.action_completed(),
                super::super::ActionCompletion::NotStarted
            );
        }
    }

    #[test]
    fn raw_truth_changing_arguments_conflict_even_with_typed_options() {
        for arguments in raw_truth_changing_argument_cases() {
            let mut options = LaunchOptions::default()
                .headless(true)
                .proxy(ProxyOptions::new("http://proxy.test:8080").unwrap());
            for argument in &arguments {
                options = options.arg(argument);
            }
            assert_eq!(
                LaunchCommand::build(
                    Path::new("chrome.exe"),
                    &options,
                    Path::new("C:/tmp/browserkit-profile"),
                )
                .unwrap_err(),
                ConfigurationFailure::ConflictingTypedLaunchArgument,
                "arguments {arguments:?}"
            );
        }
    }

    #[tokio::test]
    async fn proxy_conflict_is_a_structured_preflight_error_before_spawn() {
        let options = LaunchOptions::default()
            .executable("Z:/browserkit-must-not-exist/chrome.exe")
            .proxy(ProxyOptions::new("http://proxy.test:8080").unwrap())
            .arg("--proxy-server=http://other.test:8080");

        let error = match launch_browser(options).await {
            Err(error) => error,
            Ok(_) => panic!("proxy conflict must fail before spawning a browser"),
        };
        assert_eq!(
            error.configuration_failure(),
            Some(&ConfigurationFailure::ConflictingTypedLaunchArgument)
        );
        assert_eq!(error.phase(), super::super::OperationPhase::Preparation);
        assert_eq!(
            error.action_completed(),
            super::super::ActionCompletion::NotStarted
        );
    }

    #[test]
    fn typed_proxy_rejects_raw_proxy_flag_conflicts() {
        for raw in [
            "--proxy-server=http://other.test:8080",
            "--proxy-bypass-list=localhost",
        ] {
            let options = LaunchOptions::default()
                .proxy(ProxyOptions::new("http://proxy.test:8080").unwrap())
                .arg(raw);
            let failure = LaunchCommand::build(
                Path::new("chrome.exe"),
                &options,
                Path::new("C:/tmp/browserkit-profile"),
            )
            .unwrap_err();
            assert_eq!(
                failure,
                ConfigurationFailure::ConflictingTypedLaunchArgument
            );
        }
    }
}
