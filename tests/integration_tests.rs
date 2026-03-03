// Integration test suite
//
// Tests requiring Chrome are marked #[ignore].
// Run all tests (including Chrome ones) with:
//   cargo test --test integration_tests -- --include-ignored --test-threads=1

mod common;

use serde_json::json;

// ═══════════════════════════════════════════════════════════════════════════
// S1: Daemon Lifecycle (no Chrome needed)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn s1_ping_returns_running() {
    let daemon = common::DaemonFixture::start().await;
    let resp = daemon.send("ping", json!({})).await;
    assert!(resp.ok, "ping should succeed: {:?}", resp.error);
    assert_eq!(resp.data.unwrap()["status"], "running");
    daemon.stop().await;
}

#[tokio::test]
async fn s1_daemon_status_initial_state() {
    let daemon = common::DaemonFixture::start().await;
    let resp = daemon.send("daemon.status", json!({})).await;
    assert!(resp.ok, "daemon.status: {:?}", resp.error);
    let data = resp.data.unwrap();
    assert!(data["pid"].as_u64().unwrap_or(0) > 0, "pid > 0");
    assert_eq!(data["port"].as_u64().unwrap(), daemon.port as u64);
    assert_eq!(data["browsers"], 0);
    assert_eq!(data["workspaces"], 0);
    daemon.stop().await;
}

#[tokio::test]
async fn s1_daemon_stop_returns_stopping() {
    let daemon = common::DaemonFixture::start().await;
    let resp = daemon.send("daemon.stop", json!({})).await;
    assert!(resp.ok, "daemon.stop: {:?}", resp.error);
    assert_eq!(resp.data.unwrap()["status"], "stopping");
}

#[tokio::test]
async fn s1_after_stop_connections_fail() {
    let daemon = common::DaemonFixture::start().await;
    let port = daemon.port;
    daemon.send("daemon.stop", json!({})).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let result = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port)).await;
    assert!(result.is_err(), "connection should fail after daemon stop");
}

#[tokio::test]
async fn s1_multiple_requests_same_connection() {
    let daemon = common::DaemonFixture::start().await;
    let mut client = daemon.connect().await;

    let r1 = client.send("ping", json!({})).await;
    assert!(r1.ok, "ping: {:?}", r1.error);

    let r2 = client.send("daemon.status", json!({})).await;
    assert!(r2.ok, "daemon.status: {:?}", r2.error);

    let r3 = client.send("no.such.command", json!({})).await;
    assert!(!r3.ok, "unknown command should fail");
    assert!(r3.error.unwrap().contains("unknown command"));

    daemon.stop().await;
}

#[tokio::test]
async fn s1_concurrent_connections() {
    let daemon = common::DaemonFixture::start().await;
    let port = daemon.port;

    let mut handles = Vec::new();
    for _ in 0..5 {
        handles.push(tokio::spawn(async move {
            let stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
                .await
                .unwrap();
            let mut client = common::TestClient::new(stream);
            let resp = client.send("ping", json!({})).await;
            assert!(resp.ok);
            assert_eq!(resp.data.unwrap()["status"], "running");
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    daemon.stop().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// S2: Browser Connection Management (no Chrome needed for error cases)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn s2_browser_list_empty_initially() {
    let daemon = common::DaemonFixture::start().await;
    let resp = daemon.send("browser.list", json!({})).await;
    assert!(resp.ok, "browser.list: {:?}", resp.error);
    let data = resp.data.unwrap();
    assert!(data.is_array());
    assert_eq!(data.as_array().unwrap().len(), 0);
    daemon.stop().await;
}

#[tokio::test]
async fn s2_browser_connect_invalid_host_returns_error() {
    let daemon = common::DaemonFixture::start().await;
    let resp = daemon
        .send("browser.connect", json!({"host": "localhost:19999"}))
        .await;
    assert!(!resp.ok, "connecting to invalid host should fail");
    let err = resp.error.unwrap();
    assert!(
        err.contains("connection") || err.contains("failed") || err.contains("refused")
            || err.contains("Browser") || err.contains("connect"),
        "error should mention connection failure, got: {}",
        err
    );
    daemon.stop().await;
}

#[tokio::test]
async fn s2_browser_connect_missing_host_param() {
    let daemon = common::DaemonFixture::start().await;
    let resp = daemon.send("browser.connect", json!({})).await;
    assert!(!resp.ok);
    assert!(resp.error.unwrap().contains("host"));
    daemon.stop().await;
}

#[tokio::test]
async fn s2_browser_disconnect_missing_host_param() {
    let daemon = common::DaemonFixture::start().await;
    let resp = daemon.send("browser.disconnect", json!({})).await;
    assert!(!resp.ok);
    assert!(resp.error.unwrap().contains("host"));
    daemon.stop().await;
}

#[tokio::test]
async fn s2_browser_disconnect_nonexistent_host() {
    let daemon = common::DaemonFixture::start().await;
    let resp = daemon
        .send("browser.disconnect", json!({"host": "localhost:9999"}))
        .await;
    assert!(!resp.ok);
    daemon.stop().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// C9: wid Prefix Matching (no Chrome needed)
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn c9_wid_prefix_matching_no_chrome() {
    use browserkit::daemon::state::{resolve_wid, DaemonState};
    use browserkit::workspace::Workspace;
    use std::collections::HashMap;

    fn make_ws(wid: &str) -> Workspace {
        Workspace {
            wid: wid.to_string(),
            browser_host: "localhost:9222".to_string(),
            browser_context_id: format!("ctx-{}", wid),
            label: None,
            tabs: HashMap::new(),
            active_tab: None,
            created_at: 0,
            last_active: 0,
        }
    }

    let state = DaemonState::new();
    state.workspaces.insert("a3f2".to_string(), make_ws("a3f2"));
    state.workspaces.insert("b7e1".to_string(), make_ws("b7e1"));
    state.workspaces.insert("c9d4".to_string(), make_ws("c9d4"));

    // Exact match
    assert_eq!(resolve_wid(&state, "a3f2").unwrap(), "a3f2");

    // Unique prefix
    assert_eq!(resolve_wid(&state, "b7").unwrap(), "b7e1");

    // Non-existent
    let err = resolve_wid(&state, "zzzz").unwrap_err();
    assert!(err.to_string().contains("workspace not found"));

    // Ambiguous: add a3b1 to conflict with a3f2 on prefix "a3"
    state.workspaces.insert("a3b1".to_string(), make_ws("a3b1"));
    let err = resolve_wid(&state, "a3").unwrap_err();
    assert!(
        err.to_string().contains("ambiguous"),
        "should be ambiguous, got: {}",
        err
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// S3: Workspace Basic Operations (requires Chrome)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[ignore = "requires Chrome"]
async fn s3_workspace_lifecycle() {
    let daemon = common::DaemonFixture::start().await;

    // S3-1: ws.new auto-launches browser
    let resp = daemon.send("ws.new", json!({})).await;
    assert!(resp.ok, "ws.new: {:?}", resp.error);
    let data = resp.data.unwrap();
    let wid = data["wid"].as_str().unwrap().to_string();
    assert_eq!(wid.len(), 4, "wid should be 4 chars");
    assert!(data["active_tab"].as_str().is_some(), "active_tab should be set");

    // S3-2: ws.list returns 1 workspace
    let resp = daemon.send("ws.list", json!({})).await;
    assert!(resp.ok);
    let list = resp.data.unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["wid"], wid);

    // S3-3: ws.info returns correct details
    let resp = daemon.send("ws.info", json!({"wid": wid})).await;
    assert!(resp.ok, "ws.info: {:?}", resp.error);
    let info = resp.data.unwrap();
    assert_eq!(info["wid"], wid);
    assert_eq!(info["tabs"].as_array().unwrap().len(), 1);
    assert!(info["active_tab"].as_str().is_some());

    // S3-4: daemon.status shows 1 browser, 1 workspace
    let resp = daemon.send("daemon.status", json!({})).await;
    assert!(resp.ok);
    let status = resp.data.unwrap();
    assert_eq!(status["browsers"], 1);
    assert_eq!(status["workspaces"], 1);

    // S3-5: ws.close
    let resp = daemon.send("ws.close", json!({"wid": wid})).await;
    assert!(resp.ok, "ws.close: {:?}", resp.error);
    assert_eq!(resp.data.unwrap()["status"], "closed");

    // S3-6: ws.list returns empty
    let resp = daemon.send("ws.list", json!({})).await;
    assert!(resp.ok);
    assert_eq!(resp.data.unwrap().as_array().unwrap().len(), 0);

    // S3-7: daemon.status shows 0 browsers (managed browser auto-cleaned)
    let resp = daemon.send("daemon.status", json!({})).await;
    assert!(resp.ok);
    let status = resp.data.unwrap();
    assert_eq!(status["browsers"], 0, "managed browser should be cleaned up");
    assert_eq!(status["workspaces"], 0);

    daemon.stop().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// S4: Navigation (requires Chrome)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[ignore = "requires Chrome"]
async fn s4_navigation() {
    let daemon = common::DaemonFixture::start().await;

    let resp = daemon.send("ws.new", json!({})).await;
    assert!(resp.ok, "ws.new: {:?}", resp.error);
    let wid = resp.data.unwrap()["wid"].as_str().unwrap().to_string();

    // S4-2: goto example.com
    let resp = daemon
        .send("goto", json!({"wid": wid, "url": "https://example.com"}))
        .await;
    assert!(resp.ok, "goto example.com: {:?}", resp.error);

    // S4-3: nav.url returns example.com
    let resp = daemon.send("nav.url", json!({"wid": wid})).await;
    assert!(resp.ok, "nav.url: {:?}", resp.error);
    let url = resp.data.unwrap()["url"].as_str().unwrap().to_string();
    assert!(url.contains("example.com"), "url should contain example.com, got: {}", url);

    // S4-4: nav.title returns non-empty string
    let resp = daemon.send("nav.title", json!({"wid": wid})).await;
    assert!(resp.ok, "nav.title: {:?}", resp.error);
    let title = resp.data.unwrap()["title"].as_str().unwrap().to_string();
    assert!(!title.is_empty(), "title should not be empty");

    // S4-5: goto httpbin.org/get
    let resp = daemon
        .send("goto", json!({"wid": wid, "url": "https://httpbin.org/get"}))
        .await;
    assert!(resp.ok, "goto httpbin.org: {:?}", resp.error);

    // S4-6: nav.back
    let resp = daemon.send("nav.back", json!({"wid": wid})).await;
    assert!(resp.ok, "nav.back: {:?}", resp.error);

    // S4-7: nav.url after back should be example.com
    let resp = daemon.send("nav.url", json!({"wid": wid})).await;
    assert!(resp.ok);
    let url = resp.data.unwrap()["url"].as_str().unwrap().to_string();
    assert!(url.contains("example.com"), "after back, url should be example.com, got: {}", url);

    // S4-8: nav.forward
    let resp = daemon.send("nav.forward", json!({"wid": wid})).await;
    assert!(resp.ok, "nav.forward: {:?}", resp.error);

    // S4-9: nav.url after forward should be httpbin.org
    let resp = daemon.send("nav.url", json!({"wid": wid})).await;
    assert!(resp.ok);
    let url = resp.data.unwrap()["url"].as_str().unwrap().to_string();
    assert!(url.contains("httpbin.org"), "after forward, url should be httpbin.org, got: {}", url);

    // S4-10: reload
    let resp = daemon.send("reload", json!({"wid": wid})).await;
    assert!(resp.ok, "reload: {:?}", resp.error);

    daemon.send("ws.close", json!({"wid": wid})).await;
    daemon.stop().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// S5: Page Capture (requires Chrome)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[ignore = "requires Chrome"]
async fn s5_page_capture() {
    let daemon = common::DaemonFixture::start().await;

    let resp = daemon.send("ws.new", json!({})).await;
    assert!(resp.ok);
    let wid = resp.data.unwrap()["wid"].as_str().unwrap().to_string();

    let resp = daemon
        .send("goto", json!({"wid": wid, "url": "https://example.com"}))
        .await;
    assert!(resp.ok, "goto: {:?}", resp.error);

    // S5-2: screenshot viewport
    let resp = daemon.send("screenshot", json!({"wid": wid})).await;
    assert!(resp.ok, "screenshot: {:?}", resp.error);
    let b64 = resp.data.unwrap()["data"].as_str().unwrap().to_string();
    assert!(b64.len() > 1000, "screenshot base64 should be substantial, len={}", b64.len());

    // S5-3: screenshot full_page
    let resp = daemon
        .send("screenshot", json!({"wid": wid, "full_page": true}))
        .await;
    assert!(resp.ok, "screenshot full_page: {:?}", resp.error);
    let full_b64 = resp.data.unwrap()["data"].as_str().unwrap().to_string();
    assert!(full_b64.len() > 1000, "full page screenshot should be substantial");

    // S5-4: html full page
    let resp = daemon.send("html", json!({"wid": wid})).await;
    assert!(resp.ok, "html: {:?}", resp.error);
    let html = resp.data.unwrap()["html"].as_str().unwrap().to_string();
    assert!(html.contains("<html"), "html should contain <html tag");
    assert!(html.contains("Example Domain"), "html should contain 'Example Domain'");

    // S5-5: html with selector
    let resp = daemon
        .send("html", json!({"wid": wid, "selector": "h1"}))
        .await;
    assert!(resp.ok, "html selector: {:?}", resp.error);
    let h1_html = resp.data.unwrap()["html"].as_str().unwrap().to_string();
    assert!(h1_html.contains("<h1"), "h1 html should contain <h1 tag");
    assert!(h1_html.contains("Example Domain"), "h1 html should contain 'Example Domain'");

    // S5-6: pdf
    let resp = daemon.send("pdf", json!({"wid": wid})).await;
    assert!(resp.ok, "pdf: {:?}", resp.error);
    let pdf_data = resp.data.unwrap();
    let pdf_b64 = pdf_data["data"].as_str().unwrap();
    assert!(pdf_b64.len() > 1000, "pdf base64 should be substantial");

    daemon.send("ws.close", json!({"wid": wid})).await;
    daemon.stop().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// S6: JavaScript Execution (requires Chrome)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[ignore = "requires Chrome"]
async fn s6_javascript_execution() {
    let daemon = common::DaemonFixture::start().await;

    let resp = daemon.send("ws.new", json!({})).await;
    assert!(resp.ok);
    let wid = resp.data.unwrap()["wid"].as_str().unwrap().to_string();

    let resp = daemon
        .send("goto", json!({"wid": wid, "url": "https://example.com"}))
        .await;
    assert!(resp.ok, "goto: {:?}", resp.error);

    // S6-2: eval document.title
    let resp = daemon
        .send("eval", json!({"wid": wid, "expr": "document.title"}))
        .await;
    assert!(resp.ok, "eval document.title: {:?}", resp.error);
    assert_eq!(resp.data.unwrap()["result"], "Example Domain");

    // S6-3: eval arithmetic
    let resp = daemon
        .send("eval", json!({"wid": wid, "expr": "1 + 2"}))
        .await;
    assert!(resp.ok, "eval 1+2: {:?}", resp.error);
    assert_eq!(resp.data.unwrap()["result"], 3);

    // S6-4: eval window.location.href
    let resp = daemon
        .send("eval", json!({"wid": wid, "expr": "window.location.href"}))
        .await;
    assert!(resp.ok, "eval location: {:?}", resp.error);
    let href = resp.data.unwrap()["result"].as_str().unwrap().to_string();
    assert!(href.contains("example.com"), "href should contain example.com, got: {}", href);

    // S6-5: eval syntax error returns error
    let resp = daemon
        .send("eval", json!({"wid": wid, "expr": "{{{{invalid syntax"}))
        .await;
    assert!(!resp.ok, "invalid JS should return error");

    // S6-6: js.await with Promise
    let resp = daemon
        .send("js.await", json!({"wid": wid, "expr": "Promise.resolve(42)"}))
        .await;
    assert!(resp.ok, "js.await: {:?}", resp.error);
    assert_eq!(resp.data.unwrap()["result"], 42);

    daemon.send("ws.close", json!({"wid": wid})).await;
    daemon.stop().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// S7: Page State Extraction (requires Chrome)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[ignore = "requires Chrome"]
async fn s7_page_state() {
    let daemon = common::DaemonFixture::start().await;

    let resp = daemon.send("ws.new", json!({})).await;
    assert!(resp.ok);
    let wid = resp.data.unwrap()["wid"].as_str().unwrap().to_string();

    let resp = daemon
        .send("goto", json!({"wid": wid, "url": "https://example.com"}))
        .await;
    assert!(resp.ok, "goto: {:?}", resp.error);

    // S7-2: page.state returns elements
    let resp = daemon.send("page.state", json!({"wid": wid})).await;
    assert!(resp.ok, "page.state: {:?}", resp.error);
    let elements = resp.data.unwrap()["elements"].clone();
    let elements = elements.as_array().unwrap();
    assert!(!elements.is_empty(), "page.state should return elements");

    // S7-3: all elements have width > 0 and height > 0
    for el in elements {
        let w = el["width"].as_f64().unwrap_or(0.0);
        let h = el["height"].as_f64().unwrap_or(0.0);
        assert!(w > 0.0, "element width should be > 0: {:?}", el);
        assert!(h > 0.0, "element height should be > 0: {:?}", el);
    }

    // S7-4: indices are consecutive starting from 0
    let resp = daemon.send("page.state", json!({"wid": wid})).await;
    assert!(resp.ok);
    let elements = resp.data.unwrap()["elements"].clone();
    let elements = elements.as_array().unwrap();
    for (i, el) in elements.iter().enumerate() {
        assert_eq!(el["index"].as_u64().unwrap(), i as u64, "index should be consecutive");
    }

    // S7-5: page.search finds "Example"
    let resp = daemon
        .send("page.search", json!({"wid": wid, "text": "Example"}))
        .await;
    assert!(resp.ok, "page.search: {:?}", resp.error);
    let matches = resp.data.unwrap()["matches"].clone();
    let matches = matches.as_array().unwrap();
    assert!(!matches.is_empty(), "should find 'Example' on example.com");
    assert!(matches[0]["context"].as_str().unwrap().contains("Example"));

    // S7-6: page.search for non-existent text returns empty
    let resp = daemon
        .send("page.search", json!({"wid": wid, "text": "xyzzy_nonexistent_12345"}))
        .await;
    assert!(resp.ok, "page.search empty: {:?}", resp.error);
    assert_eq!(resp.data.unwrap()["matches"].as_array().unwrap().len(), 0);

    daemon.send("ws.close", json!({"wid": wid})).await;
    daemon.stop().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// C1: Full Browser Control Flow (requires Chrome)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[ignore = "requires Chrome"]
async fn c1_full_browser_control_flow() {
    let daemon = common::DaemonFixture::start().await;

    // C1-1: ws.new with label
    let resp = daemon.send("ws.new", json!({"label": "form-task"})).await;
    assert!(resp.ok, "ws.new: {:?}", resp.error);
    let wid = resp.data.unwrap()["wid"].as_str().unwrap().to_string();

    // C1-2: ws.info shows label
    let resp = daemon.send("ws.info", json!({"wid": wid})).await;
    assert!(resp.ok);
    assert_eq!(resp.data.unwrap()["label"], "form-task");

    // C1-3: goto httpbin.org/forms/post (reliable form page)
    let resp = daemon
        .send("goto", json!({"wid": wid, "url": "https://httpbin.org/forms/post"}))
        .await;
    assert!(resp.ok, "goto httpbin forms: {:?}", resp.error);
    let url_resp = daemon.send("nav.url", json!({"wid": wid})).await;
    assert!(url_resp.ok);
    let url = url_resp.data.unwrap()["url"].as_str().unwrap().to_string();
    assert!(url.contains("httpbin.org"), "url should be httpbin.org, got: {}", url);

    // C1-4: page.state returns elements including input
    let resp = daemon.send("page.state", json!({"wid": wid})).await;
    assert!(resp.ok, "page.state: {:?}", resp.error);
    let elements = resp.data.unwrap()["elements"].clone();
    let elements = elements.as_array().unwrap();
    assert!(!elements.is_empty(), "should have elements on httpbin forms page");

    // C1-5: find custname input index (tag == "input")
    let input_idx = elements
        .iter()
        .position(|el| el["tag"].as_str() == Some("input"))
        .expect("should find an input element on httpbin forms page");

    // C1-6: type into the input
    let resp = daemon
        .send("type", json!({"wid": wid, "index": input_idx, "text": "Test User"}))
        .await;
    assert!(resp.ok, "type: {:?}", resp.error);

    // C1-7: verify input value via eval
    let resp = daemon
        .send("eval", json!({"wid": wid, "expr": "document.querySelector('input[name=custname]').value"}))
        .await;
    assert!(resp.ok, "eval input value: {:?}", resp.error);
    let val = resp.data.unwrap()["result"].as_str().unwrap_or("").to_string();
    assert!(val.contains("Test User"), "input should contain 'Test User', got: {}", val);

    // C1-8: page.state again to get updated elements
    let resp = daemon.send("page.state", json!({"wid": wid})).await;
    assert!(resp.ok, "page.state 2: {:?}", resp.error);
    let elements2 = resp.data.unwrap()["elements"].clone();
    assert!(!elements2.as_array().unwrap().is_empty());

    // C1-12: screenshot
    let resp = daemon.send("screenshot", json!({"wid": wid})).await;
    assert!(resp.ok, "screenshot: {:?}", resp.error);
    assert!(!resp.data.unwrap()["data"].as_str().unwrap().is_empty());

    // C1-13: html contains form content
    let resp = daemon.send("html", json!({"wid": wid})).await;
    assert!(resp.ok, "html: {:?}", resp.error);
    let html = resp.data.unwrap()["html"].as_str().unwrap().to_string();
    assert!(html.len() > 1000, "html should be substantial");

    daemon.send("ws.close", json!({"wid": wid})).await;
    daemon.stop().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// C2: Multi-Tab Management (requires Chrome)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[ignore = "requires Chrome"]
async fn c2_multi_tab_management() {
    let daemon = common::DaemonFixture::start().await;

    let resp = daemon.send("ws.new", json!({})).await;
    assert!(resp.ok);
    let wid = resp.data.unwrap()["wid"].as_str().unwrap().to_string();

    let resp = daemon
        .send("goto", json!({"wid": wid, "url": "https://example.com"}))
        .await;
    assert!(resp.ok, "goto example.com: {:?}", resp.error);

    // C2-3: tab.list shows 1 tab, active=true
    let resp = daemon.send("tab.list", json!({"wid": wid})).await;
    assert!(resp.ok);
    let tabs = resp.data.unwrap();
    let tabs = tabs.as_array().unwrap();
    assert_eq!(tabs.len(), 1);
    assert_eq!(tabs[0]["active"], true);
    let tid1 = tabs[0]["tid"].as_str().unwrap().to_string();

    // C2-4: tab.new with url
    let resp = daemon
        .send("tab.new", json!({"wid": wid, "url": "https://httpbin.org/get"}))
        .await;
    assert!(resp.ok, "tab.new: {:?}", resp.error);
    let tid2 = resp.data.unwrap()["tid"].as_str().unwrap().to_string();
    assert_ne!(tid1, tid2);

    // C2-5: tab.list shows 2 tabs
    let resp = daemon.send("tab.list", json!({"wid": wid})).await;
    assert!(resp.ok);
    assert_eq!(resp.data.unwrap().as_array().unwrap().len(), 2);

    // C2-7: tab.switch to tid1
    let resp = daemon
        .send("tab.switch", json!({"wid": wid, "tid": tid1}))
        .await;
    assert!(resp.ok, "tab.switch: {:?}", resp.error);

    // C2-8: nav.url should be example.com after switch
    let resp = daemon.send("nav.url", json!({"wid": wid})).await;
    assert!(resp.ok);
    let url = resp.data.unwrap()["url"].as_str().unwrap().to_string();
    assert!(url.contains("example.com"), "after switch to tid1, url should be example.com, got: {}", url);

    // C2-9: tab.new without url (about:blank)
    let resp = daemon.send("tab.new", json!({"wid": wid})).await;
    assert!(resp.ok, "tab.new blank: {:?}", resp.error);
    let tid3 = resp.data.unwrap()["tid"].as_str().unwrap().to_string();

    // C2-10: tab.list shows 3 tabs
    let resp = daemon.send("tab.list", json!({"wid": wid})).await;
    assert!(resp.ok);
    assert_eq!(resp.data.unwrap().as_array().unwrap().len(), 3);

    // C2-11: tab.close tid3
    let resp = daemon
        .send("tab.close", json!({"wid": wid, "tid": tid3}))
        .await;
    assert!(resp.ok, "tab.close tid3: {:?}", resp.error);

    // C2-12: tab.list shows 2 tabs, tid3 gone
    let resp = daemon.send("tab.list", json!({"wid": wid})).await;
    assert!(resp.ok);
    let tabs = resp.data.unwrap();
    let tabs = tabs.as_array().unwrap();
    assert_eq!(tabs.len(), 2);
    assert!(!tabs.iter().any(|t| t["tid"] == tid3), "tid3 should be gone");

    daemon.send("ws.close", json!({"wid": wid})).await;
    daemon.stop().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// C3: Multi-Workspace Isolation (requires Chrome)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[ignore = "requires Chrome"]
async fn c3_multi_workspace_isolation() {
    let daemon = common::DaemonFixture::start().await;

    // C3-1: ws.new ws-a
    let resp = daemon.send("ws.new", json!({"label": "ws-a"})).await;
    assert!(resp.ok, "ws.new ws-a: {:?}", resp.error);
    let wid_a = resp.data.unwrap()["wid"].as_str().unwrap().to_string();

    // C3-2: ws.new ws-b
    let resp = daemon.send("ws.new", json!({"label": "ws-b"})).await;
    assert!(resp.ok, "ws.new ws-b: {:?}", resp.error);
    let wid_b = resp.data.unwrap()["wid"].as_str().unwrap().to_string();
    assert_ne!(wid_a, wid_b);

    // C3-3: daemon.status shows 2 workspaces, 1 browser (shared)
    let resp = daemon.send("daemon.status", json!({})).await;
    assert!(resp.ok);
    let status = resp.data.unwrap();
    assert_eq!(status["workspaces"], 2);
    assert_eq!(status["browsers"], 1, "should share one browser");

    // C3-4: goto example.com in ws-a
    let resp = daemon
        .send("goto", json!({"wid": wid_a, "url": "https://example.com"}))
        .await;
    assert!(resp.ok, "goto ws-a: {:?}", resp.error);

    // C3-5: goto httpbin.org in ws-b
    let resp = daemon
        .send("goto", json!({"wid": wid_b, "url": "https://httpbin.org/get"}))
        .await;
    assert!(resp.ok, "goto ws-b: {:?}", resp.error);

    // C3-6: storage.local.set in ws-a
    let resp = daemon
        .send("storage.local.set", json!({"wid": wid_a, "key": "isolation_key", "value": "value-a"}))
        .await;
    assert!(resp.ok, "storage.local.set ws-a: {:?}", resp.error);

    // C3-7: storage.local.set in ws-b
    let resp = daemon
        .send("storage.local.set", json!({"wid": wid_b, "key": "isolation_key", "value": "value-b"}))
        .await;
    assert!(resp.ok, "storage.local.set ws-b: {:?}", resp.error);

    // C3-8: storage.local.get ws-a should return value-a
    let resp = daemon
        .send("storage.local.get", json!({"wid": wid_a, "key": "isolation_key"}))
        .await;
    assert!(resp.ok, "storage.local.get ws-a: {:?}", resp.error);
    let val = resp.data.unwrap()["value"].as_str().unwrap_or("").to_string();
    assert_eq!(val, "value-a", "ws-a should have value-a, got: {}", val);

    // C3-9: storage.local.get ws-b should return value-b
    let resp = daemon
        .send("storage.local.get", json!({"wid": wid_b, "key": "isolation_key"}))
        .await;
    assert!(resp.ok, "storage.local.get ws-b: {:?}", resp.error);
    let val = resp.data.unwrap()["value"].as_str().unwrap_or("").to_string();
    assert_eq!(val, "value-b", "ws-b should have value-b, got: {}", val);

    // C3-10: nav.url ws-a should still be example.com
    let resp = daemon.send("nav.url", json!({"wid": wid_a})).await;
    assert!(resp.ok);
    assert!(resp.data.unwrap()["url"].as_str().unwrap().contains("example.com"));

    // C3-11: nav.url ws-b should still be httpbin.org
    let resp = daemon.send("nav.url", json!({"wid": wid_b})).await;
    assert!(resp.ok);
    assert!(resp.data.unwrap()["url"].as_str().unwrap().contains("httpbin.org"));

    // C3-12: close ws-a
    let resp = daemon.send("ws.close", json!({"wid": wid_a})).await;
    assert!(resp.ok, "ws.close ws-a: {:?}", resp.error);

    // C3-13: daemon.status shows 1 workspace, 1 browser (ws-b still active)
    let resp = daemon.send("daemon.status", json!({})).await;
    assert!(resp.ok);
    let status = resp.data.unwrap();
    assert_eq!(status["workspaces"], 1);
    assert_eq!(status["browsers"], 1, "browser should remain for ws-b");

    // C3-14: close ws-b
    let resp = daemon.send("ws.close", json!({"wid": wid_b})).await;
    assert!(resp.ok, "ws.close ws-b: {:?}", resp.error);

    // C3-15: daemon.status shows 0 workspaces, 0 browsers
    let resp = daemon.send("daemon.status", json!({})).await;
    assert!(resp.ok);
    let status = resp.data.unwrap();
    assert_eq!(status["workspaces"], 0);
    assert_eq!(status["browsers"], 0, "managed browser should be cleaned up");

    daemon.stop().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// C4: Storage Export/Import (requires Chrome)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[ignore = "requires Chrome"]
async fn c4_storage_export_import() {
    let daemon = common::DaemonFixture::start().await;

    let resp = daemon.send("ws.new", json!({})).await;
    assert!(resp.ok);
    let wid1 = resp.data.unwrap()["wid"].as_str().unwrap().to_string();

    let resp = daemon
        .send("goto", json!({"wid": wid1, "url": "https://httpbin.org/get"}))
        .await;
    assert!(resp.ok, "goto ws1: {:?}", resp.error);

    // Set localStorage values
    daemon
        .send("storage.local.set", json!({"wid": wid1, "key": "user", "value": "alice"}))
        .await;
    daemon
        .send("storage.local.set", json!({"wid": wid1, "key": "token", "value": "abc123"}))
        .await;

    // C4-4: storage.export
    let resp = daemon.send("storage.export", json!({"wid": wid1})).await;
    assert!(resp.ok, "storage.export: {:?}", resp.error);
    let export_data = resp.data.unwrap();

    // C4-5: verify exported content
    let ls = export_data.get("localStorage")
        .or_else(|| export_data.get("local_storage"))
        .expect("export should contain localStorage");
    assert_eq!(ls["user"], "alice");
    assert_eq!(ls["token"], "abc123");

    // C4-6: create new workspace
    let resp = daemon.send("ws.new", json!({})).await;
    assert!(resp.ok);
    let wid2 = resp.data.unwrap()["wid"].as_str().unwrap().to_string();

    // C4-7: goto same domain in wid2
    let resp = daemon
        .send("goto", json!({"wid": wid2, "url": "https://httpbin.org/get"}))
        .await;
    assert!(resp.ok, "goto ws2: {:?}", resp.error);

    // C4-8: storage.import
    let resp = daemon
        .send("storage.import", json!({"wid": wid2, "state": export_data}))
        .await;
    assert!(resp.ok, "storage.import: {:?}", resp.error);

    // C4-9: verify user was imported
    let resp = daemon
        .send("storage.local.get", json!({"wid": wid2, "key": "user"}))
        .await;
    assert!(resp.ok, "get user ws2: {:?}", resp.error);
    assert_eq!(resp.data.unwrap()["value"], "alice");

    // C4-10: verify token was imported
    let resp = daemon
        .send("storage.local.get", json!({"wid": wid2, "key": "token"}))
        .await;
    assert!(resp.ok, "get token ws2: {:?}", resp.error);
    assert_eq!(resp.data.unwrap()["value"], "abc123");

    daemon.send("ws.close", json!({"wid": wid1})).await;
    daemon.send("ws.close", json!({"wid": wid2})).await;
    daemon.stop().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// C8: Raw CDP Commands (requires Chrome)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[ignore = "requires Chrome"]
async fn c8_raw_cdp_commands() {
    let daemon = common::DaemonFixture::start().await;

    let resp = daemon.send("ws.new", json!({})).await;
    assert!(resp.ok);
    let wid = resp.data.unwrap()["wid"].as_str().unwrap().to_string();

    let resp = daemon
        .send("goto", json!({"wid": wid, "url": "https://example.com"}))
        .await;
    assert!(resp.ok, "goto: {:?}", resp.error);

    // C8-2: cdp.send Runtime.evaluate
    // Response structure: {"wid":..., "method":..., "result": <raw CDP response>}
    // Runtime.evaluate raw response: {"result": {"type":"number","value":2}}
    let resp = daemon
        .send("cdp.send", json!({"wid": wid, "method": "Runtime.evaluate", "params": {"expression": "1+1"}}))
        .await;
    assert!(resp.ok, "cdp.send Runtime.evaluate: {:?}", resp.error);
    let data = resp.data.unwrap();
    // The raw CDP result is nested under data["result"]
    let cdp_result = &data["result"];
    let value = cdp_result["result"]["value"].as_u64()
        .or_else(|| cdp_result["value"].as_u64());
    assert_eq!(value, Some(2), "1+1 should equal 2, got cdp_result: {:?}", cdp_result);

    // C8-3: cdp.send Page.getLayoutMetrics
    let resp = daemon
        .send("cdp.send", json!({"wid": wid, "method": "Page.getLayoutMetrics"}))
        .await;
    assert!(resp.ok, "cdp.send Page.getLayoutMetrics: {:?}", resp.error);
    let data = resp.data.unwrap();
    let cdp_result = &data["result"];
    assert!(
        cdp_result.get("cssContentSize").is_some() || cdp_result.get("contentSize").is_some(),
        "should have content size, got: {:?}", cdp_result
    );

    // C8-4: cdp.send DOM.getDocument
    let resp = daemon
        .send("cdp.send", json!({"wid": wid, "method": "DOM.getDocument"}))
        .await;
    assert!(resp.ok, "cdp.send DOM.getDocument: {:?}", resp.error);
    let data = resp.data.unwrap();
    let cdp_result = &data["result"];
    assert!(cdp_result["root"]["nodeId"].as_u64().is_some(), "should have root nodeId, got: {:?}", cdp_result);

    // C8-5: cdp.send invalid method returns error
    let resp = daemon
        .send("cdp.send", json!({"wid": wid, "method": "Fake.nonExistentMethod"}))
        .await;
    assert!(!resp.ok, "invalid CDP method should fail");

    daemon.send("ws.close", json!({"wid": wid})).await;
    daemon.stop().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// C10: Screenshot Element Selector (requires Chrome)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[ignore = "requires Chrome"]
async fn c10_screenshot_element_selector() {
    let daemon = common::DaemonFixture::start().await;

    let resp = daemon.send("ws.new", json!({})).await;
    assert!(resp.ok);
    let wid = resp.data.unwrap()["wid"].as_str().unwrap().to_string();

    let resp = daemon
        .send("goto", json!({"wid": wid, "url": "https://example.com"}))
        .await;
    assert!(resp.ok, "goto: {:?}", resp.error);

    // C10-2: full viewport screenshot
    let resp = daemon.send("screenshot", json!({"wid": wid})).await;
    assert!(resp.ok, "viewport screenshot: {:?}", resp.error);
    let full_b64 = resp.data.unwrap()["data"].as_str().unwrap().to_string();
    assert!(full_b64.len() > 1000);

    // C10-3: screenshot with selector "h1"
    let resp = daemon
        .send("screenshot", json!({"wid": wid, "selector": "h1"}))
        .await;
    assert!(resp.ok, "screenshot h1: {:?}", resp.error);
    let h1_b64 = resp.data.unwrap()["data"].as_str().unwrap().to_string();
    assert!(!h1_b64.is_empty());

    // C10-4: h1 screenshot should be smaller than full page
    assert!(
        h1_b64.len() < full_b64.len(),
        "h1 screenshot ({}) should be smaller than full ({})",
        h1_b64.len(), full_b64.len()
    );

    // C10-5: screenshot with non-existent selector returns error
    let resp = daemon
        .send("screenshot", json!({"wid": wid, "selector": ".nonexistent_xyz_12345"}))
        .await;
    assert!(!resp.ok, "non-existent selector should fail");
    let err = resp.error.unwrap();
    assert!(
        err.contains("not found") || err.contains("element"),
        "error should mention element not found, got: {}", err
    );

    daemon.send("ws.close", json!({"wid": wid})).await;
    daemon.stop().await;
}
