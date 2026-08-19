// TCP client: sends requests to daemon, formats output
//
// Implements connect_or_start() which auto-starts the daemon if needed,
// and provides request/response communication over newline-delimited JSON.

use std::process::Command as StdCommand;
use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpStream;

use crate::daemon::protocol::{Request, Response};
use crate::error::BkError;

const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(30);
const DAEMON_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const DAEMON_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const DAEMON_DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DAEMON_MAX_REQUEST_TIMEOUT: Duration = Duration::from_secs(600);
const DAEMON_REQUEST_GRACE: Duration = Duration::from_secs(5);

/// A connected client to the daemon.
pub struct DaemonClient {
    reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: BufWriter<tokio::net::tcp::OwnedWriteHalf>,
}

impl DaemonClient {
    /// Connect to the daemon, auto-starting it if necessary.
    ///
    /// 1. Read `~/.bk/daemon.port` and try to connect + ping
    /// 2. If that fails, spawn the daemon as a background process
    /// 3. Poll until a connected, ping-verified client is ready and reuse it
    pub async fn connect_or_start() -> Result<Self, BkError> {
        // Try connecting to existing daemon
        if let Ok(client) = Self::try_connect().await {
            return Ok(client);
        }

        // Start daemon in background
        Self::start_daemon_background()?;

        // Persisted metadata is loaded before the daemon advertises its port;
        // browser reconnection continues in the background after readiness.
        Self::wait_for_daemon_ready(DAEMON_START_TIMEOUT).await
    }

    /// Connect to an already-running daemon without auto-starting one.
    ///
    /// Returns `Ok(client)` if a healthy daemon is reachable, or an error if
    /// no daemon is running. Used by `daemon stop` and `daemon status` to
    /// avoid spawning a new daemon just to query/stop it.
    pub async fn connect_only() -> Result<Self, BkError> {
        Self::try_connect().await
    }

    /// Try to connect to the daemon using the port from the port file.
    async fn try_connect() -> Result<Self, BkError> {
        Self::try_connect_from_port_file(&crate::daemon::port_file_path()).await
    }

    async fn try_connect_from_port_file(path: &std::path::Path) -> Result<Self, BkError> {
        let port = std::fs::read_to_string(path)
            .ok()
            .and_then(|value| value.trim().parse().ok())
            .ok_or_else(|| BkError::Other("no daemon port file found".into()))?;

        Self::connect_to_port(port).await
    }

    pub(crate) async fn connect_to_port(port: u16) -> Result<Self, BkError> {
        let stream = tokio::time::timeout(
            DAEMON_CONNECT_TIMEOUT,
            TcpStream::connect(format!("127.0.0.1:{port}")),
        )
        .await
        .map_err(|_| BkError::Other(format!("daemon connect timed out on port {port}")))?
        .map_err(|e| BkError::Other(format!("cannot connect to daemon on port {port}: {e}")))?;

        let (read_half, write_half) = stream.into_split();
        let mut client = Self {
            reader: BufReader::new(read_half),
            writer: BufWriter::new(write_half),
        };

        // Verify with a ping
        let resp = client
            .send_request_with_timeout(
                &Request {
                    cmd: "ping".into(),
                    params: json!({}),
                },
                DAEMON_HANDSHAKE_TIMEOUT,
            )
            .await?;

        if !resp.ok {
            return Err(BkError::Other("daemon ping failed".into()));
        }

        Ok(client)
    }

    /// Start the daemon as a background process.
    fn start_daemon_background() -> Result<(), BkError> {
        validate_daemon_config_before_spawn(crate::config::load_config)?;
        let exe = std::env::current_exe()
            .map_err(|e| BkError::Other(format!("cannot find current executable: {}", e)))?;

        // Spawn `bk daemon start` as a detached process
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            const DETACHED_PROCESS: u32 = 0x00000008;
            StdCommand::new(&exe)
                .args(["daemon", "start"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
                .spawn()
                .map_err(|e| BkError::Other(format!("failed to start daemon: {}", e)))?;
        }

        #[cfg(not(windows))]
        {
            StdCommand::new(&exe)
                .args(["daemon", "start"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .map_err(|e| BkError::Other(format!("failed to start daemon: {}", e)))?;
        }

        Ok(())
    }

    /// Poll the daemon until it responds to ping, returning that live connection.
    async fn wait_for_daemon_ready(timeout: Duration) -> Result<Self, BkError> {
        Self::wait_for_daemon_ready_at(timeout, &crate::daemon::port_file_path()).await
    }

    async fn wait_for_daemon_ready_at(
        timeout: Duration,
        port_file: &std::path::Path,
    ) -> Result<Self, BkError> {
        let deadline = tokio::time::Instant::now() + timeout;
        let poll_interval = Duration::from_millis(100);

        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err(BkError::Other("timeout waiting for daemon to start".into()));
            }

            let remaining = deadline.saturating_duration_since(now);
            match tokio::time::timeout(remaining, Self::try_connect_from_port_file(port_file)).await
            {
                Ok(Ok(client)) => return Ok(client),
                Err(_) => return Err(BkError::Other("timeout waiting for daemon to start".into())),
                Ok(Err(_)) => {}
            }

            tokio::time::sleep(
                poll_interval.min(deadline.saturating_duration_since(tokio::time::Instant::now())),
            )
            .await;
        }
    }

    /// Send a request and receive a single response.
    pub async fn send_request(&mut self, req: &Request) -> Result<Response, BkError> {
        self.send_request_with_timeout(req, request_timeout(req))
            .await
    }

    async fn send_request_with_timeout(
        &mut self,
        req: &Request,
        timeout: Duration,
    ) -> Result<Response, BkError> {
        tokio::time::timeout(timeout, self.send_request_inner(req))
            .await
            .map_err(|_| {
                BkError::Other(format!(
                    "daemon request '{}' timed out after {}ms",
                    req.cmd,
                    timeout.as_millis()
                ))
            })?
    }

    async fn send_request_inner(&mut self, req: &Request) -> Result<Response, BkError> {
        let json = serde_json::to_string(req)
            .map_err(|e| BkError::Other(format!("failed to serialize request: {}", e)))?;

        self.writer
            .write_all(json.as_bytes())
            .await
            .map_err(BkError::Io)?;
        self.writer.write_all(b"\n").await.map_err(BkError::Io)?;
        self.writer.flush().await.map_err(BkError::Io)?;

        let mut line = String::new();
        let n = self
            .reader
            .read_line(&mut line)
            .await
            .map_err(BkError::Io)?;

        if n == 0 {
            return Err(BkError::Other("daemon closed connection".into()));
        }

        let resp: Response = serde_json::from_str(line.trim())
            .map_err(|e| BkError::Other(format!("invalid response from daemon: {}", e)))?;

        Ok(resp)
    }
}

fn validate_daemon_config_before_spawn(
    load: impl FnOnce() -> Result<crate::config::Config, BkError>,
) -> Result<(), BkError> {
    load().map(drop)
}

fn request_timeout(req: &Request) -> Duration {
    let requested = req
        .params
        .get("timeout")
        .and_then(serde_json::Value::as_u64)
        .map(Duration::from_millis)
        .unwrap_or(DAEMON_DEFAULT_REQUEST_TIMEOUT);
    requested
        .saturating_add(DAEMON_REQUEST_GRACE)
        .max(DAEMON_DEFAULT_REQUEST_TIMEOUT)
        .min(DAEMON_MAX_REQUEST_TIMEOUT)
}

/// Build a daemon [`Request`] from a command name and params.
pub fn build_request(cmd: &str, params: serde_json::Value) -> Request {
    Request {
        cmd: cmd.to_string(),
        params,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn isolated_port_file(port: Option<u16>) -> (tempfile::TempDir, std::path::PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("daemon.port");
        if let Some(port) = port {
            std::fs::write(&path, port.to_string()).unwrap();
        }
        (temp, path)
    }

    #[test]
    fn daemon_start_timeout_allows_metadata_restore_window() {
        assert!(DAEMON_START_TIMEOUT >= Duration::from_secs(20));
    }

    #[test]
    fn daemon_start_preflight_propagates_invalid_config() {
        let error = validate_daemon_config_before_spawn(|| {
            Err(BkError::Other("invalid config: test".into()))
        })
        .unwrap_err();

        assert!(error.to_string().contains("invalid config"));
    }

    #[test]
    fn build_request_creates_correct_request() {
        let req = build_request("ping", json!({}));
        assert_eq!(req.cmd, "ping");
        assert_eq!(req.params, json!({}));
    }

    #[test]
    fn build_request_with_params() {
        let req = build_request("session.list", json!({"verbose": true}));
        assert_eq!(req.cmd, "session.list");
        assert_eq!(req.params["verbose"], true);
    }

    #[test]
    fn build_request_with_nested_params() {
        let req = build_request(
            "open",
            json!({"session": "agent", "url": "https://example.com"}),
        );
        assert_eq!(req.cmd, "open");
        assert_eq!(req.params["session"], "agent");
        assert_eq!(req.params["url"], "https://example.com");
    }

    #[tokio::test]
    async fn daemon_client_send_request_to_real_server() {
        // Start a mini TCP server that echoes a ping response
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            let mut writer = BufWriter::new(write_half);

            // Read request
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let _req: Request = serde_json::from_str(line.trim()).unwrap();

            // Write response
            let resp = Response::ok(json!({"status": "running"}));
            let resp_json = serde_json::to_string(&resp).unwrap();
            writer.write_all(resp_json.as_bytes()).await.unwrap();
            writer.write_all(b"\n").await.unwrap();
            writer.flush().await.unwrap();
        });

        // Connect client directly (bypass port file)
        let stream = TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        let (read_half, write_half) = stream.into_split();
        let mut client = DaemonClient {
            reader: BufReader::new(read_half),
            writer: BufWriter::new(write_half),
        };

        let resp = client
            .send_request(&Request {
                cmd: "ping".into(),
                params: json!({}),
            })
            .await
            .unwrap();

        assert!(resp.ok);
        assert_eq!(resp.data.unwrap()["status"], "running");

        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn send_request_times_out_when_daemon_stalls() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server_task = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(2)).await;
        });

        let stream = TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        let (read_half, write_half) = stream.into_split();
        let mut client = DaemonClient {
            reader: BufReader::new(read_half),
            writer: BufWriter::new(write_half),
        };

        let error = client
            .send_request_with_timeout(
                &Request {
                    cmd: "ping".into(),
                    params: json!({}),
                },
                Duration::from_millis(50),
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("timed out"));
        server_task.abort();
    }

    #[tokio::test]
    async fn connect_only_returns_error_when_no_daemon() {
        let (_temp, port_file) = isolated_port_file(None);
        let result = DaemonClient::try_connect_from_port_file(&port_file).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn connect_only_succeeds_when_daemon_is_running() {
        // Start a mini TCP server that echoes a ping response (simulates daemon)
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            let mut writer = BufWriter::new(write_half);

            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();

            let resp = Response::ok(json!({"status": "running"}));
            let resp_json = serde_json::to_string(&resp).unwrap();
            writer.write_all(resp_json.as_bytes()).await.unwrap();
            writer.write_all(b"\n").await.unwrap();
            writer.flush().await.unwrap();
        });

        let (_temp, port_file) = isolated_port_file(Some(port));

        let result = DaemonClient::try_connect_from_port_file(&port_file).await;
        assert!(
            result.is_ok(),
            "connect_only should succeed when daemon is reachable"
        );

        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn readiness_returns_the_pinged_connection_for_reuse() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (_temp, port_file) = isolated_port_file(Some(port));

        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            let mut writer = BufWriter::new(write_half);

            for expected_command in ["ping", "daemon.status"] {
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let request: Request = serde_json::from_str(line.trim()).unwrap();
                assert_eq!(request.cmd, expected_command);

                let response = Response::ok(json!({"command": expected_command}));
                writer
                    .write_all(serde_json::to_string(&response).unwrap().as_bytes())
                    .await
                    .unwrap();
                writer.write_all(b"\n").await.unwrap();
                writer.flush().await.unwrap();
            }
        });

        let mut client = DaemonClient::wait_for_daemon_ready_at(Duration::from_secs(2), &port_file)
            .await
            .unwrap();
        let response = client
            .send_request(&Request {
                cmd: "daemon.status".into(),
                params: json!({}),
            })
            .await
            .unwrap();

        assert!(response.ok);
        assert_eq!(response.data.unwrap()["command"], "daemon.status");
        tokio::time::timeout(Duration::from_secs(2), server_task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn readiness_deadline_includes_a_stalled_ping() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (_temp, port_file) = isolated_port_file(Some(port));
        let server_task = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(2)).await;
        });

        let result =
            DaemonClient::wait_for_daemon_ready_at(Duration::from_millis(100), &port_file).await;

        let error = match result {
            Ok(_) => panic!("stalled ping unexpectedly became ready"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("timeout waiting"));
        server_task.abort();
    }
}
