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
    /// 3. Poll for readiness (up to 5 seconds)
    /// 4. Connect again
    pub async fn connect_or_start() -> Result<Self, BkError> {
        // Try connecting to existing daemon
        if let Ok(client) = Self::try_connect().await {
            return Ok(client);
        }

        // Start daemon in background
        Self::start_daemon_background()?;

        // Wait for daemon to become ready (poll ping for up to 5 seconds)
        Self::wait_for_daemon_ready(Duration::from_secs(5)).await?;

        // Connect again
        Self::try_connect().await
    }

    /// Try to connect to the daemon using the port from the port file.
    async fn try_connect() -> Result<Self, BkError> {
        let port = crate::daemon::read_port_file()
            .ok_or_else(|| BkError::Other("no daemon port file found".into()))?;

        let stream = TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .map_err(|e| BkError::Other(format!("cannot connect to daemon on port {}: {}", port, e)))?;

        let (read_half, write_half) = stream.into_split();
        let mut client = Self {
            reader: BufReader::new(read_half),
            writer: BufWriter::new(write_half),
        };

        // Verify with a ping
        let resp = client.send_request(&Request {
            cmd: "ping".into(),
            params: json!({}),
        }).await?;

        if !resp.ok {
            return Err(BkError::Other("daemon ping failed".into()));
        }

        Ok(client)
    }

    /// Start the daemon as a background process.
    fn start_daemon_background() -> Result<(), BkError> {
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

    /// Poll the daemon until it responds to ping, or timeout.
    ///
    /// First probes the TCP port with a lightweight connect (no ping) to avoid
    /// creating and discarding full connections on every poll iteration.
    /// Only establishes a full connection + ping once the port is open.
    async fn wait_for_daemon_ready(timeout: Duration) -> Result<(), BkError> {
        let start = tokio::time::Instant::now();
        let poll_interval = Duration::from_millis(100);

        loop {
            if start.elapsed() > timeout {
                return Err(BkError::Other(
                    "timeout waiting for daemon to start".into(),
                ));
            }

            // Lightweight probe: just check if the port is open yet
            let port = match crate::daemon::read_port_file() {
                Some(p) => p,
                None => {
                    tokio::time::sleep(poll_interval).await;
                    continue;
                }
            };

            if tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
                .await
                .is_ok()
            {
                // Port is open — now do a full connect + ping to confirm readiness
                if Self::try_connect().await.is_ok() {
                    return Ok(());
                }
            }

            tokio::time::sleep(poll_interval).await;
        }
    }

    /// Send a request and receive a single response.
    pub async fn send_request(&mut self, req: &Request) -> Result<Response, BkError> {
        let json = serde_json::to_string(req)
            .map_err(|e| BkError::Other(format!("failed to serialize request: {}", e)))?;

        self.writer
            .write_all(json.as_bytes())
            .await
            .map_err(BkError::Io)?;
        self.writer
            .write_all(b"\n")
            .await
            .map_err(BkError::Io)?;
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

    /// Read streaming responses (for network.monitor, cdp.events, etc.).
    ///
    /// Calls the provided callback for each response line until the connection
    /// closes or the callback returns `false`.
    pub async fn read_streaming<F>(&mut self, mut on_response: F) -> Result<(), BkError>
    where
        F: FnMut(Response) -> bool,
    {
        loop {
            let mut line = String::new();
            let n = self
                .reader
                .read_line(&mut line)
                .await
                .map_err(BkError::Io)?;

            if n == 0 {
                break; // EOF — daemon closed connection
            }

            match serde_json::from_str::<Response>(line.trim()) {
                Ok(resp) => {
                    if !on_response(resp) {
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "invalid streaming response line");
                }
            }
        }
        Ok(())
    }
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

    #[test]
    fn build_request_creates_correct_request() {
        let req = build_request("ping", json!({}));
        assert_eq!(req.cmd, "ping");
        assert_eq!(req.params, json!({}));
    }

    #[test]
    fn build_request_with_params() {
        let req = build_request("ws.new", json!({"label": "test"}));
        assert_eq!(req.cmd, "ws.new");
        assert_eq!(req.params["label"], "test");
    }

    #[test]
    fn build_request_with_nested_params() {
        let req = build_request("goto", json!({"wid": "a3f2", "url": "https://example.com"}));
        assert_eq!(req.cmd, "goto");
        assert_eq!(req.params["wid"], "a3f2");
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
    async fn daemon_client_streaming_responses() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            let mut writer = BufWriter::new(write_half);

            // Read the initial request
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();

            // Send 3 streaming responses then close
            for i in 0..3 {
                let resp = Response::ok(json!({"event": i}));
                let resp_json = serde_json::to_string(&resp).unwrap();
                writer.write_all(resp_json.as_bytes()).await.unwrap();
                writer.write_all(b"\n").await.unwrap();
                writer.flush().await.unwrap();
            }
            // Close connection (drop writer)
        });

        let stream = TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        let (read_half, write_half) = stream.into_split();
        let mut client = DaemonClient {
            reader: BufReader::new(read_half),
            writer: BufWriter::new(write_half),
        };

        // Send initial request
        client
            .send_request(&Request {
                cmd: "network.monitor".into(),
                params: json!({"wid": "test"}),
            })
            .await
            .ok(); // First response is consumed by send_request

        // Read remaining streaming responses
        let mut events = Vec::new();
        client
            .read_streaming(|resp| {
                events.push(resp);
                true
            })
            .await
            .unwrap();

        assert_eq!(events.len(), 2); // 2 remaining after first consumed by send_request

        server_task.await.unwrap();
    }
}
