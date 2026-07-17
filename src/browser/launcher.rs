// Chrome process launcher: starts Chrome with CDP debugging port
//
// Finds an available port in 9222..=9322, launches Chrome with remote
// debugging enabled, and waits up to 5 seconds for the CDP endpoint
// to accept TCP connections.

use std::process::{Child, Command};
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::time;

use crate::browser::finder::BrowserFinder;
use crate::daemon::bk_home;
use crate::error::BkError;

/// Result of a successful Chrome launch.
pub struct LaunchResult {
    /// The remote-debugging port Chrome is listening on.
    pub port: u16,
    /// Chrome process PID.
    pub pid: u32,
    /// Handle to the child process (caller owns the lifetime).
    pub child: Child,
}

/// Find an available port in the range 9222..=9322 by binding a `TcpListener`.
///
/// Returns the bound listener (still holding the port) along with the port number.
/// The caller must drop the listener only after Chrome has been spawned, to prevent
/// another process from stealing the port between detection and Chrome startup (TOCTOU).
fn find_available_port() -> Result<(std::net::TcpListener, u16), BkError> {
    for port in 9222..=9322 {
        if let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", port)) {
            return Ok((listener, port));
        }
    }
    Err(BkError::Other(
        "No available port in range 9222-9322".into(),
    ))
}

/// Launch Chrome and wait for the CDP endpoint to become reachable.
///
/// 1. Discovers the Chrome executable via `BrowserFinder::find()`.
/// 2. Picks an available port in 9222–9322, holding the `TcpListener` binding
///    until Chrome is spawned to prevent TOCTOU port-stealing.
/// 3. Spawns Chrome with `--remote-debugging-port`, `--user-data-dir`,
///    `--no-first-run`, and `--no-default-browser-check`.
///    If `disable_security` is true, also passes `--ignore-certificate-errors`
///    and `--disable-web-security`.
///    If `headless` is true, passes `--headless=new`; otherwise the browser
///    window is shown.
/// 4. Polls the TCP port for up to 5 seconds until CDP is ready.
pub async fn launch_chrome_with_config(
    disable_security: bool,
    headless: bool,
) -> Result<LaunchResult, BkError> {
    let chrome_path = BrowserFinder::find()?;
    let (port_holder, port) = find_available_port()?;

    let user_data_dir = bk_home().join(format!("chrome-{}", port));

    let mut cmd = Command::new(&chrome_path);
    cmd.arg(format!("--remote-debugging-port={}", port))
        .arg(format!("--user-data-dir={}", user_data_dir.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check");

    if disable_security {
        cmd.arg("--ignore-certificate-errors")
            .arg("--disable-web-security");
    }

    if headless {
        cmd.arg("--headless=new");
    }

    let child = cmd
        .spawn()
        .map_err(|e| BkError::Other(format!("Failed to launch Chrome: {}", e)))?;

    // Release the port holder now that Chrome has been spawned and will bind the port itself.
    drop(port_holder);

    let pid = child.id();

    // Wait up to 5 seconds for the CDP endpoint to accept connections.
    wait_for_cdp_ready(port, Duration::from_secs(5)).await?;

    tracing::info!(
        port = port,
        pid = pid,
        headless = headless,
        "Chrome launched"
    );

    Ok(LaunchResult { port, pid, child })
}

/// Launch Chrome with default settings (headless, security flags enabled).
pub async fn launch_chrome() -> Result<LaunchResult, BkError> {
    launch_chrome_with_config(true, true).await
}

/// Poll `127.0.0.1:{port}` until a TCP connection succeeds or `timeout` elapses.
async fn wait_for_cdp_ready(port: u16, timeout: Duration) -> Result<(), BkError> {
    let deadline = time::Instant::now() + timeout;
    loop {
        if time::Instant::now() >= deadline {
            return Err(BkError::BrowserStartupTimeout);
        }
        match TcpStream::connect(("127.0.0.1", port)).await {
            Ok(_) => return Ok(()),
            Err(_) => time::sleep(Duration::from_millis(100)).await,
        }
    }
}
