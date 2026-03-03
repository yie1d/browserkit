// Shared test infrastructure for integration tests
//
// DaemonFixture: starts an in-process daemon server on a random port.
// TestClient: sends requests over TCP and reads responses.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpStream;
use tokio::sync::{watch, RwLock};

use browserkit::daemon::protocol::{Request, Response};
use browserkit::daemon::server::DaemonServer;
use browserkit::daemon::state::DaemonState;

/// A running daemon server for use in tests.
pub struct DaemonFixture {
    pub port: u16,
}

impl DaemonFixture {
    /// Start a fresh daemon server and return the fixture.
    pub async fn start() -> Self {
        let state = Arc::new(RwLock::new(DaemonState::new()));
        let (tx, rx) = watch::channel(false);
        let server = DaemonServer::start(state, tx, rx)
            .await
            .expect("failed to start daemon server");
        Self { port: server.port }
    }

    /// Connect a raw TCP client to the daemon.
    pub async fn connect(&self) -> TestClient {
        let stream = TcpStream::connect(format!("127.0.0.1:{}", self.port))
            .await
            .expect("failed to connect to daemon");
        TestClient::new(stream)
    }

    /// Send a single request and return the response.
    pub async fn send(&self, cmd: &str, params: Value) -> Response {
        let mut client = self.connect().await;
        client.send(cmd, params).await
    }

    /// Stop the daemon gracefully.
    pub async fn stop(&self) {
        let _ = self.send("daemon.stop", json!({})).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A simple TCP client for sending requests to the daemon in tests.
pub struct TestClient {
    reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: BufWriter<tokio::net::tcp::OwnedWriteHalf>,
}

impl TestClient {
    pub fn new(stream: TcpStream) -> Self {
        let (r, w) = stream.into_split();
        Self {
            reader: BufReader::new(r),
            writer: BufWriter::new(w),
        }
    }

    /// Send a command and wait for a response.
    pub async fn send(&mut self, cmd: &str, params: Value) -> Response {
        let req = Request {
            cmd: cmd.to_string(),
            params,
        };
        let json_str = serde_json::to_string(&req).unwrap();
        self.writer.write_all(json_str.as_bytes()).await.unwrap();
        self.writer.write_all(b"\n").await.unwrap();
        self.writer.flush().await.unwrap();

        let mut line = String::new();
        self.reader.read_line(&mut line).await.unwrap();
        serde_json::from_str(line.trim()).expect("invalid response JSON")
    }
}
