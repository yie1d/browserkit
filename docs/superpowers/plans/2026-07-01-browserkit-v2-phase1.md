# browserkit v2 Phase 1 Implementation Plan

> **Status: COMPLETE** — All 14 tasks implemented. 592 lib tests + 47 bin tests passing as of 2026-07-02.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Rebuild browserkit CLI and daemon to be only-agent friendly: unified JSON output, session-based isolation, structured errors, and snapshot+act as the primary interaction model.

**Architecture:** The daemon retains its existing TCP server and CDP connection infrastructure. New session abstraction replaces workspace. CLI is rebuilt with new commands (setup/connect/snapshot/act/navigate/open/close/tabs/session). All output is JSON. Deprecated aliases emit stderr warnings.

**Tech Stack:** Rust, tokio, clap 4.x derive, cdpkit (path dep, local unreleased), serde_json, DashMap, parking_lot

## Global Constraints

- cdpkit = path dependency pointing to `D:\Program\cdp\cdpkit-rs\cdpkit` (local unreleased build that adds `CDP::closed().await`)
- In `Cargo.toml`: `cdpkit = { path = "../cdpkit-rs/cdpkit" }` (switch back to crates.io once cdpkit publishes new version)
- All CLI output is JSON (`{"ok":true,"data":{...}}` or `{"ok":false,"error":{...}}`)
- No --format flag, no text/tsv output
- Never connect to real Chrome in tests
- Commit messages: English, Conventional Commits, no AI attribution
- `cargo build` + `cargo test --lib` must pass after each task
- Existing workspace-based code is preserved in parallel (Phase 3 removes it)
- New handler files coexist with old ones; routing is additive

---

## Task 1: Structured Error System

**Goal:** Add `ErrorCode` enum and `ErrorDetail` struct so all v2 commands return machine-readable errors with code/message/suggestion/recoverable.

**Files to modify:**
- `src/error.rs` -- add `ErrorCode` enum with all codes from REDESIGN.md 7.2
- `src/daemon/protocol.rs` -- change `Response.error` from `Option<String>` to `Option<serde_json::Value>`; add `Response::error_detail()` constructor

### Steps

- [x] **1.1 Write tests**

  Add to `src/error.rs`:

  ```rust
  #[cfg(test)]
  mod error_code_tests {
      use super::*;

      #[test]
      fn error_code_serializes_as_screaming_snake() {
          assert_eq!(serde_json::to_string(&ErrorCode::NotConnected).unwrap(), "\"NOT_CONNECTED\"");
          assert_eq!(serde_json::to_string(&ErrorCode::RefNotFound).unwrap(), "\"REF_NOT_FOUND\"");
          assert_eq!(serde_json::to_string(&ErrorCode::SessionLimitExceeded).unwrap(), "\"SESSION_LIMIT_EXCEEDED\"");
      }

      #[test]
      fn error_code_deserializes_from_screaming_snake() {
          let code: ErrorCode = serde_json::from_str("\"BROWSER_NOT_RUNNING\"").unwrap();
          assert_eq!(code, ErrorCode::BrowserNotRunning);
      }

      #[test]
      fn error_code_suggestion_is_non_empty() {
          assert!(!ErrorCode::RefNotFound.suggestion().is_empty());
          assert!(ErrorCode::RefNotFound.suggestion().contains("snapshot"));
      }

      #[test]
      fn error_code_recoverable_classification() {
          assert!(ErrorCode::NotConnected.recoverable());
          assert!(ErrorCode::RefNotFound.recoverable());
          assert!(!ErrorCode::BrowserVersionTooOld.recoverable());
          assert!(!ErrorCode::TargetCrashed.recoverable());
          assert!(!ErrorCode::DaemonError.recoverable());
      }
  }
  ```

  Add to `src/daemon/protocol.rs` tests module:

  ```rust
  #[test]
  fn response_error_detail_has_correct_structure() {
      use crate::error::ErrorCode;
      let resp = Response::error_detail(ErrorCode::RefNotFound, "element ref 42 not found".into(), None);
      assert!(!resp.ok);
      let err = resp.error.unwrap();
      assert_eq!(err["code"], "REF_NOT_FOUND");
      assert!(err["message"].as_str().unwrap().contains("42"));
      assert!(err["suggestion"].as_str().unwrap().contains("snapshot"));
      assert_eq!(err["recoverable"], true);
  }

  #[test]
  fn response_error_detail_custom_suggestion() {
      use crate::error::ErrorCode;
      let resp = Response::error_detail(ErrorCode::Timeout, "timed out".into(), Some("try again with --timeout 60000".into()));
      let err = resp.error.unwrap();
      assert_eq!(err["suggestion"], "try again with --timeout 60000");
  }

  #[test]
  fn response_error_detail_json_wire_format() {
      use crate::error::ErrorCode;
      let resp = Response::error_detail(ErrorCode::NotConnected, "no connection".into(), None);
      let json = serde_json::to_value(&resp).unwrap();
      assert_eq!(json["ok"], false);
      assert_eq!(json["error"]["code"], "NOT_CONNECTED");
      assert_eq!(json["error"]["recoverable"], true);
  }

  #[test]
  fn response_legacy_err_unchanged() {
      let resp = Response::err("something broke");
      let json = serde_json::to_value(&resp).unwrap();
      assert_eq!(json["error"], "something broke");
  }
  ```

- [x] **1.2 Run tests (expect compile failure)**

  ```bash
  cargo test --lib error_code_tests 2>&1 | head -20
  cargo test --lib response_error_detail 2>&1 | head -20
  ```

- [x] **1.3 Implement ErrorCode enum in src/error.rs**

  Add after existing `BkError` enum (keep BkError unchanged):

  ```rust
  use serde::{Deserialize, Serialize};

  /// Machine-readable error codes for v2 structured responses.
  #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
  #[serde(rename_all = "SCREAMING_SNAKE_CASE")]
  pub enum ErrorCode {
      NotConnected,
      RefNotFound,
      RemoteDebugNotEnabled,
      ConnectionRefused,
      BrowserNotRunning,
      BrowserVersionTooOld,
      BrowserNotInstalled,
      ChromeDisconnected,
      SessionNotFound,
      SessionNoTab,
      DialogBlocking,
      NavigateFailed,
      Timeout,
      ElementNotVisible,
      ElementNotInteractable,
      TargetNotFound,
      TargetCrashed,
      JsError,
      InvalidArgument,
      DaemonError,
      FileNotFound,
      SelectorNotFound,
      SessionLimitExceeded,
      TabLimitExceeded,
      Unauthorized,
  }

  impl ErrorCode {
      pub fn suggestion(&self) -> &'static str {
          match self {
              Self::NotConnected => "run 'bk connect' first to establish a browser connection",
              Self::RefNotFound => "call snapshot to refresh refs -- page may have changed since last snapshot",
              Self::RemoteDebugNotEnabled => "open chrome://inspect/#remote-debugging and enable, then retry bk connect",
              Self::ConnectionRefused => "check if Chrome showed an authorization dialog and click Allow, then retry",
              Self::BrowserNotRunning => "manually open Chrome/Edge, then retry bk connect",
              Self::BrowserVersionTooOld => "upgrade Chrome/Edge to version 112 or later",
              Self::BrowserNotInstalled => "install Google Chrome from https://www.google.com/chrome",
              Self::ChromeDisconnected => "Chrome may have closed; run bk connect to reconnect",
              Self::SessionNotFound => "session may have expired or been closed; create a new one",
              Self::SessionNoTab => "use bk open to create a tab first",
              Self::DialogBlocking => "handle the dialog first: bk act dialog accept/dismiss",
              Self::NavigateFailed => "check URL is valid and accessible",
              Self::Timeout => "increase --timeout or check if page is responsive",
              Self::ElementNotVisible => "element may be hidden or overlapped; try scrolling or waiting",
              Self::ElementNotInteractable => "element is disabled; check page state",
              Self::TargetNotFound => "tab may have been closed; run bk tabs to see available tabs",
              Self::TargetCrashed => "tab has crashed and cannot recover",
              Self::JsError => "check expression syntax",
              Self::InvalidArgument => "check command syntax",
              Self::DaemonError => "restart daemon: bk daemon stop && bk daemon start",
              Self::FileNotFound => "check file path exists and is absolute",
              Self::SelectorNotFound => "selector matched no elements; check page state",
              Self::SessionLimitExceeded => "close unused sessions with 'bk session close --session <name>'",
              Self::TabLimitExceeded => "close unused tabs with 'bk close --target <tid>'",
              Self::Unauthorized => "daemon token mismatch; restart daemon or check ~/.bk/daemon.token",
          }
      }

      pub fn recoverable(&self) -> bool {
          !matches!(self, Self::BrowserVersionTooOld | Self::BrowserNotInstalled | Self::TargetCrashed | Self::DaemonError)
      }
  }
  ```

- [x] **1.4 Modify Response in src/daemon/protocol.rs**

  Change `error` field from `Option<String>` to `Option<serde_json::Value>`:

  ```rust
  #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
  pub struct Response {
      pub ok: bool,
      #[serde(skip_serializing_if = "Option::is_none")]
      pub data: Option<serde_json::Value>,
      #[serde(skip_serializing_if = "Option::is_none")]
      pub error: Option<serde_json::Value>,
  }

  impl Response {
      pub fn ok(data: serde_json::Value) -> Self {
          Self { ok: true, data: Some(data), error: None }
      }

      pub fn err(msg: impl Into<String>) -> Self {
          Self { ok: false, data: None, error: Some(serde_json::Value::String(msg.into())) }
      }

      pub fn error_detail(code: ErrorCode, message: String, suggestion: Option<String>) -> Self {
          let suggestion = suggestion.unwrap_or_else(|| code.suggestion().to_string());
          let recoverable = code.recoverable();
          Self {
              ok: false,
              data: None,
              error: Some(serde_json::json!({
                  "code": code,
                  "message": message,
                  "suggestion": suggestion,
                  "recoverable": recoverable,
              })),
          }
      }
  }
  ```

  Update existing tests: `resp.error` comparisons change from `Some("msg".into())` to `Some(serde_json::Value::String("msg".into()))`.

- [x] **1.5 Build and run all tests**

  ```bash
  cargo build
  cargo test --lib error_code_tests
  cargo test --lib response_error_detail
  cargo test --lib protocol::tests
  ```

- [x] **1.6 Commit**

  ```bash
  git add src/error.rs src/daemon/protocol.rs
  git commit -m "feat: add structured ErrorCode enum and error_detail Response constructor"
  ```

---

## Task 2: Daemon Token Authentication

**Goal:** Generate a random token on daemon start, write to `~/.bk/daemon.token` (0600 permissions), validate every TCP request against it.

**Files to modify:**
- `src/daemon/mod.rs` -- generate token on start, write token file
- `src/daemon/server.rs` -- validate `"token"` field in every request JSON
- `src/daemon/protocol.rs` -- add optional `token` field to `Request`
- `src/client.rs` -- read token file, inject into every request

### Steps

- [x] **2.1 Write tests**

  In `src/daemon/mod.rs` (or new `src/daemon/token.rs`):

  ```rust
  #[cfg(test)]
  mod token_tests {
      use super::*;

      #[test]
      fn generate_token_is_64_hex_chars() {
          let token = generate_daemon_token();
          assert_eq!(token.len(), 64);
          assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
      }

      #[test]
      fn generate_token_is_unique() {
          let t1 = generate_daemon_token();
          let t2 = generate_daemon_token();
          assert_ne!(t1, t2);
      }
  }
  ```

  In `src/daemon/server.rs` tests:

  ```rust
  #[tokio::test]
  async fn server_rejects_request_without_token() {
      let port = start_server_with_token("test-secret").await;
      let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).await.unwrap();
      let req = r#"{"cmd":"ping","params":{}}"#;
      stream.write_all(format!("{req}\n").as_bytes()).await.unwrap();
      let mut buf = vec![0u8; 4096];
      let n = stream.read(&mut buf).await.unwrap();
      let resp: Response = serde_json::from_str(std::str::from_utf8(&buf[..n]).unwrap().trim()).unwrap();
      assert!(!resp.ok);
      assert!(resp.error.unwrap().to_string().contains("UNAUTHORIZED"));
  }

  #[tokio::test]
  async fn server_accepts_request_with_valid_token() {
      let port = start_server_with_token("test-secret").await;
      let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).await.unwrap();
      let req = r#"{"cmd":"ping","params":{},"token":"test-secret"}"#;
      stream.write_all(format!("{req}\n").as_bytes()).await.unwrap();
      let mut buf = vec![0u8; 4096];
      let n = stream.read(&mut buf).await.unwrap();
      let resp: Response = serde_json::from_str(std::str::from_utf8(&buf[..n]).unwrap().trim()).unwrap();
      assert!(resp.ok);
  }

  #[tokio::test]
  async fn server_rejects_request_with_wrong_token() {
      let port = start_server_with_token("test-secret").await;
      let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).await.unwrap();
      let req = r#"{"cmd":"ping","params":{},"token":"wrong-token"}"#;
      stream.write_all(format!("{req}\n").as_bytes()).await.unwrap();
      let mut buf = vec![0u8; 4096];
      let n = stream.read(&mut buf).await.unwrap();
      let resp: Response = serde_json::from_str(std::str::from_utf8(&buf[..n]).unwrap().trim()).unwrap();
      assert!(!resp.ok);
  }
  ```

- [x] **2.2 Run tests (expect failure)**

  ```bash
  cargo test --lib token_tests 2>&1 | head -20
  cargo test --lib server_rejects_request_without_token 2>&1 | head -20
  ```

- [x] **2.3 Implement token generation**

  In `src/daemon/mod.rs` (or new file `src/daemon/token.rs` re-exported from mod.rs):

  ```rust
  use rand::Rng;
  use std::path::PathBuf;

  /// Generate a 64-character random hex token for daemon authentication.
  pub fn generate_daemon_token() -> String {
      let mut rng = rand::thread_rng();
      let bytes: [u8; 32] = rng.gen();
      hex::encode(bytes) // or format with {:02x} loop since we already have rand
  }

  /// Write the daemon token to ~/.bk/daemon.token with restrictive permissions.
  pub fn write_token_file(token: &str) -> std::io::Result<()> {
      let path = token_file_path();
      std::fs::write(&path, token)?;
      // On Unix, set 0600 permissions
      #[cfg(unix)]
      {
          use std::os::unix::fs::PermissionsExt;
          std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
      }
      Ok(())
  }

  /// Read the daemon token from ~/.bk/daemon.token.
  pub fn read_token_file() -> Option<String> {
      let path = token_file_path();
      std::fs::read_to_string(&path).ok().map(|s| s.trim().to_string())
  }

  /// Path to the token file: ~/.bk/daemon.token
  pub fn token_file_path() -> PathBuf {
      bk_home().join("daemon.token")
  }
  ```

  Note: Since we already use `rand` crate, format with iterator instead of adding `hex` crate:
  ```rust
  pub fn generate_daemon_token() -> String {
      let mut rng = rand::thread_rng();
      (0..32).map(|_| format!("{:02x}", rng.gen::<u8>())).collect()
  }
  ```

- [x] **2.4 Add token field to Request**

  In `src/daemon/protocol.rs`:

  ```rust
  pub struct Request {
      pub cmd: String,
      #[serde(default)]
      pub params: serde_json::Value,
      /// Authentication token (required for all requests when daemon has a token set).
      #[serde(default)]
      pub token: Option<String>,
  }
  ```

- [x] **2.5 Add token validation in server**

  In `src/daemon/server.rs`, modify `handle_connection` or add to `DaemonServer::start`:

  Store the token in `DaemonServer` or pass via `HandlerContext`. In `handle_connection`, after parsing the request, check token before dispatching:

  ```rust
  // In handle_connection, after read_request succeeds:
  if let Some(expected_token) = &ctx.daemon_token {
      let provided = req.token.as_deref().unwrap_or("");
      if provided != expected_token.as_str() {
          let resp = Response::error_detail(
              ErrorCode::Unauthorized,
              "invalid or missing daemon token".into(),
              None,
          );
          let _ = write_response(&mut writer, &resp).await;
          break; // disconnect unauthorized client
      }
  }
  ```

  Add `daemon_token: Option<String>` to `HandlerContext`.

- [x] **2.6 Inject token in client**

  In `src/client.rs`, `send_request` method -- before serializing, inject token:

  ```rust
  pub async fn send_request(&mut self, req: &Request) -> Result<Response, BkError> {
      let mut req = req.clone();
      if req.token.is_none() {
          req.token = crate::daemon::read_token_file();
      }
      // ... serialize and send
  }
  ```

- [x] **2.7 Call write_token_file on daemon start**

  In `src/daemon/mod.rs` `run_daemon_start` function, after binding the port:

  ```rust
  let token = generate_daemon_token();
  write_token_file(&token)?;
  // Pass token to server via HandlerContext
  ```

- [x] **2.8 Build and run all tests**

  ```bash
  cargo build
  cargo test --lib token_tests
  cargo test --lib server_rejects_request_without_token
  cargo test --lib server_accepts_request_with_valid_token
  cargo test --lib server_rejects_request_with_wrong_token
  cargo test --lib protocol::tests
  ```

- [x] **2.9 Commit**

  ```bash
  git add src/daemon/mod.rs src/daemon/server.rs src/daemon/protocol.rs src/client.rs
  git commit -m "feat: add daemon token authentication for TCP requests"
  ```

---

## Task 3: Session Abstraction Layer + Config Limits

**Goal:** Introduce `Session` type (default + isolated modes) backed by CDP BrowserContext. Add `[limits]` config section with max_sessions/max_tabs_per_session/session_timeout_hours.

**Files to create/modify:**
- `src/daemon/session.rs` -- **new file**: `Session` struct, `SessionMode` enum, session CRUD helpers
- `src/daemon/state.rs` -- add `sessions: DashMap<String, Session>` to `DaemonState`
- `src/daemon/mod.rs` -- re-export session module
- `src/config.rs` -- replace workspace-oriented limits with session-oriented limits

### Steps

- [x] **3.1 Write tests**

  Create `src/daemon/session.rs` with tests:

  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;

      #[test]
      fn session_mode_default_and_isolated() {
          let s = Session::new_default("localhost:9222".into());
          assert_eq!(s.mode, SessionMode::Default);
          assert!(s.browser_context_id.is_none());
          assert_eq!(s.name, "default");

          let s2 = Session::new_isolated("agent-a".into(), "localhost:9222".into(), "CTX123".into());
          assert_eq!(s2.mode, SessionMode::Isolated);
          assert_eq!(s2.browser_context_id, Some("CTX123".into()));
          assert_eq!(s2.name, "agent-a");
      }

      #[test]
      fn session_tracks_tabs() {
          let mut s = Session::new_default("localhost:9222".into());
          s.add_tab("TAB1".into(), "https://example.com".into(), "Example".into());
          assert_eq!(s.tab_count(), 1);
          assert_eq!(s.active_target, Some("TAB1".into()));

          s.add_tab("TAB2".into(), "https://other.com".into(), "Other".into());
          assert_eq!(s.tab_count(), 2);
          assert_eq!(s.active_target, Some("TAB2".into())); // new tab becomes active

          s.remove_tab("TAB2");
          assert_eq!(s.tab_count(), 1);
          assert_eq!(s.active_target, Some("TAB1".into())); // falls back
      }

      #[test]
      fn session_tab_limit_check() {
          let mut s = Session::new_default("localhost:9222".into());
          for i in 0..5 {
              s.add_tab(format!("T{i}"), format!("https://t{i}.com"), format!("T{i}"));
          }
          assert!(!s.can_add_tab(5)); // at limit
          assert!(s.can_add_tab(6));  // higher limit OK
      }

      #[test]
      fn session_last_active_updates() {
          let s = Session::new_default("localhost:9222".into());
          let t1 = s.last_active;
          std::thread::sleep(std::time::Duration::from_millis(10));
          // In real code, touch() updates last_active
      }
  }
  ```

  In `src/config.rs` add test:

  ```rust
  #[test]
  fn parse_v2_limits_config() {
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
  ```

- [x] **3.2 Run tests (expect failure)**

  ```bash
  cargo test --lib daemon::session::tests 2>&1 | head -20
  cargo test --lib parse_v2_limits_config 2>&1 | head -20
  ```

- [x] **3.3 Implement Session struct**

  Create `src/daemon/session.rs`:

  ```rust
  use std::collections::HashMap;
  use serde::{Deserialize, Serialize};

  /// Session operation mode.
  #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
  #[serde(rename_all = "lowercase")]
  pub enum SessionMode {
      Default,
      Isolated,
  }

  /// A tab within a session.
  #[derive(Debug, Clone, Serialize, Deserialize)]
  pub struct SessionTab {
      pub target_id: String,
      pub url: String,
      pub title: String,
      pub cdp_session_id: String,
  }

  /// Session: the v2 isolation unit replacing workspace.
  #[derive(Debug, Clone, Serialize, Deserialize)]
  pub struct Session {
      pub name: String,
      pub mode: SessionMode,
      pub browser_host: String,
      pub browser_context_id: Option<String>,
      pub tabs: HashMap<String, SessionTab>,
      pub active_target: Option<String>,
      pub created_at: u64,
      pub last_active: u64,
  }

  impl Session {
      pub fn new_default(browser_host: String) -> Self {
          let now = now_ts();
          Self {
              name: "default".into(),
              mode: SessionMode::Default,
              browser_host,
              browser_context_id: None,
              tabs: HashMap::new(),
              active_target: None,
              created_at: now,
              last_active: now,
          }
      }

      pub fn new_isolated(name: String, browser_host: String, browser_context_id: String) -> Self {
          let now = now_ts();
          Self {
              name,
              mode: SessionMode::Isolated,
              browser_host,
              browser_context_id: Some(browser_context_id),
              tabs: HashMap::new(),
              active_target: None,
              created_at: now,
              last_active: now,
          }
      }

      pub fn add_tab(&mut self, target_id: String, url: String, title: String) {
          self.tabs.insert(target_id.clone(), SessionTab {
              target_id: target_id.clone(),
              url,
              title,
              cdp_session_id: String::new(),
          });
          self.active_target = Some(target_id);
          self.touch();
      }

      pub fn remove_tab(&mut self, target_id: &str) {
          self.tabs.remove(target_id);
          if self.active_target.as_deref() == Some(target_id) {
              self.active_target = self.tabs.keys().last().cloned();
          }
          self.touch();
      }

      pub fn tab_count(&self) -> usize {
          self.tabs.len()
      }

      pub fn can_add_tab(&self, max: usize) -> bool {
          max == 0 || self.tabs.len() < max
      }

      pub fn touch(&mut self) {
          self.last_active = now_ts();
      }
  }

  fn now_ts() -> u64 {
      std::time::SystemTime::now()
          .duration_since(std::time::UNIX_EPOCH)
          .unwrap_or_default()
          .as_secs()
  }
  ```

- [x] **3.4 Add sessions to DaemonState**

  In `src/daemon/state.rs`:

  ```rust
  use crate::daemon::session::Session;

  pub struct DaemonState {
      // ... existing fields ...
      /// v2 sessions: name -> Session
      pub sessions: DashMap<String, Session>,
  }
  ```

  Initialize as `sessions: DashMap::new()` in `DaemonState::new()`.

- [x] **3.5 Update config.rs with v2 limits**

  Add new fields to `LimitsConfig` (keep old fields for backward compat):

  ```rust
  #[derive(Debug, Clone, Deserialize, Default)]
  #[serde(default)]
  pub struct LimitsConfig {
      // Legacy (v1, kept for backward compat)
      pub max_workspaces: usize,
      pub max_tabs_per_workspace: usize,
      pub js_timeout_seconds: u64,
      // V2 session limits
      pub max_sessions: usize,
      pub max_tabs_per_session: usize,
      pub session_timeout_hours: u64,
  }
  ```

  Default impl: `max_sessions: 10`, `max_tabs_per_session: 5`, `session_timeout_hours: 72`.

- [x] **3.6 Register module in src/daemon/mod.rs**

  Add `pub mod session;`

- [x] **3.7 Build and run tests**

  ```bash
  cargo build
  cargo test --lib daemon::session::tests
  cargo test --lib parse_v2_limits_config
  cargo test --lib config::tests
  ```

- [x] **3.8 Commit**

  ```bash
  git add src/daemon/session.rs src/daemon/state.rs src/daemon/mod.rs src/config.rs
  git commit -m "feat: add Session abstraction layer with default/isolated modes and config limits"
  ```

---

## Task 4: Chrome Crash Detection

**Goal:** Monitor CDP WebSocket close/error events. When Chrome disconnects, immediately clean browsers DashMap and mark affected sessions as disconnected. Subsequent commands return `CHROME_DISCONNECTED`.

**Files to modify:**
- `src/browser/mod.rs` -- spawn WebSocket health monitor task after CDP connect
- `src/daemon/state.rs` -- add `disconnected: bool` tracking or use browser presence check
- `src/daemon/session.rs` -- add `disconnected` field, helper to check connectivity

### Steps

- [x] **4.1 Write tests**

  In `src/daemon/session.rs`:

  ```rust
  #[test]
  fn session_disconnected_flag() {
      let mut s = Session::new_default("localhost:9222".into());
      assert!(!s.disconnected);
      s.mark_disconnected();
      assert!(s.disconnected);
  }
  ```

  In `src/daemon/state.rs` or a new test:

  ```rust
  #[test]
  fn cleanup_sessions_on_browser_disconnect() {
      let state = DaemonState::new();
      let mut session = Session::new_default("localhost:9222".into());
      session.add_tab("T1".into(), "https://x.com".into(), "X".into());
      state.sessions.insert("default".into(), session);
      state.browsers.insert("localhost:9222".into(), /* mock browser */);

      // Simulate disconnect
      state.handle_browser_disconnect("localhost:9222");

      assert!(!state.browsers.contains_key("localhost:9222"));
      let s = state.sessions.get("default").unwrap();
      assert!(s.disconnected);
  }
  ```

- [x] **4.2 Run tests (expect failure)**

  ```bash
  cargo test --lib session_disconnected_flag 2>&1 | head -20
  cargo test --lib cleanup_sessions_on_browser_disconnect 2>&1 | head -20
  ```

- [x] **4.3 Add disconnected field to Session**

  In `src/daemon/session.rs`:

  ```rust
  pub struct Session {
      // ... existing fields ...
      /// Set to true when the backing browser WebSocket closes unexpectedly.
      #[serde(default)]
      pub disconnected: bool,
  }

  impl Session {
      pub fn mark_disconnected(&mut self) {
          self.disconnected = true;
      }
  }
  ```

- [x] **4.4 Add handle_browser_disconnect to DaemonState**

  In `src/daemon/state.rs`:

  ```rust
  impl DaemonState {
      /// Called when a browser WebSocket disconnects.
      /// Removes browser from DashMap and marks all sessions using it as disconnected.
      pub fn handle_browser_disconnect(&self, host: &str) {
          self.browsers.remove(host);
          // Cancel any auto-attach tasks for this host
          if let Some((_, token)) = self.auto_attach_tasks.remove(host) {
              token.cancel();
          }
          // Mark all sessions using this browser as disconnected
          for mut entry in self.sessions.iter_mut() {
              if entry.value().browser_host == host {
                  entry.value_mut().mark_disconnected();
              }
          }
          self.request_persist();
          tracing::warn!(host, "browser disconnected, sessions marked");
      }
  }
  ```

- [x] **4.5 Switch to local cdpkit path dependency**

  `CDP::closed().await` is available in the local unreleased cdpkit build.
  Update `Cargo.toml`:

  ```toml
  # Replace:
  # cdpkit = "0.4.0"
  # With:
  cdpkit = { path = "../cdpkit-rs/cdpkit" }
  ```

  Verify it builds:

  ```bash
  cargo build 2>&1 | tail -5
  ```

  Expected: clean build (no errors about missing `closed` method).

- [x] **4.6 Spawn WebSocket health monitor**

  In `src/browser/mod.rs`, after successful CDP connection, spawn a task:

  ```rust
  /// Spawn a background task that detects WebSocket closure for a browser.
  /// Uses cdpkit's CDP::closed().await which resolves immediately when the
  /// WebSocket closes (Chrome crash, shutdown, or network error).
  pub fn spawn_disconnect_monitor(
      state: Arc<DaemonState>,
      host: String,
      cdp: Arc<CDP>,
  ) {
      tokio::spawn(async move {
          cdp.closed().await;
          tracing::warn!(host = %host, "CDP WebSocket closed, triggering disconnect cleanup");
          state.handle_browser_disconnect(&host);
      });
  }
  ```

  Call this in `src/browser/mod.rs` wherever CDP connection is established
  (search for `CDP::connect` call site, add `spawn_disconnect_monitor` right after).

- [x] **4.7 Guard v2 handlers against disconnected sessions**

  Add helper in `src/daemon/session.rs`:

  ```rust
  use crate::error::ErrorCode;
  use crate::daemon::protocol::Response;

  impl Session {
      /// Returns an error response if this session is disconnected.
      pub fn check_connected(&self) -> Result<(), Response> {
          if self.disconnected {
              Err(Response::error_detail(
                  ErrorCode::ChromeDisconnected,
                  format!("browser for session '{}' has disconnected", self.name),
                  None,
              ))
          } else {
              Ok(())
          }
      }
  }
  ```

- [x] **4.8 Build and run tests**

  ```bash
  cargo build
  cargo test --lib session_disconnected_flag
  cargo test --lib cleanup_sessions_on_browser_disconnect
  ```

- [x] **4.9 Commit**

  ```bash
  git add Cargo.toml src/browser/mod.rs src/daemon/state.rs src/daemon/session.rs
  git commit -m "feat: detect Chrome WebSocket disconnection and mark sessions"
  ```

---

## Task 5: `bk connect` Command

**Goal:** Implement the connect handler that discovers Chrome/Edge via DevToolsActivePort, establishes CDP connection, and creates/finds a session. Idempotent: returns `already_connected` if connected.

**Files to create/modify:**
- `src/daemon/handler/connect.rs` -- **new file**: handle_connect logic
- `src/daemon/handler/mod.rs` -- add route `"connect"` and `"v2.connect"`
- `src/browser/finder.rs` -- add `discover_browser_via_port_file()` that reads DevToolsActivePort

### Steps

- [x] **5.1 Write tests**

  In `src/daemon/handler/connect.rs`:

  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;
      use crate::daemon::state::DaemonState;
      use crate::daemon::protocol::{Request, Response};
      use serde_json::json;
      use std::sync::Arc;

      #[test]
      fn connect_result_already_connected() {
          let state = Arc::new(DaemonState::new());
          // Insert a browser and default session
          let session = Session::new_default("localhost:9222".into());
          state.sessions.insert("default".into(), session);
          // Simulate browser present
          // (We cannot insert a real Browser without CDP, but we can test the logic path)

          let result = check_already_connected(&state, None);
          // With browser in state, should report already_connected
          assert!(result.is_some());
      }

      #[test]
      fn connect_result_formats_correctly() {
          let resp = build_connect_response("connected", "Chrome 136", "default", 3);
          let json = serde_json::to_value(&resp).unwrap();
          assert_eq!(json["ok"], true);
          assert_eq!(json["data"]["status"], "connected");
          assert_eq!(json["data"]["browser"], "Chrome 136");
          assert_eq!(json["data"]["session"], "default");
          assert_eq!(json["data"]["tabs"], 3);
      }

      #[test]
      fn connect_not_connected_returns_appropriate_error() {
          // When no Chrome is running and no DevToolsActivePort exists
          let err = determine_connection_error(false, false, false);
          assert_eq!(err, ErrorCode::BrowserNotRunning);
      }

      #[test]
      fn connect_running_no_debug_returns_remote_debug_error() {
          let err = determine_connection_error(true, false, false);
          assert_eq!(err, ErrorCode::RemoteDebugNotEnabled);
      }
  }
  ```

  In `src/browser/finder.rs`:

  ```rust
  #[cfg(test)]
  mod discover_tests {
      use super::*;
      use tempfile::TempDir;
      use std::fs;

      #[test]
      fn parse_devtools_active_port_file() {
          let dir = TempDir::new().unwrap();
          let port_file = dir.path().join("DevToolsActivePort");
          fs::write(&port_file, "9222\n/devtools/browser/abc-123\n").unwrap();

          let result = parse_devtools_active_port(&port_file).unwrap();
          assert_eq!(result.port, 9222);
          assert_eq!(result.ws_path, "/devtools/browser/abc-123");
      }

      #[test]
      fn parse_devtools_active_port_missing_file() {
          let result = parse_devtools_active_port(std::path::Path::new("/nonexistent/DevToolsActivePort"));
          assert!(result.is_err());
      }

      #[test]
      fn parse_devtools_active_port_invalid_content() {
          let dir = TempDir::new().unwrap();
          let port_file = dir.path().join("DevToolsActivePort");
          fs::write(&port_file, "not_a_number\n").unwrap();
          let result = parse_devtools_active_port(&port_file);
          assert!(result.is_err());
      }

      #[test]
      fn known_chrome_user_data_dirs_not_empty() {
          let dirs = chrome_user_data_dirs();
          assert!(!dirs.is_empty());
      }

      #[test]
      fn known_edge_user_data_dirs_not_empty() {
          let dirs = edge_user_data_dirs();
          assert!(!dirs.is_empty());
      }
  }
  ```

- [x] **5.2 Run tests (expect failure)**

  ```bash
  cargo test --lib handler::connect::tests 2>&1 | head -20
  cargo test --lib discover_tests 2>&1 | head -20
  ```

- [x] **5.3 Implement DevToolsActivePort parser in src/browser/finder.rs**

  ```rust
  use std::path::{Path, PathBuf};

  /// Parsed DevToolsActivePort file content.
  pub struct DevToolsPortInfo {
      pub port: u16,
      pub ws_path: String,
  }

  /// Parse a DevToolsActivePort file (line 1 = port, line 2 = ws path).
  pub fn parse_devtools_active_port(path: &Path) -> Result<DevToolsPortInfo, String> {
      let content = std::fs::read_to_string(path)
          .map_err(|e| format!("cannot read DevToolsActivePort: {e}"))?;
      let mut lines = content.lines();
      let port: u16 = lines.next()
          .ok_or("DevToolsActivePort file is empty")?
          .trim()
          .parse()
          .map_err(|e| format!("invalid port number: {e}"))?;
      let ws_path = lines.next()
          .unwrap_or("/devtools/browser/")
          .trim()
          .to_string();
      Ok(DevToolsPortInfo { port, ws_path })
  }

  /// Return known Chrome user data directory paths for the current OS.
  pub fn chrome_user_data_dirs() -> Vec<PathBuf> {
      let mut dirs = Vec::new();
      #[cfg(target_os = "windows")]
      if let Ok(local) = std::env::var("LOCALAPPDATA") {
          dirs.push(PathBuf::from(local).join("Google").join("Chrome").join("User Data"));
      }
      #[cfg(target_os = "macos")]
      if let Ok(home) = std::env::var("HOME") {
          dirs.push(PathBuf::from(home).join("Library/Application Support/Google/Chrome"));
      }
      #[cfg(target_os = "linux")]
      if let Ok(home) = std::env::var("HOME") {
          dirs.push(PathBuf::from(home).join(".config/google-chrome"));
      }
      dirs
  }

  /// Return known Edge user data directory paths for the current OS.
  pub fn edge_user_data_dirs() -> Vec<PathBuf> {
      let mut dirs = Vec::new();
      #[cfg(target_os = "windows")]
      if let Ok(local) = std::env::var("LOCALAPPDATA") {
          dirs.push(PathBuf::from(local).join("Microsoft").join("Edge").join("User Data"));
      }
      #[cfg(target_os = "macos")]
      if let Ok(home) = std::env::var("HOME") {
          dirs.push(PathBuf::from(home).join("Library/Application Support/Microsoft Edge"));
      }
      #[cfg(target_os = "linux")]
      if let Ok(home) = std::env::var("HOME") {
          dirs.push(PathBuf::from(home).join(".config/microsoft-edge"));
      }
      dirs
  }

  /// Scan known data dirs for DevToolsActivePort, return first found.
  pub fn find_devtools_port() -> Option<DevToolsPortInfo> {
      for dir in chrome_user_data_dirs().iter().chain(edge_user_data_dirs().iter()) {
          let port_file = dir.join("DevToolsActivePort");
          if let Ok(info) = parse_devtools_active_port(&port_file) {
              return Some(info);
          }
      }
      None
  }
  ```

- [x] **5.4 Implement connect handler**

  Create `src/daemon/handler/connect.rs`:

  ```rust
  use std::sync::Arc;
  use serde_json::json;
  use crate::daemon::protocol::{Request, Response};
  use crate::daemon::state::DaemonState;
  use crate::daemon::session::{Session, SessionMode};
  use crate::error::ErrorCode;
  use crate::browser::finder;

  /// Handle the `connect` command.
  pub async fn handle_connect(req: &Request, state: &Arc<DaemonState>) -> Response {
      let session_name = req.params.get("session")
          .and_then(|v| v.as_str())
          .map(|s| s.to_string());

      let effective_name = session_name.as_deref().unwrap_or("default");

      // Check if already connected
      if let Some(resp) = check_already_connected(state, effective_name) {
          return resp;
      }

      // Try to discover and connect Chrome/Edge
      match discover_and_connect(state, effective_name).await {
          Ok(resp) => resp,
          Err(resp) => resp,
      }
  }

  fn check_already_connected(state: &Arc<DaemonState>, session_name: &str) -> Option<Response> {
      if let Some(session) = state.sessions.get(session_name) {
          if !session.disconnected && state.browsers.contains_key(&session.browser_host) {
              let browser_version = "Chrome"; // TODO: store version in Browser struct
              return Some(build_connect_response(
                  "already_connected",
                  browser_version,
                  session_name,
                  session.tab_count(),
              ));
          }
      }
      None
  }

  fn build_connect_response(status: &str, browser: &str, session: &str, tabs: usize) -> Response {
      Response::ok(json!({
          "status": status,
          "browser": browser,
          "session": session,
          "tabs": tabs,
      }))
  }

  fn determine_connection_error(is_running: bool, has_port_file: bool, port_connectable: bool) -> ErrorCode {
      if !is_running {
          ErrorCode::BrowserNotRunning
      } else if !has_port_file {
          ErrorCode::RemoteDebugNotEnabled
      } else {
          ErrorCode::ConnectionRefused
      }
  }

  async fn discover_and_connect(state: &Arc<DaemonState>, session_name: &str) -> Result<Response, Response> {
      // Find DevToolsActivePort
      let port_info = finder::find_devtools_port()
          .ok_or_else(|| {
              // Determine specific error
              let is_running = is_browser_process_running();
              let code = determine_connection_error(is_running, false, false);
              Response::error_detail(code, code.suggestion().into(), None)
          })?;

      // Attempt WebSocket connection via cdpkit
      let ws_url = format!("ws://127.0.0.1:{}{}", port_info.port, port_info.ws_path);
      let cdp = cdpkit::CDP::connect(&ws_url).await
          .map_err(|e| Response::error_detail(
              ErrorCode::ConnectionRefused,
              format!("CDP connection failed: {e}"),
              None,
          ))?;

      let cdp = std::sync::Arc::new(cdp);
      let host = format!("127.0.0.1:{}", port_info.port);

      // Register browser
      state.browsers.insert(host.clone(), crate::daemon::state::Browser {
          host: host.clone(),
          cdp: Arc::clone(&cdp),
          managed: false,
          pid: None,
          child: None,
      });

      // Create or update session
      let session = if session_name == "default" {
          Session::new_default(host.clone())
      } else {
          // Create isolated BrowserContext via CDP
          let ctx_resp = cdpkit::target::methods::CreateBrowserContext::new()
              .send(cdp.as_ref())
              .await
              .map_err(|e| Response::error_detail(
                  ErrorCode::DaemonError,
                  format!("failed to create BrowserContext: {e}"),
                  None,
              ))?;
          Session::new_isolated(session_name.into(), host.clone(), ctx_resp.browser_context_id)
      };

      let tab_count = session.tab_count();
      state.sessions.insert(session_name.into(), session);
      state.request_persist();

      // Spawn disconnect monitor
      crate::browser::spawn_disconnect_monitor(Arc::clone(state), host, cdp);

      Ok(build_connect_response("connected", "Chrome", session_name, tab_count))
  }

  /// Check if Chrome or Edge process is running (platform-specific).
  fn is_browser_process_running() -> bool {
      #[cfg(target_os = "windows")]
      {
          std::process::Command::new("tasklist")
              .args(["/FI", "IMAGENAME eq chrome.exe", "/NH"])
              .output()
              .map(|o| String::from_utf8_lossy(&o.stdout).contains("chrome.exe"))
              .unwrap_or(false)
      }
      #[cfg(not(target_os = "windows"))]
      {
          std::process::Command::new("pgrep")
              .args(["-x", "chrome|Google Chrome|msedge"])
              .output()
              .map(|o| o.status.success())
              .unwrap_or(false)
      }
  }
  ```

- [x] **5.5 Add routing in handler/mod.rs**

  ```rust
  pub mod connect;

  // In handle_request match:
  "connect" | "v2.connect" => connect::handle_connect(req, state).await,
  ```

- [x] **5.6 Build and run tests**

  ```bash
  cargo build
  cargo test --lib handler::connect::tests
  cargo test --lib discover_tests
  ```

- [x] **5.7 Commit**

  ```bash
  git add src/daemon/handler/connect.rs src/daemon/handler/mod.rs src/browser/finder.rs
  git commit -m "feat: implement bk connect with Chrome/Edge discovery via DevToolsActivePort"
  ```

---

## Task 6: `bk setup` Command (Pure CLI Side)

**Goal:** Interactive CLI command that guides the user through enabling Chrome remote debugging. Does NOT go through daemon -- runs entirely in the client process.

**Files to modify:**
- `src/main.rs` -- add `Command::Setup` variant and handle it without daemon
- `src/browser/finder.rs` -- add Chrome/Edge installation detection helpers

### Steps

- [x] **6.1 Write tests**

  In `src/browser/finder.rs`:

  ```rust
  #[cfg(test)]
  mod setup_tests {
      use super::*;

      #[test]
      fn chrome_install_paths_not_empty() {
          let paths = chrome_install_paths();
          assert!(!paths.is_empty());
      }

      #[test]
      fn edge_install_paths_not_empty() {
          let paths = edge_install_paths();
          assert!(!paths.is_empty());
      }

      #[test]
      fn detect_installed_browser_returns_result() {
          // This test verifies the function compiles and returns a valid enum
          let result = detect_installed_browser();
          match result {
              BrowserDetection::Chrome(_) | BrowserDetection::Edge(_) | BrowserDetection::None => {}
          }
      }

      #[test]
      fn setup_status_json_format() {
          let json = build_setup_success_json("Chrome 136");
          assert_eq!(json["ok"], true);
          assert_eq!(json["data"]["status"], "ready");
          assert!(json["data"]["browser"].as_str().unwrap().contains("Chrome"));
      }
  }
  ```

- [x] **6.2 Run tests (expect failure)**

  ```bash
  cargo test --lib setup_tests 2>&1 | head -20
  ```

- [x] **6.3 Implement browser detection helpers**

  In `src/browser/finder.rs`:

  ```rust
  /// Result of detecting installed browsers.
  pub enum BrowserDetection {
      Chrome(PathBuf),
      Edge(PathBuf),
      None,
  }

  /// Known Chrome executable paths per platform.
  pub fn chrome_install_paths() -> Vec<PathBuf> {
      let mut paths = Vec::new();
      #[cfg(target_os = "windows")]
      {
          if let Ok(pf) = std::env::var("PROGRAMFILES") {
              paths.push(PathBuf::from(&pf).join("Google/Chrome/Application/chrome.exe"));
          }
          if let Ok(pf) = std::env::var("PROGRAMFILES(X86)") {
              paths.push(PathBuf::from(&pf).join("Google/Chrome/Application/chrome.exe"));
          }
          if let Ok(local) = std::env::var("LOCALAPPDATA") {
              paths.push(PathBuf::from(&local).join("Google/Chrome/Application/chrome.exe"));
          }
      }
      #[cfg(target_os = "macos")]
      {
          paths.push(PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"));
      }
      #[cfg(target_os = "linux")]
      {
          paths.push(PathBuf::from("/usr/bin/google-chrome"));
          paths.push(PathBuf::from("/usr/bin/google-chrome-stable"));
          paths.push(PathBuf::from("/usr/bin/chromium-browser"));
          paths.push(PathBuf::from("/usr/bin/chromium"));
      }
      paths
  }

  /// Known Edge executable paths per platform.
  pub fn edge_install_paths() -> Vec<PathBuf> {
      let mut paths = Vec::new();
      #[cfg(target_os = "windows")]
      {
          if let Ok(pf) = std::env::var("PROGRAMFILES(X86)") {
              paths.push(PathBuf::from(&pf).join("Microsoft/Edge/Application/msedge.exe"));
          }
          if let Ok(pf) = std::env::var("PROGRAMFILES") {
              paths.push(PathBuf::from(&pf).join("Microsoft/Edge/Application/msedge.exe"));
          }
      }
      #[cfg(target_os = "macos")]
      {
          paths.push(PathBuf::from("/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"));
      }
      #[cfg(target_os = "linux")]
      {
          paths.push(PathBuf::from("/usr/bin/microsoft-edge"));
          paths.push(PathBuf::from("/usr/bin/microsoft-edge-stable"));
      }
      paths
  }

  /// Detect if Chrome or Edge is installed.
  pub fn detect_installed_browser() -> BrowserDetection {
      for p in chrome_install_paths() {
          if p.exists() { return BrowserDetection::Chrome(p); }
      }
      for p in edge_install_paths() {
          if p.exists() { return BrowserDetection::Edge(p); }
      }
      BrowserDetection::None
  }
  ```

- [x] **6.4 Implement setup command in main.rs**

  Add `Command::Setup` to the Command enum:

  ```rust
  /// Interactive setup: guide user through enabling Chrome remote debugging
  #[command(about = "Set up Chrome remote debugging (interactive, one-time)")]
  Setup,
  ```

  Handle it in main before connecting to daemon:

  ```rust
  Command::Setup => {
      return run_setup().await;
  }
  ```

  Implement `run_setup()`:

  ```rust
  async fn run_setup() -> Result<(), Box<dyn std::error::Error>> {
      use browserkit::browser::finder::*;

      // Step 1: Detect browser
      let browser = detect_installed_browser();
      let (browser_name, inspect_url) = match &browser {
          BrowserDetection::Chrome(p) => {
              eprintln!("Checking Chrome... found at {}", p.display());
              ("Chrome", "chrome://inspect/#remote-debugging")
          }
          BrowserDetection::Edge(p) => {
              eprintln!("Checking Edge... found at {}", p.display());
              ("Edge", "edge://inspect/#remote-debugging")
          }
          BrowserDetection::None => {
              let resp = serde_json::json!({
                  "ok": false,
                  "error": {
                      "code": "BROWSER_NOT_INSTALLED",
                      "message": "neither Chrome nor Edge found",
                      "suggestion": "install Google Chrome from https://www.google.com/chrome",
                      "recoverable": false
                  }
              });
              println!("{}", serde_json::to_string(&resp)?);
              return Ok(());
          }
      };

      // Step 2: Check if already configured (DevToolsActivePort exists)
      if find_devtools_port().is_some() {
          eprintln!("Checking remote debugging... already enabled!");
          let resp = build_setup_success_json(browser_name);
          println!("{}", serde_json::to_string(&resp)?);
          return Ok(());
      }

      // Step 3: Guide user
      eprintln!("Checking remote debugging... not enabled\n");
      eprintln!("Remote debugging lets bk connect to your {} browser.", browser_name);
      eprintln!("You only need to do this once.\n");
      eprintln!("Steps:");
      eprintln!("  1. Open {} (if not already open)", browser_name);
      eprintln!("  2. In the address bar, type: {}", inspect_url);
      eprintln!("  3. Enable remote debugging (check the box or toggle)");
      eprintln!("  4. Come back here and press Enter\n");
      eprintln!("Waiting... [Press Enter when done]");

      // Wait for user input
      let mut _input = String::new();
      std::io::stdin().read_line(&mut _input)?;

      // Step 4: Poll for DevToolsActivePort (up to 30 attempts, 1s each)
      eprintln!("Checking connection...");
      for _ in 0..30 {
          if find_devtools_port().is_some() {
              eprintln!("Connected to {}!", browser_name);
              let resp = build_setup_success_json(browser_name);
              println!("{}", serde_json::to_string(&resp)?);
              return Ok(());
          }
          tokio::time::sleep(std::time::Duration::from_secs(1)).await;
      }

      // Timeout
      let resp = serde_json::json!({
          "ok": false,
          "error": {
              "code": "REMOTE_DEBUG_NOT_ENABLED",
              "message": "could not detect remote debugging after 30s",
              "suggestion": format!("open {} and enable remote debugging, then retry bk setup", inspect_url),
              "recoverable": true
          }
      });
      println!("{}", serde_json::to_string(&resp)?);
      Ok(())
  }

  fn build_setup_success_json(browser: &str) -> serde_json::Value {
      serde_json::json!({
          "ok": true,
          "data": {
              "status": "ready",
              "browser": browser,
              "message": format!("Remote debugging enabled. Run 'bk connect' to start.")
          }
      })
  }
  ```

- [x] **6.5 Build and run tests**

  ```bash
  cargo build
  cargo test --lib setup_tests
  ```

- [x] **6.6 Commit**

  ```bash
  git add src/main.rs src/browser/finder.rs
  git commit -m "feat: add bk setup interactive command for Chrome remote debugging configuration"
  ```

---

## Task 7: `bk open` Command

**Goal:** Open a new tab in the session's BrowserContext, navigate to URL, set as active tab, return snapshot.

**Files to create/modify:**
- `src/daemon/handler/open.rs` -- **new file**: handle_open
- `src/daemon/handler/mod.rs` -- add route

### Steps

- [x] **7.1 Write tests**

  In `src/daemon/handler/open.rs`:

  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;
      use crate::daemon::state::DaemonState;
      use crate::daemon::session::Session;
      use std::sync::Arc;

      #[test]
      fn validate_open_params_requires_url() {
          let params = serde_json::json!({});
          let err = validate_open_params(&params).unwrap_err();
          assert!(err.to_string().contains("url"));
      }

      #[test]
      fn validate_open_params_accepts_url() {
          let params = serde_json::json!({"url": "https://example.com"});
          let result = validate_open_params(&params).unwrap();
          assert_eq!(result.url, "https://example.com");
          assert_eq!(result.session_name, "default");
      }

      #[test]
      fn validate_open_params_with_session() {
          let params = serde_json::json!({"url": "https://x.com", "session": "agent-a"});
          let result = validate_open_params(&params).unwrap();
          assert_eq!(result.session_name, "agent-a");
      }

      #[test]
      fn tab_limit_exceeded_check() {
          let state = Arc::new(DaemonState::new());
          let mut session = Session::new_default("localhost:9222".into());
          for i in 0..5 {
              session.add_tab(format!("T{i}"), format!("https://t{i}.com"), format!("T{i}"));
          }
          state.sessions.insert("default".into(), session);

          let result = check_tab_limit(&state, "default", 5);
          assert!(result.is_err());
          let resp = result.unwrap_err();
          let json = serde_json::to_value(&resp).unwrap();
          assert_eq!(json["error"]["code"], "TAB_LIMIT_EXCEEDED");
      }
  }
  ```

- [x] **7.2 Run tests (expect failure)**

  ```bash
  cargo test --lib handler::open::tests 2>&1 | head -20
  ```

- [x] **7.3 Implement open handler**

  Create `src/daemon/handler/open.rs`:

  ```rust
  use std::sync::Arc;
  use serde_json::json;
  use crate::daemon::protocol::{Request, Response};
  use crate::daemon::state::DaemonState;
  use crate::error::ErrorCode;

  struct OpenParams {
      url: String,
      session_name: String,
      timeout: u64,
  }

  fn validate_open_params(params: &serde_json::Value) -> Result<OpenParams, Response> {
      let url = params.get("url")
          .and_then(|v| v.as_str())
          .ok_or_else(|| Response::error_detail(
              ErrorCode::InvalidArgument,
              "missing required parameter: url".into(),
              None,
          ))?
          .to_string();

      let session_name = params.get("session")
          .and_then(|v| v.as_str())
          .unwrap_or("default")
          .to_string();

      let timeout = params.get("timeout")
          .and_then(|v| v.as_u64())
          .unwrap_or(30000);

      Ok(OpenParams { url, session_name, timeout })
  }

  fn check_tab_limit(state: &Arc<DaemonState>, session_name: &str, max: usize) -> Result<(), Response> {
      if max == 0 { return Ok(()); }
      if let Some(session) = state.sessions.get(session_name) {
          if !session.can_add_tab(max) {
              return Err(Response::error_detail(
                  ErrorCode::TabLimitExceeded,
                  format!("session '{}' already has {} tabs (limit: {})", session_name, session.tab_count(), max),
                  None,
              ));
          }
      }
      Ok(())
  }

  pub async fn handle_open(req: &Request, state: &Arc<DaemonState>) -> Response {
      let params = validate_open_params(&req.params);
      let params = match params {
          Ok(p) => p,
          Err(resp) => return resp,
      };

      // Check tab limit
      let max_tabs = state.config.limits.max_tabs_per_session;
      if let Err(resp) = check_tab_limit(state, &params.session_name, max_tabs) {
          return resp;
      }

      // Get session (must exist -- connect should have been called)
      let session = match state.sessions.get(&params.session_name) {
          Some(s) => s,
          None => return Response::error_detail(
              ErrorCode::SessionNotFound,
              format!("session '{}' not found", params.session_name),
              Some("run 'bk connect' first or specify --session".into()),
          ),
      };

      // Check session is connected
      if let Err(resp) = session.check_connected() {
          return resp;
      }

      // Get CDP connection
      let cdp = match state.browsers.get(&session.browser_host) {
          Some(b) => Arc::clone(&b.cdp),
          None => return Response::error_detail(
              ErrorCode::ChromeDisconnected,
              "no browser connection for this session".into(),
              None,
          ),
      };

      let browser_context_id = session.browser_context_id.clone();
      drop(session); // Release DashMap ref before async

      // Create new tab via CDP Target.createTarget
      let mut create = cdpkit::target::methods::CreateTarget::new(params.url.clone());
      if let Some(ctx_id) = &browser_context_id {
          create = create.with_browser_context_id(ctx_id.clone());
      }

      let result = match create.send(cdp.as_ref()).await {
          Ok(r) => r,
          Err(e) => return Response::error_detail(
              ErrorCode::NavigateFailed,
              format!("failed to create tab: {e}"),
              None,
          ),
      };

      let target_id = result.target_id;

      // Attach to the new target
      let attach_result = cdpkit::target::methods::AttachToTarget::new(target_id.clone())
          .with_flatten(true)
          .send(cdp.as_ref())
          .await;

      let session_id = match attach_result {
          Ok(r) => r.session_id,
          Err(e) => return Response::error_detail(
              ErrorCode::DaemonError,
              format!("failed to attach to new tab: {e}"),
              None,
          ),
      };

      // Update session state
      if let Some(mut session) = state.sessions.get_mut(&params.session_name) {
          session.add_tab(target_id.clone(), params.url.clone(), String::new());
          if let Some(tab) = session.tabs.get_mut(&target_id) {
              tab.cdp_session_id = session_id.clone();
          }
      }
      state.request_persist();

      // Wait for page load then get snapshot
      // (Reuse snapshot logic from Task 8 -- for now return basic info)
      // TODO: After Task 8, call get_snapshot() here
      Response::ok(json!({
          "target": target_id,
          "url": params.url,
          "session": params.session_name,
      }))
  }
  ```

- [x] **7.4 Add route in handler/mod.rs**

  ```rust
  pub mod open;

  // In match:
  "open" | "v2.open" => open::handle_open(req, state).await,
  ```

- [x] **7.5 Build and run tests**

  ```bash
  cargo build
  cargo test --lib handler::open::tests
  ```

- [x] **7.6 Commit**

  ```bash
  git add src/daemon/handler/open.rs src/daemon/handler/mod.rs
  git commit -m "feat: implement bk open command to create tabs in session"
  ```

---

## Task 8: `bk snapshot` Command

**Goal:** Return complete page state (elements + page_text + scroll + viewport) with dom-stable wait strategy. Reuse existing `page/state.rs` discovery logic.

**Files to create/modify:**
- `src/daemon/handler/snapshot.rs` -- **new file**: handle_snapshot
- `src/daemon/handler/mod.rs` -- add route
- `src/page/state.rs` -- extract reusable `get_page_elements()` if needed (or call existing functions directly)

### Steps

- [x] **8.1 Write tests**

  In `src/daemon/handler/snapshot.rs`:

  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;

      #[test]
      fn validate_snapshot_params_defaults() {
          let params = serde_json::json!({});
          let p = validate_snapshot_params(&params);
          assert_eq!(p.session_name, "default");
          assert_eq!(p.target, None);
          assert_eq!(p.wait_strategy, WaitStrategy::DomStable);
          assert!(!p.full);
          assert_eq!(p.timeout, 30000);
      }

      #[test]
      fn validate_snapshot_params_custom() {
          let params = serde_json::json!({
              "session": "agent-a",
              "target": "TAB123",
              "wait": "networkidle",
              "full": true,
              "timeout": 60000
          });
          let p = validate_snapshot_params(&params);
          assert_eq!(p.session_name, "agent-a");
          assert_eq!(p.target, Some("TAB123".into()));
          assert_eq!(p.wait_strategy, WaitStrategy::NetworkIdle);
          assert!(p.full);
          assert_eq!(p.timeout, 60000);
      }

      #[test]
      fn wait_strategy_from_str() {
          assert_eq!(WaitStrategy::from_param(Some("dom-stable")), WaitStrategy::DomStable);
          assert_eq!(WaitStrategy::from_param(Some("networkidle")), WaitStrategy::NetworkIdle);
          assert_eq!(WaitStrategy::from_param(Some("none")), WaitStrategy::None);
          assert_eq!(WaitStrategy::from_param(None), WaitStrategy::DomStable);
          assert_eq!(WaitStrategy::from_param(Some("invalid")), WaitStrategy::DomStable);
      }

      #[test]
      fn page_text_truncation() {
          let long_text = "a".repeat(3000);
          let truncated = truncate_page_text(&long_text, 2000);
          assert!(truncated.len() <= 2000);
      }

      #[test]
      fn page_text_wrapping() {
          let text = "Hello World";
          let wrapped = wrap_page_text(text);
          assert!(wrapped.starts_with("[PAGE_CONTENT_START]"));
          assert!(wrapped.ends_with("[PAGE_CONTENT_END]"));
          assert!(wrapped.contains("Hello World"));
      }

      #[test]
      fn snapshot_response_structure() {
          let data = build_snapshot_data(
              "https://example.com",
              "Example",
              "TAB123",
              1280, 720,
              0, 0, 900, 0,
              vec![],
              0, 0,
              "page text here",
              false,
          );
          assert_eq!(data["url"], "https://example.com");
          assert_eq!(data["title"], "Example");
          assert_eq!(data["target"], "TAB123");
          assert_eq!(data["viewport"]["width"], 1280);
          assert_eq!(data["scroll"]["height"], 900);
          assert_eq!(data["total_elements"], 0);
          assert_eq!(data["truncated"], false);
      }
  }
  ```

- [x] **8.2 Run tests (expect failure)**

  ```bash
  cargo test --lib handler::snapshot::tests 2>&1 | head -20
  ```

- [x] **8.3 Implement snapshot handler**

  Create `src/daemon/handler/snapshot.rs`:

  ```rust
  use std::sync::Arc;
  use serde_json::json;
  use crate::daemon::protocol::{Request, Response};
  use crate::daemon::state::DaemonState;
  use crate::error::ErrorCode;

  #[derive(Debug, Clone, PartialEq, Eq)]
  pub enum WaitStrategy {
      DomStable,
      NetworkIdle,
      None,
  }

  impl WaitStrategy {
      pub fn from_param(s: Option<&str>) -> Self {
          match s {
              Some("networkidle") => Self::NetworkIdle,
              Some("none") => Self::None,
              _ => Self::DomStable,
          }
      }
  }

  struct SnapshotParams {
      session_name: String,
      target: Option<String>,
      wait_strategy: WaitStrategy,
      full: bool,
      no_page_text: bool,
      timeout: u64,
  }

  fn validate_snapshot_params(params: &serde_json::Value) -> SnapshotParams {
      SnapshotParams {
          session_name: params.get("session").and_then(|v| v.as_str()).unwrap_or("default").into(),
          target: params.get("target").and_then(|v| v.as_str()).map(|s| s.into()),
          wait_strategy: WaitStrategy::from_param(params.get("wait").and_then(|v| v.as_str())),
          full: params.get("full").and_then(|v| v.as_bool()).unwrap_or(false),
          no_page_text: params.get("no_page_text").and_then(|v| v.as_bool()).unwrap_or(false),
          timeout: params.get("timeout").and_then(|v| v.as_u64()).unwrap_or(30000),
      }
  }

  fn truncate_page_text(text: &str, max: usize) -> &str {
      if text.len() <= max {
          return text;
      }
      // Try to truncate at a sentence or paragraph boundary
      let slice = &text[..max];
      if let Some(pos) = slice.rfind('\n') {
          if pos > max / 2 { return &text[..pos]; }
      }
      if let Some(pos) = slice.rfind(". ") {
          if pos > max / 2 { return &text[..pos + 1]; }
      }
      slice
  }

  fn wrap_page_text(text: &str) -> String {
      format!("[PAGE_CONTENT_START]{text}[PAGE_CONTENT_END]")
  }

  fn build_snapshot_data(
      url: &str, title: &str, target: &str,
      vp_width: u32, vp_height: u32,
      scroll_x: i64, scroll_y: i64, scroll_height: i64, scroll_percent: i64,
      elements: Vec<serde_json::Value>,
      total_elements: usize, elements_shown: usize,
      page_text: &str, truncated: bool,
  ) -> serde_json::Value {
      json!({
          "url": url,
          "title": title,
          "target": target,
          "viewport": {"width": vp_width, "height": vp_height},
          "scroll": {"x": scroll_x, "y": scroll_y, "height": scroll_height, "percent": scroll_percent},
          "elements": elements,
          "total_elements": total_elements,
          "elements_shown": elements_shown,
          "page_text": page_text,
          "truncated": truncated,
      })
  }

  pub async fn handle_snapshot(req: &Request, state: &Arc<DaemonState>) -> Response {
      let params = validate_snapshot_params(&req.params);

      // Resolve session and target
      let session = match state.sessions.get(&params.session_name) {
          Some(s) => s,
          None => return Response::error_detail(
              ErrorCode::SessionNotFound,
              format!("session '{}' not found", params.session_name),
              None,
          ),
      };

      if let Err(resp) = session.check_connected() { return resp; }

      let target_id = match params.target.as_ref().or(session.active_target.as_ref()) {
          Some(t) => t.clone(),
          None => return Response::error_detail(ErrorCode::SessionNoTab, "no active tab".into(), None),
      };

      let session_tab = match session.tabs.get(&target_id) {
          Some(t) => t.clone(),
          None => return Response::error_detail(
              ErrorCode::TargetNotFound,
              format!("target '{}' not in session", target_id),
              None,
          ),
      };

      let browser_host = session.browser_host.clone();
      drop(session);

      // Get CDP
      let cdp = match state.browsers.get(&browser_host) {
          Some(b) => Arc::clone(&b.cdp),
          None => return Response::error_detail(ErrorCode::ChromeDisconnected, "browser gone".into(), None),
      };

      let cdp_session = cdp.session(&session_tab.cdp_session_id);

      // Execute page state discovery (reuse existing logic from page/state.rs)
      // Call get_full_page_state which runs the DISCOVER_ELEMENTS_JS
      match crate::page::state::get_full_page_state(&cdp_session, params.full).await {
          Ok(page_state) => {
              let max_text = if params.full { 8000 } else { 2000 };
              let text = truncate_page_text(&page_state.page_text, max_text);
              let truncated = text.len() < page_state.page_text.len();
              let wrapped = if params.no_page_text { String::new() } else { wrap_page_text(text) };

              let max_elements = if params.full { usize::MAX } else { 50 };
              let elements_shown = page_state.elements.len().min(max_elements);
              let elements: Vec<_> = page_state.elements.into_iter()
                  .take(max_elements)
                  .map(|e| element_to_json(&e))
                  .collect();

              let data = build_snapshot_data(
                  &page_state.url, &page_state.title, &target_id,
                  page_state.viewport_width, page_state.viewport_height,
                  page_state.scroll_x, page_state.scroll_y,
                  page_state.scroll_height, page_state.scroll_percent,
                  elements,
                  page_state.total_elements, elements_shown,
                  &wrapped, truncated,
              );
              Response::ok(data)
          }
          Err(e) => Response::error_detail(
              ErrorCode::JsError,
              format!("snapshot failed: {e}"),
              None,
          ),
      }
  }

  fn element_to_json(el: &crate::page::ElementInfo) -> serde_json::Value {
      let mut obj = json!({
          "ref": el.backend_node_id,
          "tag": el.tag,
      });
      let m = obj.as_object_mut().unwrap();
      if let Some(t) = &el.text { m.insert("text".into(), json!(t)); }
      if let Some(t) = &el.element_type { m.insert("type".into(), json!(t)); }
      if let Some(id) = &el.id { m.insert("id".into(), json!(id)); }
      if let Some(href) = &el.href { m.insert("href".into(), json!(href)); }
      if let Some(ph) = &el.placeholder { m.insert("placeholder".into(), json!(ph)); }
      if let Some(al) = &el.aria_label { m.insert("aria_label".into(), json!(al)); }
      if let Some(v) = &el.value { m.insert("value".into(), json!(v)); }
      if let Some(c) = el.checked { m.insert("checked".into(), json!(c)); }
      json!(obj)
  }
  ```

  Note: The `get_full_page_state` function signature may need adapting. Review `src/page/state.rs` for the actual return type and adjust. The key insight is reusing the existing element discovery JS and `ElementInfo` struct.

- [x] **8.4 Add route**

  In `src/daemon/handler/mod.rs`:

  ```rust
  pub mod snapshot;

  "snapshot" | "v2.snapshot" => snapshot::handle_snapshot(req, state).await,
  ```

- [x] **8.5 Build and run tests**

  ```bash
  cargo build
  cargo test --lib handler::snapshot::tests
  ```

- [x] **8.6 Commit**

  ```bash
  git add src/daemon/handler/snapshot.rs src/daemon/handler/mod.rs
  git commit -m "feat: implement bk snapshot with dom-stable wait and page text wrapping"
  ```

---

## Task 9: `bk navigate` Command

**Goal:** Unified navigation command merging goto/back/forward/reload. Includes SPA detection (URL change + DOM stable fallback when load event does not fire).

**Files to create/modify:**
- `src/daemon/handler/navigate_v2.rs` -- **new file**: handle_navigate_v2
- `src/daemon/handler/mod.rs` -- add route

### Steps

- [x] **9.1 Write tests**

  In `src/daemon/handler/navigate_v2.rs`:

  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;

      #[test]
      fn validate_navigate_params_url() {
          let params = serde_json::json!({"url": "https://example.com"});
          let p = validate_navigate_params(&params).unwrap();
          assert_eq!(p.action, NavAction::Goto("https://example.com".into()));
          assert_eq!(p.session_name, "default");
      }

      #[test]
      fn validate_navigate_params_back() {
          let params = serde_json::json!({"back": true});
          let p = validate_navigate_params(&params).unwrap();
          assert_eq!(p.action, NavAction::Back);
      }

      #[test]
      fn validate_navigate_params_forward() {
          let params = serde_json::json!({"forward": true});
          let p = validate_navigate_params(&params).unwrap();
          assert_eq!(p.action, NavAction::Forward);
      }

      #[test]
      fn validate_navigate_params_reload() {
          let params = serde_json::json!({"reload": true});
          let p = validate_navigate_params(&params).unwrap();
          assert_eq!(p.action, NavAction::Reload);
      }

      #[test]
      fn validate_navigate_params_no_action_is_error() {
          let params = serde_json::json!({});
          let err = validate_navigate_params(&params).unwrap_err();
          let json = serde_json::to_value(&err).unwrap();
          assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
      }

      #[test]
      fn validate_navigate_params_with_session_and_target() {
          let params = serde_json::json!({
              "url": "https://x.com",
              "session": "agent-a",
              "target": "TAB1",
              "timeout": 60000
          });
          let p = validate_navigate_params(&params).unwrap();
          assert_eq!(p.session_name, "agent-a");
          assert_eq!(p.target, Some("TAB1".into()));
          assert_eq!(p.timeout, 60000);
      }
  }
  ```

- [x] **9.2 Run tests (expect failure)**

  ```bash
  cargo test --lib handler::navigate_v2::tests 2>&1 | head -20
  ```

- [x] **9.3 Implement navigate handler**

  Create `src/daemon/handler/navigate_v2.rs`:

  ```rust
  use std::sync::Arc;
  use serde_json::json;
  use crate::daemon::protocol::{Request, Response};
  use crate::daemon::state::DaemonState;
  use crate::error::ErrorCode;

  #[derive(Debug, Clone, PartialEq, Eq)]
  pub enum NavAction {
      Goto(String),
      Back,
      Forward,
      Reload,
  }

  struct NavigateParams {
      action: NavAction,
      session_name: String,
      target: Option<String>,
      timeout: u64,
  }

  fn validate_navigate_params(params: &serde_json::Value) -> Result<NavigateParams, Response> {
      let action = if let Some(url) = params.get("url").and_then(|v| v.as_str()) {
          NavAction::Goto(url.to_string())
      } else if params.get("back").and_then(|v| v.as_bool()).unwrap_or(false) {
          NavAction::Back
      } else if params.get("forward").and_then(|v| v.as_bool()).unwrap_or(false) {
          NavAction::Forward
      } else if params.get("reload").and_then(|v| v.as_bool()).unwrap_or(false) {
          NavAction::Reload
      } else {
          return Err(Response::error_detail(
              ErrorCode::InvalidArgument,
              "navigate requires url, --back, --forward, or --reload".into(),
              None,
          ));
      };

      Ok(NavigateParams {
          action,
          session_name: params.get("session").and_then(|v| v.as_str()).unwrap_or("default").into(),
          target: params.get("target").and_then(|v| v.as_str()).map(|s| s.into()),
          timeout: params.get("timeout").and_then(|v| v.as_u64()).unwrap_or(30000),
      })
  }

  pub async fn handle_navigate_v2(req: &Request, state: &Arc<DaemonState>) -> Response {
      let params = match validate_navigate_params(&req.params) {
          Ok(p) => p,
          Err(resp) => return resp,
      };

      // Resolve session and target (same pattern as snapshot)
      let session = match state.sessions.get(&params.session_name) {
          Some(s) => s,
          None => return Response::error_detail(
              ErrorCode::SessionNotFound,
              format!("session '{}' not found", params.session_name),
              None,
          ),
      };

      if let Err(resp) = session.check_connected() { return resp; }

      let target_id = match params.target.as_ref().or(session.active_target.as_ref()) {
          Some(t) => t.clone(),
          None => return Response::error_detail(ErrorCode::SessionNoTab, "no active tab".into(), None),
      };

      let tab = match session.tabs.get(&target_id) {
          Some(t) => t.clone(),
          None => return Response::error_detail(ErrorCode::TargetNotFound, "target not in session".into(), None),
      };

      let browser_host = session.browser_host.clone();
      drop(session);

      let cdp = match state.browsers.get(&browser_host) {
          Some(b) => Arc::clone(&b.cdp),
          None => return Response::error_detail(ErrorCode::ChromeDisconnected, "browser disconnected".into(), None),
      };

      let cdp_session = cdp.session(&tab.cdp_session_id);

      // Execute navigation based on action
      let result = match &params.action {
          NavAction::Goto(url) => {
              // Use existing page navigation logic
              crate::page::navigation::navigate_and_wait(&cdp_session, url, params.timeout).await
          }
          NavAction::Back => {
              cdpkit::page::methods::GoBack::new().send(&cdp_session).await
                  .map(|_| ()).map_err(|e| e.into())
          }
          NavAction::Forward => {
              cdpkit::page::methods::GoForward::new().send(&cdp_session).await
                  .map(|_| ()).map_err(|e| e.into())
          }
          NavAction::Reload => {
              cdpkit::page::methods::Reload::new().send(&cdp_session).await
                  .map(|_| ()).map_err(|e| e.into())
          }
      };

      match result {
          Ok(()) => {
              // Get current URL and title after navigation
              let url = get_current_url(&cdp_session).await.unwrap_or_default();
              let title = get_current_title(&cdp_session).await.unwrap_or_default();

              // Update session tab info
              if let Some(mut session) = state.sessions.get_mut(&params.session_name) {
                  if let Some(tab) = session.tabs.get_mut(&target_id) {
                      tab.url = url.clone();
                      tab.title = title.clone();
                  }
                  session.touch();
              }
              state.request_persist();

              Response::ok(json!({
                  "url": url,
                  "title": title,
                  "target": target_id,
              }))
          }
          Err(e) => Response::error_detail(
              ErrorCode::NavigateFailed,
              format!("navigation failed: {e}"),
              None,
          ),
      }
  }

  async fn get_current_url(session: &impl cdpkit::Sender) -> Option<String> {
      let js = "window.location.href";
      cdpkit::runtime::methods::Evaluate::new(js.into())
          .send(session).await.ok()
          .and_then(|r| r.result.value)
          .and_then(|v| v.as_str().map(|s| s.to_string()))
  }

  async fn get_current_title(session: &impl cdpkit::Sender) -> Option<String> {
      let js = "document.title";
      cdpkit::runtime::methods::Evaluate::new(js.into())
          .send(session).await.ok()
          .and_then(|r| r.result.value)
          .and_then(|v| v.as_str().map(|s| s.to_string()))
  }
  ```

  Note: `navigate_and_wait` references the existing `src/page/navigation.rs` module. Adapt function signatures to match what already exists. The SPA detection (URL change + 200ms DOM stable) is handled within that module's existing logic or needs a thin wrapper.

- [x] **9.4 Add route**

  In `src/daemon/handler/mod.rs`:

  ```rust
  pub mod navigate_v2;

  "navigate" | "v2.navigate" => navigate_v2::handle_navigate_v2(req, state).await,
  ```

- [x] **9.5 Build and run tests**

  ```bash
  cargo build
  cargo test --lib handler::navigate_v2::tests
  ```

- [x] **9.6 Commit**

  ```bash
  git add src/daemon/handler/navigate_v2.rs src/daemon/handler/mod.rs
  git commit -m "feat: implement bk navigate with back/forward/reload and SPA detection"
  ```

---

## Task 10: `bk act click/type/press` (Core 3 Actions)

**Goal:** Implement the three most common interaction actions via a unified `act` dispatcher. Each returns result + state_diff (from Task 11, initially null).

**Files to create/modify:**
- `src/daemon/handler/act_v2.rs` -- **new file**: unified act dispatcher + click/type/press implementations
- `src/daemon/handler/mod.rs` -- add route

### Steps

- [x] **10.1 Write tests**

  In `src/daemon/handler/act_v2.rs`:

  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;

      #[test]
      fn parse_act_kind_click() {
          let params = serde_json::json!({"kind": "click", "ref": 42});
          let p = parse_act_params(&params).unwrap();
          assert_eq!(p.kind, ActKind::Click);
          assert_eq!(p.ref_id, Some(42));
      }

      #[test]
      fn parse_act_kind_click_with_coords() {
          let params = serde_json::json!({"kind": "click", "x": 100.5, "y": 200.0});
          let p = parse_act_params(&params).unwrap();
          assert_eq!(p.kind, ActKind::Click);
          assert_eq!(p.x, Some(100.5));
          assert_eq!(p.y, Some(200.0));
      }

      #[test]
      fn parse_act_kind_type() {
          let params = serde_json::json!({"kind": "type", "ref": 55, "text": "hello"});
          let p = parse_act_params(&params).unwrap();
          assert_eq!(p.kind, ActKind::Type);
          assert_eq!(p.ref_id, Some(55));
          assert_eq!(p.text, Some("hello".into()));
          assert!(!p.append); // default: replace
      }

      #[test]
      fn parse_act_kind_type_append() {
          let params = serde_json::json!({"kind": "type", "ref": 55, "text": "more", "append": true});
          let p = parse_act_params(&params).unwrap();
          assert!(p.append);
      }

      #[test]
      fn parse_act_kind_press() {
          let params = serde_json::json!({"kind": "press", "keys": ["Enter"]});
          let p = parse_act_params(&params).unwrap();
          assert_eq!(p.kind, ActKind::Press);
          assert_eq!(p.keys, vec!["Enter"]);
      }

      #[test]
      fn parse_act_kind_press_combo() {
          let params = serde_json::json!({"kind": "press", "keys": ["Control+a", "Backspace"]});
          let p = parse_act_params(&params).unwrap();
          assert_eq!(p.keys, vec!["Control+a", "Backspace"]);
      }

      #[test]
      fn parse_act_missing_kind_is_error() {
          let params = serde_json::json!({"ref": 42});
          let err = parse_act_params(&params).unwrap_err();
          let json = serde_json::to_value(&err).unwrap();
          assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
      }

      #[test]
      fn parse_act_click_no_ref_no_coords_is_error() {
          let params = serde_json::json!({"kind": "click"});
          let err = parse_act_params(&params).unwrap_err();
          let json = serde_json::to_value(&err).unwrap();
          assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
      }

      #[test]
      fn act_response_structure() {
          let resp = build_act_response("click", Some(42), "completed", None, None, "TAB1");
          let json = serde_json::to_value(&resp).unwrap();
          assert_eq!(json["data"]["action"], "click");
          assert_eq!(json["data"]["ref"], 42);
          assert_eq!(json["data"]["result"], "completed");
          assert_eq!(json["data"]["target"], "TAB1");
      }
  }
  ```

- [x] **10.2 Run tests (expect failure)**

  ```bash
  cargo test --lib handler::act_v2::tests 2>&1 | head -20
  ```

- [x] **10.3 Implement act handler**

  Create `src/daemon/handler/act_v2.rs`:

  ```rust
  use std::sync::Arc;
  use serde_json::json;
  use crate::daemon::protocol::{Request, Response};
  use crate::daemon::state::DaemonState;
  use crate::error::ErrorCode;

  #[derive(Debug, Clone, PartialEq, Eq)]
  pub enum ActKind {
      Click,
      Type,
      Press,
      // Phase 2: Scroll, Select, Fill, Hover, Focus, Drag, Upload, Dialog
  }

  struct ActParams {
      kind: ActKind,
      session_name: String,
      target: Option<String>,
      timeout: u64,
      no_state_diff: bool,
      // Click params
      ref_id: Option<i64>,
      x: Option<f64>,
      y: Option<f64>,
      // Type params
      text: Option<String>,
      append: bool,
      // Press params
      keys: Vec<String>,
  }

  fn parse_act_params(params: &serde_json::Value) -> Result<ActParams, Response> {
      let kind_str = params.get("kind").and_then(|v| v.as_str())
          .ok_or_else(|| Response::error_detail(
              ErrorCode::InvalidArgument,
              "missing required parameter: kind (click/type/press)".into(),
              None,
          ))?;

      let kind = match kind_str {
          "click" => ActKind::Click,
          "type" => ActKind::Type,
          "press" => ActKind::Press,
          _ => return Err(Response::error_detail(
              ErrorCode::InvalidArgument,
              format!("unsupported act kind: '{}' (supported: click, type, press)", kind_str),
              None,
          )),
      };

      let ref_id = params.get("ref").and_then(|v| v.as_i64());
      let x = params.get("x").and_then(|v| v.as_f64());
      let y = params.get("y").and_then(|v| v.as_f64());
      let text = params.get("text").and_then(|v| v.as_str()).map(|s| s.to_string());
      let append = params.get("append").and_then(|v| v.as_bool()).unwrap_or(false);
      let keys: Vec<String> = params.get("keys")
          .and_then(|v| v.as_array())
          .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
          .unwrap_or_default();

      // Validation per kind
      match kind {
          ActKind::Click => {
              if ref_id.is_none() && (x.is_none() || y.is_none()) {
                  return Err(Response::error_detail(
                      ErrorCode::InvalidArgument,
                      "click requires --ref or both --x and --y".into(),
                      None,
                  ));
              }
          }
          ActKind::Type => {
              if ref_id.is_none() {
                  return Err(Response::error_detail(ErrorCode::InvalidArgument, "type requires --ref".into(), None));
              }
              if text.is_none() {
                  return Err(Response::error_detail(ErrorCode::InvalidArgument, "type requires text".into(), None));
              }
          }
          ActKind::Press => {
              if keys.is_empty() {
                  return Err(Response::error_detail(ErrorCode::InvalidArgument, "press requires keys".into(), None));
              }
          }
      }

      Ok(ActParams {
          kind,
          session_name: params.get("session").and_then(|v| v.as_str()).unwrap_or("default").into(),
          target: params.get("target").and_then(|v| v.as_str()).map(|s| s.into()),
          timeout: params.get("timeout").and_then(|v| v.as_u64()).unwrap_or(30000),
          no_state_diff: params.get("no_state_diff").and_then(|v| v.as_bool()).unwrap_or(false),
          ref_id, x, y, text, append, keys,
      })
  }

  fn build_act_response(
      action: &str,
      ref_id: Option<i64>,
      result: &str,
      state_diff: Option<serde_json::Value>,
      new_tab: Option<&str>,
      target: &str,
  ) -> Response {
      let mut data = json!({
          "action": action,
          "result": result,
          "state_diff": state_diff,
          "target": target,
      });
      if let Some(r) = ref_id {
          data["ref"] = json!(r);
      }
      if let Some(nt) = new_tab {
          data["new_tab"] = json!(nt);
      }
      Response::ok(data)
  }

  pub async fn handle_act_v2(req: &Request, state: &Arc<DaemonState>) -> Response {
      let params = match parse_act_params(&req.params) {
          Ok(p) => p,
          Err(resp) => return resp,
      };

      // Resolve session and target (same pattern as snapshot/navigate)
      let session = match state.sessions.get(&params.session_name) {
          Some(s) => s,
          None => return Response::error_detail(ErrorCode::SessionNotFound, format!("session '{}' not found", params.session_name), None),
      };
      if let Err(resp) = session.check_connected() { return resp; }

      let target_id = match params.target.as_ref().or(session.active_target.as_ref()) {
          Some(t) => t.clone(),
          None => return Response::error_detail(ErrorCode::SessionNoTab, "no active tab".into(), None),
      };
      let tab = match session.tabs.get(&target_id) {
          Some(t) => t.clone(),
          None => return Response::error_detail(ErrorCode::TargetNotFound, "target not in session".into(), None),
      };
      let browser_host = session.browser_host.clone();
      drop(session);

      let cdp = match state.browsers.get(&browser_host) {
          Some(b) => Arc::clone(&b.cdp),
          None => return Response::error_detail(ErrorCode::ChromeDisconnected, "disconnected".into(), None),
      };
      let cdp_session = cdp.session(&tab.cdp_session_id);

      // Dispatch by kind
      match params.kind {
          ActKind::Click => {
              execute_click(&cdp_session, &params, &target_id, state).await
          }
          ActKind::Type => {
              execute_type(&cdp_session, &params, &target_id).await
          }
          ActKind::Press => {
              execute_press(&cdp_session, &params, &target_id).await
          }
      }
  }

  async fn execute_click(
      session: &impl cdpkit::Sender,
      params: &ActParams,
      target_id: &str,
      _state: &Arc<DaemonState>,
  ) -> Response {
      // Resolve element position from ref (backendNodeId) or use x/y coordinates
      let (x, y) = if let Some(ref_id) = params.ref_id {
          match resolve_element_position(session, ref_id).await {
              Ok(pos) => pos,
              Err(resp) => return resp,
          }
      } else {
          (params.x.unwrap(), params.y.unwrap())
      };

      // Dispatch click: mousePressed + mouseReleased
      let click_result = dispatch_click(session, x, y).await;
      match click_result {
          Ok(()) => build_act_response("click", params.ref_id, "completed", None, None, target_id),
          Err(e) => Response::error_detail(ErrorCode::JsError, format!("click failed: {e}"), None),
      }
  }

  async fn execute_type(
      session: &impl cdpkit::Sender,
      params: &ActParams,
      target_id: &str,
  ) -> Response {
      let ref_id = params.ref_id.unwrap();
      let text = params.text.as_deref().unwrap();

      // Focus the element first
      if let Err(resp) = focus_element(session, ref_id).await {
          return resp;
      }

      // Clear existing content (unless --append)
      if !params.append {
          if let Err(e) = clear_input(session, ref_id).await {
              return Response::error_detail(ErrorCode::JsError, format!("clear failed: {e}"), None);
          }
      }

      // Type text via CDP Input.insertText
      let type_result = cdpkit::input::methods::InsertText::new(text.into())
          .send(session).await;

      match type_result {
          Ok(_) => build_act_response("type", Some(ref_id), "completed", None, None, target_id),
          Err(e) => Response::error_detail(ErrorCode::JsError, format!("type failed: {e}"), None),
      }
  }

  async fn execute_press(
      session: &impl cdpkit::Sender,
      params: &ActParams,
      target_id: &str,
  ) -> Response {
      // Reuse existing keys handler logic from page/interaction.rs
      for key in &params.keys {
          if let Err(e) = dispatch_key(session, key).await {
              return Response::error_detail(ErrorCode::JsError, format!("press '{}' failed: {e}", key), None);
          }
      }
      build_act_response("press", None, "completed", None, None, target_id)
  }

  /// Resolve element center position from backendNodeId.
  async fn resolve_element_position(session: &impl cdpkit::Sender, backend_node_id: i64) -> Result<(f64, f64), Response> {
      // Use DOM.resolveNode + getContentQuads (same logic as existing click_by_target)
      let quads = cdpkit::dom::methods::GetContentQuads::new()
          .with_backend_node_id(backend_node_id)
          .send(session).await
          .map_err(|_| Response::error_detail(ErrorCode::RefNotFound, format!("element ref {} not found", backend_node_id), None))?;

      let quad = quads.quads.first()
          .ok_or_else(|| Response::error_detail(ErrorCode::ElementNotVisible, "element has no visible area".into(), None))?;

      // Calculate center from quad points
      let (cx, cy) = quad_center(quad);
      Ok((cx, cy))
  }

  fn quad_center(quad: &[f64]) -> (f64, f64) {
      if quad.len() >= 8 {
          let cx = (quad[0] + quad[2] + quad[4] + quad[6]) / 4.0;
          let cy = (quad[1] + quad[3] + quad[5] + quad[7]) / 4.0;
          (cx, cy)
      } else {
          (0.0, 0.0)
      }
  }

  async fn dispatch_click(session: &impl cdpkit::Sender, x: f64, y: f64) -> Result<(), crate::error::BkError> {
      cdpkit::input::methods::DispatchMouseEvent::new("mousePressed".into(), x, y)
          .with_button("left".into())
          .with_click_count(1)
          .send(session).await?;
      cdpkit::input::methods::DispatchMouseEvent::new("mouseReleased".into(), x, y)
          .with_button("left".into())
          .with_click_count(1)
          .send(session).await?;
      Ok(())
  }

  async fn focus_element(session: &impl cdpkit::Sender, backend_node_id: i64) -> Result<(), Response> {
      cdpkit::dom::methods::Focus::new()
          .with_backend_node_id(backend_node_id)
          .send(session).await
          .map_err(|_| Response::error_detail(ErrorCode::RefNotFound, format!("cannot focus ref {}", backend_node_id), None))?;
      Ok(())
  }

  async fn clear_input(session: &impl cdpkit::Sender, _backend_node_id: i64) -> Result<(), crate::error::BkError> {
      // Select all + delete (React-compatible approach from existing code)
      dispatch_key(session, "Control+a").await?;
      dispatch_key(session, "Backspace").await?;
      Ok(())
  }

  async fn dispatch_key(session: &impl cdpkit::Sender, key: &str) -> Result<(), crate::error::BkError> {
      // Parse combo keys like "Control+a"
      // Reuse existing key dispatch logic from page/interaction.rs
      // Simplified: dispatch keyDown + keyUp for each key in the combo
      crate::page::interaction::dispatch_key_combo(session, key).await
  }
  ```

  Note: `dispatch_key_combo`, `dispatch_click`, `focus_element` etc. adapt from existing `src/page/interaction.rs` functions. The exact function names may differ -- review and align during implementation.

- [x] **10.4 Add route**

  In `src/daemon/handler/mod.rs`:

  ```rust
  pub mod act_v2;

  "act" | "v2.act" => act_v2::handle_act_v2(req, state).await,
  ```

- [x] **10.5 Build and run tests**

  ```bash
  cargo build
  cargo test --lib handler::act_v2::tests
  ```

- [x] **10.6 Commit**

  ```bash
  git add src/daemon/handler/act_v2.rs src/daemon/handler/mod.rs
  git commit -m "feat: implement bk act click/type/press with ref-based element addressing"
  ```

---

## Task 11: state_diff Calculation

**Goal:** After each act operation, compare URL/title/element count before and after to produce a state_diff object. The diff tells the agent what changed without requiring a full snapshot.

**Files to create/modify:**
- `src/page/state_diff.rs` -- **new file**: StateDiff struct + capture/compare logic
- `src/page/mod.rs` -- re-export state_diff
- `src/daemon/handler/act_v2.rs` -- integrate state_diff into act responses

### Steps

- [x] **11.1 Write tests**

  Create `src/page/state_diff.rs` with tests:

  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;

      #[test]
      fn state_snapshot_captures_fields() {
          let snap = StateSnapshot {
              url: "https://example.com".into(),
              title: "Example".into(),
              element_count: 42,
          };
          assert_eq!(snap.url, "https://example.com");
          assert_eq!(snap.title, "Example");
          assert_eq!(snap.element_count, 42);
      }

      #[test]
      fn compute_diff_no_change() {
          let before = StateSnapshot { url: "https://a.com".into(), title: "A".into(), element_count: 10 };
          let after = StateSnapshot { url: "https://a.com".into(), title: "A".into(), element_count: 10 };
          let diff = compute_state_diff(&before, &after);
          assert!(diff.url_changed.is_none());
          assert!(diff.title_changed.is_none());
          assert_eq!(diff.elements_added, 0);
          assert_eq!(diff.elements_removed, 0);
      }

      #[test]
      fn compute_diff_url_changed() {
          let before = StateSnapshot { url: "https://a.com/login".into(), title: "Login".into(), element_count: 5 };
          let after = StateSnapshot { url: "https://a.com/dashboard".into(), title: "Dashboard".into(), element_count: 15 };
          let diff = compute_state_diff(&before, &after);
          let url_change = diff.url_changed.unwrap();
          assert_eq!(url_change.from, "https://a.com/login");
          assert_eq!(url_change.to, "https://a.com/dashboard");
          let title_change = diff.title_changed.unwrap();
          assert_eq!(title_change.from, "Login");
          assert_eq!(title_change.to, "Dashboard");
          assert_eq!(diff.elements_added, 10);
          assert_eq!(diff.elements_removed, 0);
      }

      #[test]
      fn compute_diff_elements_removed() {
          let before = StateSnapshot { url: "https://a.com".into(), title: "A".into(), element_count: 20 };
          let after = StateSnapshot { url: "https://a.com".into(), title: "A".into(), element_count: 12 };
          let diff = compute_state_diff(&before, &after);
          assert_eq!(diff.elements_added, 0);
          assert_eq!(diff.elements_removed, 8);
      }

      #[test]
      fn state_diff_to_json() {
          let diff = StateDiff {
              url_changed: Some(UrlChange { from: "https://a.com".into(), to: "https://b.com".into() }),
              title_changed: None,
              elements_added: 3,
              elements_removed: 1,
          };
          let json = diff.to_json();
          assert_eq!(json["url_changed"]["from"], "https://a.com");
          assert_eq!(json["url_changed"]["to"], "https://b.com");
          assert!(json["title_changed"].is_null());
          assert_eq!(json["elements_added"], 3);
          assert_eq!(json["elements_removed"], 1);
      }

      #[test]
      fn state_diff_null_when_no_changes() {
          let diff = StateDiff {
              url_changed: None,
              title_changed: None,
              elements_added: 0,
              elements_removed: 0,
          };
          let json = diff.to_json();
          // Even with no changes, return the full structure (agent expects consistent shape)
          assert_eq!(json["elements_added"], 0);
      }
  }
  ```

- [x] **11.2 Run tests (expect failure)**

  ```bash
  cargo test --lib page::state_diff::tests 2>&1 | head -20
  ```

- [x] **11.3 Implement StateDiff**

  Create `src/page/state_diff.rs`:

  ```rust
  use serde::Serialize;
  use serde_json::json;

  /// A lightweight snapshot of page state for diff computation.
  #[derive(Debug, Clone)]
  pub struct StateSnapshot {
      pub url: String,
      pub title: String,
      pub element_count: usize,
  }

  /// URL change record.
  #[derive(Debug, Clone, Serialize)]
  pub struct UrlChange {
      pub from: String,
      pub to: String,
  }

  /// Title change record.
  #[derive(Debug, Clone, Serialize)]
  pub struct TitleChange {
      pub from: String,
      pub to: String,
  }

  /// The diff between pre-action and post-action page state.
  #[derive(Debug, Clone)]
  pub struct StateDiff {
      pub url_changed: Option<UrlChange>,
      pub title_changed: Option<TitleChange>,
      pub elements_added: i64,
      pub elements_removed: i64,
  }

  /// Compare two state snapshots and produce a diff.
  pub fn compute_state_diff(before: &StateSnapshot, after: &StateSnapshot) -> StateDiff {
      let url_changed = if before.url != after.url {
          Some(UrlChange { from: before.url.clone(), to: after.url.clone() })
      } else {
          None
      };

      let title_changed = if before.title != after.title {
          Some(TitleChange { from: before.title.clone(), to: after.title.clone() })
      } else {
          None
      };

      let count_diff = after.element_count as i64 - before.element_count as i64;
      let (elements_added, elements_removed) = if count_diff >= 0 {
          (count_diff, 0)
      } else {
          (0, -count_diff)
      };

      StateDiff { url_changed, title_changed, elements_added, elements_removed }
  }

  impl StateDiff {
      /// Serialize to JSON value for inclusion in act responses.
      pub fn to_json(&self) -> serde_json::Value {
          json!({
              "url_changed": self.url_changed.as_ref().map(|c| json!({"from": c.from, "to": c.to})),
              "title_changed": self.title_changed.as_ref().map(|c| json!({"from": c.from, "to": c.to})),
              "elements_added": self.elements_added,
              "elements_removed": self.elements_removed,
          })
      }
  }

  /// Capture a lightweight state snapshot from a CDP session.
  /// Uses Runtime.evaluate to get URL, title, and interactive element count.
  pub async fn capture_state_snapshot(session: &impl cdpkit::Sender) -> Result<StateSnapshot, crate::error::BkError> {
      let js = r#"JSON.stringify({
          url: window.location.href,
          title: document.title,
          count: document.querySelectorAll('a,button,input,select,textarea,[role="button"],[role="link"],[role="checkbox"],[role="radio"],[role="tab"],[contenteditable="true"]').length
      })"#;

      let result = cdpkit::runtime::methods::Evaluate::new(js.into())
          .send(session).await
          .map_err(|e| crate::error::BkError::JsError(format!("state snapshot eval failed: {e}")))?;

      let json_str = result.result.value
          .and_then(|v| v.as_str().map(|s| s.to_string()))
          .ok_or_else(|| crate::error::BkError::JsError("state snapshot returned non-string".into()))?;

      let parsed: serde_json::Value = serde_json::from_str(&json_str)
          .map_err(|e| crate::error::BkError::JsError(format!("state snapshot parse error: {e}")))?;

      Ok(StateSnapshot {
          url: parsed["url"].as_str().unwrap_or("").to_string(),
          title: parsed["title"].as_str().unwrap_or("").to_string(),
          element_count: parsed["count"].as_u64().unwrap_or(0) as usize,
      })
  }
  ```

- [x] **11.4 Register module in src/page/mod.rs**

  Add `pub mod state_diff;`

- [x] **11.5 Integrate into act_v2.rs**

  In `handle_act_v2`, wrap the action execution with before/after snapshots:

  ```rust
  use crate::page::state_diff::{capture_state_snapshot, compute_state_diff};

  // Before action:
  let before = if !params.no_state_diff {
      capture_state_snapshot(&cdp_session).await.ok()
  } else {
      None
  };

  // ... execute action ...

  // After action (wait 500ms DOM stable window):
  let state_diff_json = if let Some(before) = before {
      tokio::time::sleep(std::time::Duration::from_millis(500)).await;
      match capture_state_snapshot(&cdp_session).await {
          Ok(after) => Some(compute_state_diff(&before, &after).to_json()),
          Err(_) => None,
      }
  } else {
      None
  };

  // Include in response:
  build_act_response("click", params.ref_id, "completed", state_diff_json, None, &target_id)
  ```

- [x] **11.6 Build and run tests**

  ```bash
  cargo build
  cargo test --lib page::state_diff::tests
  cargo test --lib handler::act_v2::tests
  ```

- [x] **11.7 Commit**

  ```bash
  git add src/page/state_diff.rs src/page/mod.rs src/daemon/handler/act_v2.rs
  git commit -m "feat: add state_diff computation for act responses (URL/title/element count)"
  ```

---

## Task 12: `bk tabs` / `bk close`

**Goal:** List tabs in current session and close a specific tab. Only agent-created tabs are visible.

**Files to create/modify:**
- `src/daemon/handler/tabs_v2.rs` -- **new file**: handle_tabs_v2 and handle_close_v2
- `src/daemon/handler/mod.rs` -- add routes

### Steps

- [x] **12.1 Write tests**

  In `src/daemon/handler/tabs_v2.rs`:

  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;
      use crate::daemon::state::DaemonState;
      use crate::daemon::session::Session;
      use std::sync::Arc;

      #[test]
      fn tabs_response_format() {
          let state = Arc::new(DaemonState::new());
          let mut session = Session::new_default("localhost:9222".into());
          session.add_tab("T1".into(), "https://a.com".into(), "A".into());
          session.add_tab("T2".into(), "https://b.com".into(), "B".into());
          state.sessions.insert("default".into(), session);

          let resp = build_tabs_response(&state, "default").unwrap();
          let json = serde_json::to_value(&resp).unwrap();
          assert_eq!(json["data"]["session"], "default");
          assert_eq!(json["data"]["active_target"], "T2");
          let tabs = json["data"]["tabs"].as_array().unwrap();
          assert_eq!(tabs.len(), 2);
          assert_eq!(tabs[0]["target"], "T1");
          assert_eq!(tabs[0]["active"], false);
          assert_eq!(tabs[1]["target"], "T2");
          assert_eq!(tabs[1]["active"], true);
      }

      #[test]
      fn tabs_empty_session() {
          let state = Arc::new(DaemonState::new());
          let session = Session::new_default("localhost:9222".into());
          state.sessions.insert("default".into(), session);

          let resp = build_tabs_response(&state, "default").unwrap();
          let json = serde_json::to_value(&resp).unwrap();
          let tabs = json["data"]["tabs"].as_array().unwrap();
          assert_eq!(tabs.len(), 0);
      }

      #[test]
      fn tabs_session_not_found() {
          let state = Arc::new(DaemonState::new());
          let err = build_tabs_response(&state, "nonexistent").unwrap_err();
          let json = serde_json::to_value(&err).unwrap();
          assert_eq!(json["error"]["code"], "SESSION_NOT_FOUND");
      }

      #[test]
      fn close_removes_tab_and_updates_active() {
          let state = Arc::new(DaemonState::new());
          let mut session = Session::new_default("localhost:9222".into());
          session.add_tab("T1".into(), "https://a.com".into(), "A".into());
          session.add_tab("T2".into(), "https://b.com".into(), "B".into());
          state.sessions.insert("default".into(), session);

          // Close T2 (the active one)
          close_tab_in_session(&state, "default", "T2");

          let session = state.sessions.get("default").unwrap();
          assert_eq!(session.tab_count(), 1);
          assert_eq!(session.active_target, Some("T1".into()));
      }

      #[test]
      fn close_last_tab_leaves_no_active() {
          let state = Arc::new(DaemonState::new());
          let mut session = Session::new_default("localhost:9222".into());
          session.add_tab("T1".into(), "https://a.com".into(), "A".into());
          state.sessions.insert("default".into(), session);

          close_tab_in_session(&state, "default", "T1");

          let session = state.sessions.get("default").unwrap();
          assert_eq!(session.tab_count(), 0);
          assert_eq!(session.active_target, None);
      }
  }
  ```

- [x] **12.2 Run tests (expect failure)**

  ```bash
  cargo test --lib handler::tabs_v2::tests 2>&1 | head -20
  ```

- [x] **12.3 Implement tabs/close handlers**

  Create `src/daemon/handler/tabs_v2.rs`:

  ```rust
  use std::sync::Arc;
  use serde_json::json;
  use crate::daemon::protocol::{Request, Response};
  use crate::daemon::state::DaemonState;
  use crate::error::ErrorCode;

  fn build_tabs_response(state: &Arc<DaemonState>, session_name: &str) -> Result<Response, Response> {
      let session = state.sessions.get(session_name)
          .ok_or_else(|| Response::error_detail(
              ErrorCode::SessionNotFound,
              format!("session '{}' not found", session_name),
              None,
          ))?;

      let active = session.active_target.as_deref();
      let tabs: Vec<serde_json::Value> = session.tabs.values().map(|tab| {
          json!({
              "target": tab.target_id,
              "url": tab.url,
              "title": tab.title,
              "active": active == Some(tab.target_id.as_str()),
          })
      }).collect();

      Ok(Response::ok(json!({
          "session": session_name,
          "active_target": active,
          "tabs": tabs,
      })))
  }

  fn close_tab_in_session(state: &Arc<DaemonState>, session_name: &str, target_id: &str) {
      if let Some(mut session) = state.sessions.get_mut(session_name) {
          session.remove_tab(target_id);
      }
  }

  pub async fn handle_tabs_v2(req: &Request, state: &Arc<DaemonState>) -> Response {
      let session_name = req.params.get("session")
          .and_then(|v| v.as_str())
          .unwrap_or("default");

      match build_tabs_response(state, session_name) {
          Ok(resp) => resp,
          Err(resp) => resp,
      }
  }

  pub async fn handle_close_v2(req: &Request, state: &Arc<DaemonState>) -> Response {
      let session_name = req.params.get("session")
          .and_then(|v| v.as_str())
          .unwrap_or("default");

      let session = match state.sessions.get(session_name) {
          Some(s) => s,
          None => return Response::error_detail(ErrorCode::SessionNotFound, format!("session '{}' not found", session_name), None),
      };

      if let Err(resp) = session.check_connected() { return resp; }

      // Determine target to close
      let target_id = req.params.get("target")
          .and_then(|v| v.as_str())
          .map(|s| s.to_string())
          .or_else(|| session.active_target.clone());

      let target_id = match target_id {
          Some(t) => t,
          None => return Response::error_detail(ErrorCode::SessionNoTab, "no tab to close".into(), None),
      };

      let browser_host = session.browser_host.clone();
      drop(session);

      // Close via CDP
      let cdp = match state.browsers.get(&browser_host) {
          Some(b) => Arc::clone(&b.cdp),
          None => return Response::error_detail(ErrorCode::ChromeDisconnected, "disconnected".into(), None),
      };

      let _ = cdpkit::target::methods::CloseTarget::new(target_id.clone())
          .send(cdp.as_ref()).await;

      // Update session state
      close_tab_in_session(state, session_name, &target_id);
      state.request_persist();

      Response::ok(json!({
          "closed": target_id,
          "session": session_name,
      }))
  }
  ```

- [x] **12.4 Add routes**

  In `src/daemon/handler/mod.rs`:

  ```rust
  pub mod tabs_v2;

  "tabs" | "v2.tabs" => tabs_v2::handle_tabs_v2(req, state).await,
  "close" | "v2.close" => tabs_v2::handle_close_v2(req, state).await,
  ```

- [x] **12.5 Build and run tests**

  ```bash
  cargo build
  cargo test --lib handler::tabs_v2::tests
  ```

- [x] **12.6 Commit**

  ```bash
  git add src/daemon/handler/tabs_v2.rs src/daemon/handler/mod.rs
  git commit -m "feat: implement bk tabs and bk close for session tab management"
  ```

---

## Task 13: `bk session` Subcommands (close/list/cookies)

**Goal:** Session lifecycle management: close (dispose BrowserContext), list all sessions, and cookie operations.

**Files to create/modify:**
- `src/daemon/handler/session_v2.rs` -- **new file**: session close/list/cookies handlers
- `src/daemon/handler/mod.rs` -- add routes

### Steps

- [x] **13.1 Write tests**

  In `src/daemon/handler/session_v2.rs`:

  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;
      use crate::daemon::state::DaemonState;
      use crate::daemon::session::Session;
      use std::sync::Arc;

      #[test]
      fn session_list_response_format() {
          let state = Arc::new(DaemonState::new());
          let mut default_session = Session::new_default("localhost:9222".into());
          default_session.add_tab("T1".into(), "https://a.com".into(), "A".into());
          state.sessions.insert("default".into(), default_session);

          let isolated = Session::new_isolated("agent-a".into(), "localhost:9222".into(), "CTX1".into());
          state.sessions.insert("agent-a".into(), isolated);

          let resp = build_session_list_response(&state);
          let json = serde_json::to_value(&resp).unwrap();
          let sessions = json["data"]["sessions"].as_array().unwrap();
          assert_eq!(sessions.len(), 2);

          // Find the default session entry
          let def = sessions.iter().find(|s| s["name"] == "default").unwrap();
          assert_eq!(def["mode"], "default");
          assert_eq!(def["tabs"], 1);

          let iso = sessions.iter().find(|s| s["name"] == "agent-a").unwrap();
          assert_eq!(iso["mode"], "isolated");
          assert_eq!(iso["tabs"], 0);
      }

      #[test]
      fn session_list_empty() {
          let state = Arc::new(DaemonState::new());
          let resp = build_session_list_response(&state);
          let json = serde_json::to_value(&resp).unwrap();
          let sessions = json["data"]["sessions"].as_array().unwrap();
          assert_eq!(sessions.len(), 0);
      }

      #[test]
      fn session_close_removes_session() {
          let state = Arc::new(DaemonState::new());
          let session = Session::new_isolated("agent-a".into(), "localhost:9222".into(), "CTX1".into());
          state.sessions.insert("agent-a".into(), session);

          remove_session(&state, "agent-a");
          assert!(!state.sessions.contains_key("agent-a"));
      }

      #[test]
      fn session_close_default_only_removes_tabs() {
          let state = Arc::new(DaemonState::new());
          let mut session = Session::new_default("localhost:9222".into());
          session.add_tab("T1".into(), "https://a.com".into(), "A".into());
          session.add_tab("T2".into(), "https://b.com".into(), "B".into());
          state.sessions.insert("default".into(), session);

          clear_default_session_tabs(&state);
          let session = state.sessions.get("default").unwrap();
          assert_eq!(session.tab_count(), 0);
          assert_eq!(session.active_target, None);
          // Session itself still exists
          assert!(state.sessions.contains_key("default"));
      }

      #[test]
      fn session_limit_check() {
          let state = Arc::new(DaemonState::new());
          for i in 0..10 {
              let s = Session::new_isolated(format!("s{i}"), "localhost:9222".into(), format!("CTX{i}"));
              state.sessions.insert(format!("s{i}"), s);
          }
          let result = check_session_limit(&state, 10);
          assert!(result.is_err());
          let json = serde_json::to_value(&result.unwrap_err()).unwrap();
          assert_eq!(json["error"]["code"], "SESSION_LIMIT_EXCEEDED");
      }
  }
  ```

- [x] **13.2 Run tests (expect failure)**

  ```bash
  cargo test --lib handler::session_v2::tests 2>&1 | head -20
  ```

- [x] **13.3 Implement session handlers**

  Create `src/daemon/handler/session_v2.rs`:

  ```rust
  use std::sync::Arc;
  use serde_json::json;
  use crate::daemon::protocol::{Request, Response};
  use crate::daemon::state::DaemonState;
  use crate::daemon::session::SessionMode;
  use crate::error::ErrorCode;

  fn build_session_list_response(state: &Arc<DaemonState>) -> Response {
      let sessions: Vec<serde_json::Value> = state.sessions.iter().map(|entry| {
          let s = entry.value();
          json!({
              "name": s.name,
              "mode": s.mode,
              "tabs": s.tab_count(),
              "last_active": s.last_active,
              "browser_host": s.browser_host,
          })
      }).collect();

      Response::ok(json!({ "sessions": sessions }))
  }

  fn remove_session(state: &Arc<DaemonState>, name: &str) {
      state.sessions.remove(name);
  }

  fn clear_default_session_tabs(state: &Arc<DaemonState>) {
      if let Some(mut session) = state.sessions.get_mut("default") {
          session.tabs.clear();
          session.active_target = None;
      }
  }

  fn check_session_limit(state: &Arc<DaemonState>, max: usize) -> Result<(), Response> {
      if max == 0 { return Ok(()); }
      // Count only isolated sessions (default doesn't count toward limit)
      let count = state.sessions.iter()
          .filter(|e| e.value().mode == SessionMode::Isolated)
          .count();
      if count >= max {
          return Err(Response::error_detail(
              ErrorCode::SessionLimitExceeded,
              format!("already have {} isolated sessions (limit: {})", count, max),
              None,
          ));
      }
      Ok(())
  }

  pub async fn handle_session_close(req: &Request, state: &Arc<DaemonState>) -> Response {
      let session_name = req.params.get("session")
          .and_then(|v| v.as_str())
          .unwrap_or("default");

      if session_name == "default" {
          // Close all tabs in default session, but keep session alive
          // First close tabs via CDP
          if let Some(session) = state.sessions.get("default") {
              let targets: Vec<String> = session.tabs.keys().cloned().collect();
              let browser_host = session.browser_host.clone();
              drop(session);

              if let Some(browser) = state.browsers.get(&browser_host) {
                  for tid in &targets {
                      let _ = cdpkit::target::methods::CloseTarget::new(tid.clone())
                          .send(browser.cdp.as_ref()).await;
                  }
              }
          }
          clear_default_session_tabs(state);
      } else {
          // Close isolated session: close tabs + dispose BrowserContext
          let session = match state.sessions.get(session_name) {
              Some(s) => s,
              None => return Response::error_detail(ErrorCode::SessionNotFound, format!("session '{}' not found", session_name), None),
          };

          let targets: Vec<String> = session.tabs.keys().cloned().collect();
          let browser_host = session.browser_host.clone();
          let ctx_id = session.browser_context_id.clone();
          drop(session);

          if let Some(browser) = state.browsers.get(&browser_host) {
              // Close tabs
              for tid in &targets {
                  let _ = cdpkit::target::methods::CloseTarget::new(tid.clone())
                      .send(browser.cdp.as_ref()).await;
              }
              // Dispose BrowserContext
              if let Some(ctx) = ctx_id {
                  let _ = cdpkit::target::methods::DisposeBrowserContext::new(ctx)
                      .send(browser.cdp.as_ref()).await;
              }
          }

          remove_session(state, session_name);
      }

      state.request_persist();
      Response::ok(json!({ "closed": session_name }))
  }

  pub async fn handle_session_list(_req: &Request, state: &Arc<DaemonState>) -> Response {
      build_session_list_response(state)
  }

  pub async fn handle_session_cookies_get(req: &Request, state: &Arc<DaemonState>) -> Response {
      let session_name = req.params.get("session").and_then(|v| v.as_str()).unwrap_or("default");

      let session = match state.sessions.get(session_name) {
          Some(s) => s,
          None => return Response::error_detail(ErrorCode::SessionNotFound, format!("session '{}' not found", session_name), None),
      };
      let browser_host = session.browser_host.clone();
      let ctx_id = session.browser_context_id.clone();
      drop(session);

      let cdp = match state.browsers.get(&browser_host) {
          Some(b) => Arc::clone(&b.cdp),
          None => return Response::error_detail(ErrorCode::ChromeDisconnected, "disconnected".into(), None),
      };

      // Get cookies for the BrowserContext
      let mut get_cookies = cdpkit::network::methods::GetCookies::new();
      if let Some(_ctx) = &ctx_id {
          // Note: GetCookies doesn't directly take browserContextId.
          // For isolated sessions, we need a session from a tab in that context.
          // For now, send at browser level (gets all cookies)
      }

      match get_cookies.send(cdp.as_ref()).await {
          Ok(result) => Response::ok(json!({ "cookies": result.cookies })),
          Err(e) => Response::error_detail(ErrorCode::DaemonError, format!("get cookies failed: {e}"), None),
      }
  }

  pub async fn handle_session_cookies_set(req: &Request, state: &Arc<DaemonState>) -> Response {
      let session_name = req.params.get("session").and_then(|v| v.as_str()).unwrap_or("default");
      let cookies = match req.params.get("cookies").and_then(|v| v.as_array()) {
          Some(c) => c.clone(),
          None => return Response::error_detail(ErrorCode::InvalidArgument, "missing cookies array".into(), None),
      };

      let session = match state.sessions.get(session_name) {
          Some(s) => s,
          None => return Response::error_detail(ErrorCode::SessionNotFound, format!("session '{}' not found", session_name), None),
      };
      let browser_host = session.browser_host.clone();
      drop(session);

      let cdp = match state.browsers.get(&browser_host) {
          Some(b) => Arc::clone(&b.cdp),
          None => return Response::error_detail(ErrorCode::ChromeDisconnected, "disconnected".into(), None),
      };

      // Set cookies via CDP Network.setCookies
      match cdpkit::network::methods::SetCookies::new(cookies)
          .send(cdp.as_ref()).await {
          Ok(_) => Response::ok(json!({ "set": true })),
          Err(e) => Response::error_detail(ErrorCode::DaemonError, format!("set cookies failed: {e}"), None),
      }
  }

  pub async fn handle_session_cookies_clear(req: &Request, state: &Arc<DaemonState>) -> Response {
      let session_name = req.params.get("session").and_then(|v| v.as_str()).unwrap_or("default");

      let session = match state.sessions.get(session_name) {
          Some(s) => s,
          None => return Response::error_detail(ErrorCode::SessionNotFound, format!("session '{}' not found", session_name), None),
      };
      let browser_host = session.browser_host.clone();
      drop(session);

      let cdp = match state.browsers.get(&browser_host) {
          Some(b) => Arc::clone(&b.cdp),
          None => return Response::error_detail(ErrorCode::ChromeDisconnected, "disconnected".into(), None),
      };

      match cdpkit::network::methods::ClearBrowserCookies::new()
          .send(cdp.as_ref()).await {
          Ok(_) => Response::ok(json!({ "cleared": true })),
          Err(e) => Response::error_detail(ErrorCode::DaemonError, format!("clear cookies failed: {e}"), None),
      }
  }
  ```

- [x] **13.4 Add routes**

  In `src/daemon/handler/mod.rs`:

  ```rust
  pub mod session_v2;

  "session.close" | "v2.session.close" => session_v2::handle_session_close(req, state).await,
  "session.list" | "v2.session.list" => session_v2::handle_session_list(req, state).await,
  "session.cookies.get" => session_v2::handle_session_cookies_get(req, state).await,
  "session.cookies.set" => session_v2::handle_session_cookies_set(req, state).await,
  "session.cookies.clear" => session_v2::handle_session_cookies_clear(req, state).await,
  ```

- [x] **13.5 Build and run tests**

  ```bash
  cargo build
  cargo test --lib handler::session_v2::tests
  ```

- [x] **13.6 Commit**

  ```bash
  git add src/daemon/handler/session_v2.rs src/daemon/handler/mod.rs
  git commit -m "feat: implement session close/list/cookies subcommands"
  ```

---

## Task 14: CLI Restructure + Deprecated Aliases

**Goal:** Rebuild `src/main.rs` with v2 command structure: new top-level commands (setup/connect/snapshot/act/navigate/open/close/tabs/session), global params (--session/--target/--timeout/--no-state-diff/--focus), remove --format/--ws, always JSON output. Add deprecated aliases that print stderr warning then execute the new command.

**Files to modify:**
- `src/main.rs` -- complete rewrite of Command enum and dispatch logic
- `src/client.rs` -- remove format logic, always print raw JSON response

### Steps

- [x] **14.1 Write tests**

  In a new test module at bottom of `src/main.rs` (or as integration test):

  ```rust
  #[cfg(test)]
  mod cli_tests {
      use super::*;
      use clap::Parser;

      #[test]
      fn cli_parses_connect() {
          let cli = Cli::try_parse_from(["bk", "connect"]).unwrap();
          assert!(matches!(cli.command, Command::Connect { .. }));
      }

      #[test]
      fn cli_parses_connect_with_session() {
          let cli = Cli::try_parse_from(["bk", "connect", "--session", "agent-a"]).unwrap();
          assert_eq!(cli.session, Some("agent-a".into()));
      }

      #[test]
      fn cli_parses_snapshot() {
          let cli = Cli::try_parse_from(["bk", "snapshot"]).unwrap();
          assert!(matches!(cli.command, Command::Snapshot { .. }));
      }

      #[test]
      fn cli_parses_snapshot_full() {
          let cli = Cli::try_parse_from(["bk", "snapshot", "--full"]).unwrap();
          if let Command::Snapshot { full, .. } = cli.command {
              assert!(full);
          } else { panic!("wrong variant"); }
      }

      #[test]
      fn cli_parses_act_click() {
          let cli = Cli::try_parse_from(["bk", "act", "click", "--ref", "42"]).unwrap();
          if let Command::Act { kind, .. } = &cli.command {
              assert_eq!(kind.as_deref(), Some("click"));
          }
      }

      #[test]
      fn cli_parses_navigate_url() {
          let cli = Cli::try_parse_from(["bk", "navigate", "https://example.com"]).unwrap();
          if let Command::Navigate { url, .. } = &cli.command {
              assert_eq!(url.as_deref(), Some("https://example.com"));
          }
      }

      #[test]
      fn cli_parses_navigate_back() {
          let cli = Cli::try_parse_from(["bk", "navigate", "--back"]).unwrap();
          if let Command::Navigate { back, .. } = &cli.command {
              assert!(back);
          }
      }

      #[test]
      fn cli_parses_open() {
          let cli = Cli::try_parse_from(["bk", "open", "https://x.com"]).unwrap();
          if let Command::Open { url, .. } = &cli.command {
              assert_eq!(url, "https://x.com");
          }
      }

      #[test]
      fn cli_parses_session_close() {
          let cli = Cli::try_parse_from(["bk", "session", "close"]).unwrap();
          assert!(matches!(cli.command, Command::Session { .. }));
      }

      #[test]
      fn cli_global_session_param() {
          let cli = Cli::try_parse_from(["bk", "--session", "my-session", "snapshot"]).unwrap();
          assert_eq!(cli.session, Some("my-session".into()));
      }

      #[test]
      fn cli_global_target_param() {
          let cli = Cli::try_parse_from(["bk", "--target", "TAB123", "snapshot"]).unwrap();
          assert_eq!(cli.target, Some("TAB123".into()));
      }

      #[test]
      fn cli_global_timeout_param() {
          let cli = Cli::try_parse_from(["bk", "--timeout", "60000", "act", "click", "--ref", "5"]).unwrap();
          assert_eq!(cli.timeout, Some(60000));
      }

      // Deprecated aliases
      #[test]
      fn cli_parses_deprecated_goto() {
          let cli = Cli::try_parse_from(["bk", "goto", "https://a.com"]).unwrap();
          assert!(matches!(cli.command, Command::Goto { .. }));
      }

      #[test]
      fn cli_parses_deprecated_info() {
          let cli = Cli::try_parse_from(["bk", "info"]).unwrap();
          assert!(matches!(cli.command, Command::Info));
      }

      #[test]
      fn cli_no_format_flag() {
          // --format should not be recognized
          let result = Cli::try_parse_from(["bk", "--format", "text", "snapshot"]);
          assert!(result.is_err());
      }

      #[test]
      fn cli_no_ws_flag() {
          // --ws should not be recognized
          let result = Cli::try_parse_from(["bk", "--ws", "abc", "snapshot"]);
          assert!(result.is_err());
      }
  }
  ```

- [x] **14.2 Run tests (expect failure)**

  ```bash
  cargo test --lib cli_tests 2>&1 | head -30
  ```

- [x] **14.3 Rewrite CLI structure in src/main.rs**

  New top-level `Cli` struct (replaces existing):

  ```rust
  #[derive(Parser)]
  #[command(name = "bk", about = "Browser automation CLI for LLM agents", version)]
  pub struct Cli {
      /// Target session name (or set BK_SESSION env var)
      #[arg(long = "session", global = true, env = "BK_SESSION")]
      pub session: Option<String>,

      /// Target tab (targetId)
      #[arg(long = "target", global = true)]
      pub target: Option<String>,

      /// Timeout in milliseconds
      #[arg(long = "timeout", global = true)]
      pub timeout: Option<u64>,

      /// Skip state_diff in act responses
      #[arg(long = "no-state-diff", global = true)]
      pub no_state_diff: bool,

      /// Bring tab to foreground
      #[arg(long = "focus", global = true)]
      pub focus: bool,

      #[command(subcommand)]
      pub command: Command,
  }

  #[derive(Subcommand)]
  pub enum Command {
      // ── Primary commands ────────────────────────────────────
      /// Set up Chrome remote debugging (interactive, one-time)
      Setup,
      /// Connect to browser (idempotent)
      Connect,
      /// Get page state (elements + text + viewport)
      Snapshot {
          #[arg(long)] full: bool,
          #[arg(long)] no_page_text: bool,
          #[arg(long, default_value = "dom-stable")] wait: String,
      },
      /// Execute interaction (click/type/press/scroll/select/fill/hover/focus/drag/upload/dialog)
      Act {
          /// Action kind
          kind: Option<String>,
          /// Element ref (backendNodeId)
          #[arg(long)] r#ref: Option<i64>,
          /// Text for type action
          #[arg(long)] text: Option<String>,
          /// Append mode for type (default: replace)
          #[arg(long)] append: bool,
          /// Keys for press action
          #[arg(long, num_args = 1..)] keys: Vec<String>,
          /// X coordinate for click
          #[arg(long)] x: Option<f64>,
          /// Y coordinate for click
          #[arg(long)] y: Option<f64>,
      },
      /// Navigate to URL or back/forward/reload
      Navigate {
          /// Target URL
          url: Option<String>,
          #[arg(long)] back: bool,
          #[arg(long)] forward: bool,
          #[arg(long)] reload: bool,
      },
      /// Open URL in new tab
      Open { url: String },
      /// Close tab
      Close,
      /// List tabs in session
      Tabs,
      /// Evaluate JavaScript
      Evaluate { expression: Option<String>, #[arg(long)] file: Option<String> },
      /// Take screenshot
      Screenshot { #[arg(long)] output: Option<String>, #[arg(long)] full_page: bool },
      /// Wait for condition
      Wait {
          #[arg(long)] selector: Option<String>,
          #[arg(long)] text: Option<String>,
          #[arg(long)] text_gone: Option<String>,
          #[arg(long)] url: Option<String>,
          #[arg(long)] idle: bool,
          #[arg(long)] r#fn: Option<String>,
          #[arg(long)] time: Option<u64>,
      },
      /// Session management
      Session {
          #[command(subcommand)]
          action: SessionAction,
      },
      /// Connection status
      Status,

      // ── Internal commands (preserved) ──────────────────────
      /// Browser management
      Browser { #[command(subcommand)] action: BrowserAction },
      /// Daemon lifecycle
      Daemon { #[command(subcommand)] action: DaemonAction },

      // ── Deprecated aliases (emit stderr warning) ───────────
      /// [deprecated] Use 'navigate' instead
      #[command(hide = true)]
      Goto { url: String },
      /// [deprecated] Use 'snapshot' instead
      #[command(hide = true)]
      Info,
      /// [deprecated] Use 'evaluate' instead
      #[command(hide = true)]
      Eval { expression: Option<String> },
      /// [deprecated] Use 'screenshot' instead
      #[command(hide = true)]
      Shot { #[arg(long)] output: Option<String> },
      /// [deprecated] Use 'act click' instead
      #[command(hide = true)]
      Click { #[arg(long)] r#ref: Option<i64>, #[arg(long)] index: Option<usize> },
      /// [deprecated] Use 'act type' instead
      #[command(hide = true)]
      Type { #[arg(long)] r#ref: Option<i64>, text: Option<String> },
  }

  #[derive(Subcommand)]
  pub enum SessionAction {
      Close,
      List,
      Cookies { #[command(subcommand)] action: CookiesAction },
  }

  #[derive(Subcommand)]
  pub enum CookiesAction {
      Get,
      Set { #[arg(long)] file: String },
      Clear,
  }
  ```

- [x] **14.4 Implement dispatch logic**

  In `main()`, after parsing CLI:
  - For `Setup`: call `run_setup()` directly (no daemon)
  - For deprecated commands: print warning to stderr, then map to the equivalent v2 command
  - For all other commands: build Request JSON, send to daemon, print response JSON to stdout

  ```rust
  fn emit_deprecation_warning(old: &str, new: &str) {
      eprintln!("warning: '{}' is deprecated, use '{}' instead", old, new);
  }
  ```

  Dispatch mapping (deprecated -> v2):
  - `Goto { url }` -> `navigate` with `{"url": url}`
  - `Info` -> `snapshot` with `{}`
  - `Eval { expr }` -> `evaluate` with `{"expression": expr}`
  - `Shot { output }` -> `screenshot` with `{"output": output}`
  - `Click { ref }` -> `act` with `{"kind": "click", "ref": ref}`
  - `Type { ref, text }` -> `act` with `{"kind": "type", "ref": ref, "text": text}`

- [x] **14.5 Simplify client output**

  In `src/client.rs`, remove all format-related logic. The main function always:
  1. Sends request to daemon
  2. Receives Response JSON
  3. Prints it directly to stdout (`println!("{}", serde_json::to_string(&resp)?);`)

  No text formatting, no tsv, no colorization.

- [x] **14.6 Build and run tests**

  ```bash
  cargo build
  cargo test --lib cli_tests
  ```

- [x] **14.7 Commit**

  ```bash
  git add src/main.rs src/client.rs
  git commit -m "feat: restructure CLI for v2 with JSON-only output and deprecated aliases"
  ```

---

## Summary

After all 14 tasks are completed, browserkit has:

1. Structured error system with machine-readable codes
2. Daemon token authentication
3. Session abstraction (default + isolated BrowserContext)
4. Chrome crash detection via WebSocket monitoring
5. `bk connect` with Chrome/Edge auto-discovery
6. `bk setup` for interactive remote debugging configuration
7. `bk open` to create tabs in sessions
8. `bk snapshot` returning elements + page_text + viewport
9. `bk navigate` with back/forward/reload and SPA support
10. `bk act click/type/press` with ref-based addressing
11. state_diff on all act responses
12. `bk tabs` / `bk close` for tab management
13. `bk session close/list/cookies` for session lifecycle
14. CLI restructured with JSON-only output and deprecated aliases

All existing v1 functionality remains operational alongside (workspace handlers untouched). Phase 2 adds remaining act kinds, dialog auto-handling, session timeout, and --full snapshot mode. Phase 3 removes deprecated code.
