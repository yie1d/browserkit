// Command dispatcher: routes requests to handler functions

mod act;
mod attach;
mod browser;
pub mod common;
pub(crate) mod connect;
mod daemon;
#[allow(dead_code)]
mod debug;
mod dialog;
mod download;
mod evaluate;
mod inspect;
mod navigate;
#[allow(dead_code)]
mod network;
mod open;
mod screenshot;
mod session;
mod snapshot;
mod tabs;
mod url_policy;
mod wait;

use std::sync::Arc;

use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::DaemonState;
use crate::error::ErrorCode;

pub use common::HandlerContext;

/// Dispatch a [`Request`] to the appropriate handler and return a [`Response`].
pub async fn handle_request(
    req: &Request,
    state: &Arc<DaemonState>,
    ctx: &HandlerContext,
) -> Response {
    state.inc_request_count();
    if let Err(response) = validate_request_fields(req) {
        return response;
    }

    if let Some(session_name) = request_session_name(req) {
        let lifecycle_lock = state.session_lifecycle_lock(&session_name);
        if req.cmd == "session.close" {
            let _operation_guard = lifecycle_lock.write().await;
            if let Some(mut session) = state.sessions.get_mut(&session_name) {
                session.touch();
                drop(session);
                state.request_persist();
            }
            return dispatch_request(req, state, ctx).await;
        }
        let _operation_guard = lifecycle_lock.read().await;
        if let Some(mut session) = state.sessions.get_mut(&session_name) {
            session.touch();
            drop(session);
            state.request_persist();
        }
        return dispatch_request(req, state, ctx).await;
    }

    dispatch_request(req, state, ctx).await
}

#[derive(Clone, Copy)]
enum RequestFieldType {
    String,
    Bool,
    U64,
    I64,
    Object,
    Array,
    StringArray,
}

impl RequestFieldType {
    fn accepts(self, value: &serde_json::Value) -> bool {
        match self {
            Self::String => value.is_string(),
            Self::Bool => value.is_boolean(),
            Self::U64 => value.as_u64().is_some(),
            Self::I64 => value.as_i64().is_some(),
            Self::Object => value.is_object(),
            Self::Array => value.is_array(),
            Self::StringArray => value
                .as_array()
                .is_some_and(|items| items.iter().all(serde_json::Value::is_string)),
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::String => "a string",
            Self::Bool => "a boolean",
            Self::U64 => "an unsigned integer",
            Self::I64 => "an integer",
            Self::Object => "an object",
            Self::Array => "an array",
            Self::StringArray => "an array of strings",
        }
    }
}

#[derive(Clone, Copy)]
struct RequestField {
    name: &'static str,
    field_type: RequestFieldType,
}

const fn request_field(name: &'static str, field_type: RequestFieldType) -> RequestField {
    RequestField { name, field_type }
}

fn allowed_request_fields(command: &str) -> Option<Vec<RequestField>> {
    use RequestFieldType::{Array, Bool, Object, String, StringArray, I64, U64};

    let fields = match command {
        "ping" | "session.list" | "daemon.status" | "daemon.stop" | "browser.list" => vec![],
        "connect"
        | "tabs"
        | "session.close"
        | "session.cookies.get"
        | "session.cookies.clear"
        | "dialog.list" => vec![request_field("session", String)],
        "open" => vec![
            request_field("url", String),
            request_field("session", String),
        ],
        "snapshot" => vec![
            request_field("session", String),
            request_field("target", String),
            request_field("wait", String),
            request_field("full", Bool),
            request_field("no_page_text", Bool),
            request_field("timeout", U64),
            request_field("max_tokens", U64),
        ],
        "navigate" => vec![
            request_field("url", String),
            request_field("back", Bool),
            request_field("forward", Bool),
            request_field("reload", Bool),
            request_field("session", String),
            request_field("target", String),
            request_field("timeout", U64),
        ],
        "attach" => vec![
            request_field("session", String),
            request_field("target", String),
            request_field("pattern", String),
        ],
        "close" => vec![
            request_field("session", String),
            request_field("target", String),
        ],
        "session.cookies.set" => vec![
            request_field("session", String),
            request_field("cookies", Array),
        ],
        "session.storage.local.get" => vec![
            request_field("session", String),
            request_field("target", String),
            request_field("key", String),
        ],
        "session.storage.local.set" => vec![
            request_field("session", String),
            request_field("target", String),
            request_field("key", String),
            request_field("value", String),
        ],
        "session.storage.export" => vec![
            request_field("session", String),
            request_field("target", String),
        ],
        "session.storage.import" => vec![
            request_field("session", String),
            request_field("target", String),
            request_field("state", Object),
        ],
        "evaluate" => vec![
            request_field("session", String),
            request_field("target", String),
            request_field("expression", String),
            request_field("timeout", U64),
        ],
        "screenshot" => vec![
            request_field("session", String),
            request_field("target", String),
            request_field("full_page", Bool),
            request_field("output", String),
            request_field("selector", String),
            request_field("labels", Bool),
        ],
        "wait" => vec![
            request_field("session", String),
            request_field("target", String),
            request_field("time", U64),
            request_field("selector", String),
            request_field("text", String),
            request_field("text_gone", String),
            request_field("url", String),
            request_field("load_state", String),
            request_field("fn", String),
            request_field("timeout", U64),
        ],
        "find" => vec![
            request_field("session", String),
            request_field("target", String),
            request_field("selector", String),
            request_field("attributes", StringArray),
            request_field("max", U64),
            request_field("include_text", Bool),
        ],
        "search" => vec![
            request_field("session", String),
            request_field("target", String),
            request_field("text", String),
            request_field("regex", Bool),
            request_field("scope", String),
            request_field("context", U64),
            request_field("max", U64),
        ],
        "html" => vec![
            request_field("session", String),
            request_field("target", String),
            request_field("selector", String),
        ],
        "console" => vec![
            request_field("session", String),
            request_field("target", String),
            request_field("level", String),
            request_field("limit", U64),
        ],
        "pdf" => vec![
            request_field("session", String),
            request_field("target", String),
            request_field("output", String),
            request_field("landscape", Bool),
            request_field("background", Bool),
        ],
        "browser.connect" => vec![
            request_field("host", String),
            request_field("session", String),
        ],
        "browser.discover" => vec![
            request_field("path", String),
            request_field("session", String),
        ],
        "browser.disconnect" => vec![request_field("host", String)],
        "debug.block" => vec![
            request_field("session", String),
            request_field("target", String),
            request_field("pattern", String),
        ],
        "debug.unblock" => vec![
            request_field("session", String),
            request_field("target", String),
        ],
        "network.watch" => vec![
            request_field("session", String),
            request_field("target", String),
            request_field("pattern", String),
            request_field("count", U64),
            request_field("timeout", U64),
        ],
        "download" => vec![
            request_field("session", String),
            request_field("target", String),
            request_field("ref", I64),
            request_field("output_dir", String),
            request_field("timeout", U64),
        ],
        "debug.cdp" => vec![
            request_field("session", String),
            request_field("target", String),
            request_field("method", String),
            request_field("params", Object),
        ],
        "dialog.accept" => vec![
            request_field("session", String),
            request_field("target", String),
            request_field("text", String),
        ],
        "dialog.dismiss" => vec![
            request_field("session", String),
            request_field("target", String),
        ],
        "dialog.policy" => vec![
            request_field("session", String),
            request_field("policy", String),
        ],
        "act" => return None,
        _ => return None,
    };
    Some(fields)
}

fn validate_request_fields(req: &Request) -> Result<(), Response> {
    let allowed = allowed_request_fields(&req.cmd);
    if req.cmd != "act" && allowed.is_none() {
        return Ok(());
    }
    let object = req.params.as_object().ok_or_else(|| {
        Response::error_detail(
            ErrorCode::InvalidArgument,
            format!("{} params must be an object", req.cmd),
            None,
        )
    })?;
    let Some(allowed) = allowed else {
        return Ok(());
    };

    for (field, value) in object {
        let Some(spec) = allowed.iter().find(|spec| spec.name == field) else {
            return Err(Response::error_detail(
                ErrorCode::InvalidArgument,
                format!("unexpected field '{}' for command '{}'", field, req.cmd),
                None,
            ));
        };
        if !spec.field_type.accepts(value) {
            return Err(Response::error_detail(
                ErrorCode::InvalidArgument,
                format!(
                    "field '{}' for command '{}' must be {}",
                    field,
                    req.cmd,
                    spec.field_type.description()
                ),
                None,
            ));
        }
    }
    Ok(())
}

fn request_session_name(req: &Request) -> Option<String> {
    let session_bound = matches!(
        req.cmd.as_str(),
        "open"
            | "snapshot"
            | "navigate"
            | "act"
            | "attach"
            | "tabs"
            | "close"
            | "session.close"
            | "session.cookies.get"
            | "session.cookies.set"
            | "session.cookies.clear"
            | "session.storage.local.get"
            | "session.storage.local.set"
            | "session.storage.export"
            | "session.storage.import"
            | "evaluate"
            | "screenshot"
            | "wait"
            | "find"
            | "search"
            | "html"
            | "console"
            | "pdf"
            | "debug.block"
            | "debug.unblock"
            | "network.watch"
            | "download"
            | "debug.cdp"
            | "dialog.list"
            | "dialog.accept"
            | "dialog.dismiss"
            | "dialog.policy"
    );
    if !session_bound {
        return None;
    }
    match req.params.get("session") {
        None => Some("default".into()),
        Some(serde_json::Value::String(name)) => Some(name.clone()),
        Some(_) => None,
    }
}

async fn dispatch_request(
    req: &Request,
    state: &Arc<DaemonState>,
    ctx: &HandlerContext,
) -> Response {
    match req.cmd.as_str() {
        "ping" => daemon::handle_ping(),
        "connect" => connect::handle_connect(req, state).await,
        "open" => open::handle_open(req, state).await,
        "snapshot" => snapshot::handle_snapshot(req, state).await,
        "navigate" => navigate::handle_navigate(req, state).await,
        "act" => act::handle_act(req, state).await,
        "attach" => attach::handle_attach(req, state).await,
        "tabs" => tabs::handle_tabs(req, state).await,
        "close" => tabs::handle_close(req, state).await,
        "session.close" => session::handle_session_close(req, state).await,
        "session.list" => session::handle_session_list(req, state).await,
        "session.cookies.get" => session::handle_session_cookies_get(req, state).await,
        "session.cookies.set" => session::handle_session_cookies_set(req, state).await,
        "session.cookies.clear" => session::handle_session_cookies_clear(req, state).await,
        "session.storage.local.get" => session::handle_session_storage_local_get(req, state).await,
        "session.storage.local.set" => session::handle_session_storage_local_set(req, state).await,
        "session.storage.export" => session::handle_session_storage_export(req, state).await,
        "session.storage.import" => session::handle_session_storage_import(req, state).await,
        "evaluate" => evaluate::handle_evaluate(req, state).await,
        "screenshot" => screenshot::handle_screenshot(req, state).await,
        "wait" => wait::handle_wait(req, state).await,
        "find" | "search" | "html" | "console" | "pdf" => inspect::handle_inspect(req, state).await,
        "daemon.status" => daemon::handle_daemon_status(state, ctx).await,
        "daemon.stop" => daemon::handle_daemon_stop(state, ctx).await,
        "browser.connect" => browser::handle_browser_connect(req, state).await,
        "browser.discover" => browser::handle_browser_discover(req, state).await,
        "browser.list" => browser::handle_browser_list(state).await,
        "browser.disconnect" => browser::handle_browser_disconnect(req, state).await,
        "debug.block" => network::handle_debug_block(req, state).await,
        "debug.unblock" => network::handle_debug_unblock(req, state).await,
        "network.watch" => network::handle_network_watch(req, state).await,
        "download" => download::handle_download(req, state).await,
        "debug.cdp" => debug::handle_debug_cdp(req, state).await,
        "dialog.list" => dialog::handle_dialog_list(req, state).await,
        "dialog.accept" => dialog::handle_dialog_accept(req, state).await,
        "dialog.dismiss" => dialog::handle_dialog_dismiss(req, state).await,
        "dialog.policy" => dialog::handle_dialog_policy(req, state).await,
        _ => Response::error_detail(
            ErrorCode::InvalidArgument,
            format!("unknown command: {}", req.cmd),
            Some("run 'bk --help' to list supported commands".into()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::session::Session;

    fn test_context() -> HandlerContext {
        let (shutdown, _rx) = tokio::sync::watch::channel(false);
        HandlerContext {
            port: 0,
            pid: 0,
            shutdown,
        }
    }

    fn assert_unknown_error(value: &serde_json::Value, command: &str) {
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "INVALID_ARGUMENT");
        assert_eq!(
            value["error"]["message"],
            format!("unknown command: {command}")
        );
    }

    #[tokio::test]
    async fn session_bound_request_touches_activity_centrally() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.last_active = 1;
        state.sessions.insert("default".into(), session);
        let request = Request {
            cmd: "tabs".into(),
            params: serde_json::json!({}),
        };

        let _ = handle_request(&request, &state, &test_context()).await;

        assert!(state.sessions.get("default").unwrap().last_active > 1);
    }

    #[test]
    fn daemon_and_browser_requests_do_not_claim_a_session() {
        for command in [
            "ping",
            "connect",
            "daemon.status",
            "browser.list",
            "unknown",
        ] {
            let request = Request {
                cmd: command.into(),
                params: serde_json::json!({}),
            };
            assert_eq!(request_session_name(&request), None, "{command}");
        }
    }

    #[tokio::test]
    async fn unknown_command_returns_canonical_error() {
        let state = Arc::new(DaemonState::new());
        let command = "not-a-command";
        let request = Request {
            cmd: command.into(),
            params: serde_json::json!({}),
        };

        let response = handle_request(&request, &state, &test_context()).await;
        let value = serde_json::to_value(response).unwrap();
        assert_unknown_error(&value, command);
    }

    #[test]
    fn canonical_request_fields_accept_current_contract() {
        let contracts: &[(&str, &[&str])] = &[
            ("ping", &[]),
            ("connect", &["session"]),
            ("open", &["url", "session"]),
            (
                "snapshot",
                &[
                    "session",
                    "target",
                    "wait",
                    "full",
                    "no_page_text",
                    "timeout",
                    "max_tokens",
                ],
            ),
            (
                "navigate",
                &[
                    "url", "back", "forward", "reload", "session", "target", "timeout",
                ],
            ),
            ("attach", &["session", "target", "pattern"]),
            ("tabs", &["session"]),
            ("close", &["session", "target"]),
            ("session.close", &["session"]),
            ("session.list", &[]),
            ("session.cookies.get", &["session"]),
            ("session.cookies.set", &["session", "cookies"]),
            ("session.cookies.clear", &["session"]),
            ("session.storage.local.get", &["session", "target", "key"]),
            (
                "session.storage.local.set",
                &["session", "target", "key", "value"],
            ),
            ("session.storage.export", &["session", "target"]),
            ("session.storage.import", &["session", "target", "state"]),
            ("evaluate", &["session", "target", "expression", "timeout"]),
            (
                "screenshot",
                &[
                    "session",
                    "target",
                    "full_page",
                    "output",
                    "selector",
                    "labels",
                ],
            ),
            (
                "wait",
                &[
                    "session",
                    "target",
                    "time",
                    "selector",
                    "text",
                    "text_gone",
                    "url",
                    "load_state",
                    "fn",
                    "timeout",
                ],
            ),
            (
                "find",
                &[
                    "session",
                    "target",
                    "selector",
                    "attributes",
                    "max",
                    "include_text",
                ],
            ),
            (
                "search",
                &[
                    "session", "target", "text", "regex", "scope", "context", "max",
                ],
            ),
            ("html", &["session", "target", "selector"]),
            ("console", &["session", "target", "level", "limit"]),
            (
                "pdf",
                &["session", "target", "output", "landscape", "background"],
            ),
            ("daemon.status", &[]),
            ("daemon.stop", &[]),
            ("browser.connect", &["host", "session"]),
            ("browser.discover", &["path", "session"]),
            ("browser.list", &[]),
            ("browser.disconnect", &["host"]),
            ("debug.block", &["session", "target", "pattern"]),
            ("debug.unblock", &["session", "target"]),
            (
                "network.watch",
                &["session", "target", "pattern", "count", "timeout"],
            ),
            (
                "download",
                &["session", "target", "ref", "output_dir", "timeout"],
            ),
            ("debug.cdp", &["session", "target", "method", "params"]),
            ("dialog.list", &["session"]),
            ("dialog.accept", &["session", "target", "text"]),
            ("dialog.dismiss", &["session", "target"]),
            ("dialog.policy", &["session", "policy"]),
        ];

        for (cmd, fields) in contracts {
            let params = fields
                .iter()
                .map(|field| {
                    let value = match *field {
                        "full" | "no_page_text" | "back" | "forward" | "reload" | "full_page"
                        | "labels" | "include_text" | "regex" | "landscape" | "background" => {
                            serde_json::json!(true)
                        }
                        "timeout" | "max_tokens" | "time" | "max" | "context" | "limit"
                        | "count" | "ref" => serde_json::json!(1),
                        "params" | "state" => serde_json::json!({}),
                        "cookies" => serde_json::json!([]),
                        "attributes" => serde_json::json!(["id"]),
                        _ => serde_json::json!("value"),
                    };
                    ((*field).to_string(), value)
                })
                .collect();
            let req = Request {
                cmd: (*cmd).into(),
                params: serde_json::Value::Object(params),
            };
            assert!(validate_request_fields(&req).is_ok(), "{cmd}");
        }

        let act = Request {
            cmd: "act".into(),
            params: serde_json::json!({"kind": "click", "ref": 1}),
        };
        assert!(validate_request_fields(&act).is_ok());
    }

    #[test]
    fn canonical_request_fields_reject_old_unknown_and_non_object_params() {
        for field in ["wid", "tid", "index", "sesion"] {
            let mut params = serde_json::json!({"url": "https://example.test"});
            params[field] = serde_json::json!(1);
            let req = Request {
                cmd: "open".into(),
                params,
            };
            let value = serde_json::to_value(validate_request_fields(&req).unwrap_err()).unwrap();
            assert_eq!(value["error"]["code"], "INVALID_ARGUMENT", "{field}");
            assert!(value["error"]["message"].as_str().unwrap().contains(field));
        }

        let req = Request {
            cmd: "tabs".into(),
            params: serde_json::json!([]),
        };
        assert!(validate_request_fields(&req).is_err());

        for (command, params) in [
            (
                "evaluate",
                serde_json::json!({"expression": "1", "await_promise": false}),
            ),
            (
                "session.cookies.set",
                serde_json::json!({"file": "cookies.json"}),
            ),
        ] {
            let req = Request {
                cmd: command.into(),
                params,
            };
            assert!(validate_request_fields(&req).is_err(), "{command}");
        }
    }

    #[test]
    fn canonical_request_fields_reject_wrong_json_types() {
        for (command, field, value) in [
            ("open", "url", serde_json::json!(1)),
            ("snapshot", "full", serde_json::json!("true")),
            ("snapshot", "timeout", serde_json::json!("30000")),
            ("download", "ref", serde_json::json!("1")),
            ("debug.cdp", "params", serde_json::json!([])),
            ("session.cookies.set", "cookies", serde_json::json!({})),
            ("find", "attributes", serde_json::json!(["id", 1])),
            ("connect", "session", serde_json::Value::Null),
        ] {
            let req = Request {
                cmd: command.into(),
                params: serde_json::json!({field: value}),
            };
            let response = validate_request_fields(&req).unwrap_err();
            let json = serde_json::to_value(response).unwrap();

            assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
            assert!(
                json["error"]["message"].as_str().unwrap().contains(field),
                "unexpected error for {command}.{field}: {json}"
            );
        }
    }

    #[tokio::test]
    async fn non_canonical_route_families_remain_outside_the_protocol() {
        let state = Arc::new(DaemonState::new());
        for command in [
            "v2.open",
            "ws.list",
            "tab.list",
            "nav.goto",
            "page.wait",
            "storage.local.get",
            "network.monitor",
            "cdp.events",
        ] {
            let request = Request {
                cmd: command.into(),
                params: serde_json::json!({}),
            };
            let value =
                serde_json::to_value(handle_request(&request, &state, &test_context()).await)
                    .unwrap();
            assert_unknown_error(&value, command);
        }
    }

    #[tokio::test]
    async fn dispatch_wait_uses_session_handler() {
        let state = Arc::new(DaemonState::new());
        let req = Request {
            cmd: "wait".into(),
            params: serde_json::json!({"selector": "#app"}),
        };

        let resp = handle_request(&req, &state, &test_context()).await;
        let json = serde_json::to_value(&resp).unwrap();

        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "SESSION_NOT_FOUND");
    }

    #[tokio::test]
    async fn dispatch_network_watch_uses_session_native_handler() {
        let state = Arc::new(DaemonState::new());
        let req = Request {
            cmd: "network.watch".into(),
            params: serde_json::json!({
                "pattern": "/api/orders",
                "count": 3,
                "timeout": 5000
            }),
        };

        let resp = handle_request(&req, &state, &test_context()).await;
        let json = serde_json::to_value(&resp).unwrap();

        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "SESSION_NOT_FOUND");
    }

    #[tokio::test]
    async fn dispatch_download_uses_session_native_handler() {
        let state = Arc::new(DaemonState::new());
        let output_dir = tempfile::tempdir().unwrap();
        let req = Request {
            cmd: "download".into(),
            params: serde_json::json!({
                "ref": 42,
                "output_dir": output_dir.path(),
                "timeout": 15000
            }),
        };

        let resp = handle_request(&req, &state, &test_context()).await;
        let json = serde_json::to_value(&resp).unwrap();

        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "SESSION_NOT_FOUND");
    }

    #[tokio::test]
    async fn session_storage_routes_require_session_target() {
        let state = Arc::new(DaemonState::new());
        for cmd in [
            "session.storage.local.get",
            "session.storage.local.set",
            "session.storage.export",
            "session.storage.import",
        ] {
            let req = Request {
                cmd: cmd.into(),
                params: serde_json::json!({}),
            };
            let value =
                serde_json::to_value(handle_request(&req, &state, &test_context()).await).unwrap();
            assert_eq!(value["error"]["code"], "SESSION_NOT_FOUND");
        }
    }

    #[tokio::test]
    async fn developer_routes_use_session_resolution() {
        let state = Arc::new(DaemonState::new());
        for cmd in ["debug.cdp", "debug.block", "debug.unblock"] {
            let req = Request {
                cmd: cmd.into(),
                params: serde_json::json!({}),
            };
            let value =
                serde_json::to_value(handle_request(&req, &state, &test_context()).await).unwrap();
            assert_eq!(value["error"]["code"], "SESSION_NOT_FOUND", "{cmd}");
        }
    }

    #[tokio::test]
    async fn canonical_routes_reject_non_string_session_and_target_selectors() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab("T1".into(), "https://example.test".into(), "Example".into());
        state.sessions.insert("default".into(), session);

        let invalid_session_cases = [
            (
                "open",
                serde_json::json!({"url": "https://example.test", "session": false}),
            ),
            ("snapshot", serde_json::json!({"session": false})),
            (
                "navigate",
                serde_json::json!({"reload": true, "session": false}),
            ),
            (
                "evaluate",
                serde_json::json!({"expression": "1", "session": false}),
            ),
            ("screenshot", serde_json::json!({"session": false})),
            (
                "wait",
                serde_json::json!({"selector": "#app", "session": false}),
            ),
            (
                "attach",
                serde_json::json!({"session": false, "target": "T1"}),
            ),
            ("tabs", serde_json::json!({"session": false})),
            ("close", serde_json::json!({"session": false})),
            ("session.close", serde_json::json!({"session": false})),
            ("session.cookies.get", serde_json::json!({"session": false})),
            (
                "session.cookies.set",
                serde_json::json!({"session": false, "cookies": []}),
            ),
            (
                "session.cookies.clear",
                serde_json::json!({"session": false}),
            ),
            (
                "browser.connect",
                serde_json::json!({"host": "remote.example:9222", "session": false}),
            ),
            ("browser.discover", serde_json::json!({"session": false})),
        ];

        for (cmd, params) in invalid_session_cases {
            let request = Request {
                cmd: cmd.into(),
                params,
            };
            let value =
                serde_json::to_value(handle_request(&request, &state, &test_context()).await)
                    .unwrap();
            assert_eq!(value["error"]["code"], "INVALID_ARGUMENT", "{cmd}");
        }

        let invalid_target_cases = [
            ("snapshot", serde_json::json!({"target": false})),
            (
                "navigate",
                serde_json::json!({"reload": true, "target": false}),
            ),
            (
                "evaluate",
                serde_json::json!({"expression": "1", "target": false}),
            ),
            ("screenshot", serde_json::json!({"target": false})),
            (
                "wait",
                serde_json::json!({"selector": "#app", "target": false}),
            ),
            ("attach", serde_json::json!({"target": false})),
            ("close", serde_json::json!({"target": false})),
        ];

        for (cmd, params) in invalid_target_cases {
            let request = Request {
                cmd: cmd.into(),
                params,
            };
            let value =
                serde_json::to_value(handle_request(&request, &state, &test_context()).await)
                    .unwrap();
            assert_eq!(value["error"]["code"], "INVALID_ARGUMENT", "{cmd}");
        }

        for (cmd, params) in [
            ("html", serde_json::json!({"selector": false})),
            ("dialog.policy", serde_json::json!({"policy": false})),
        ] {
            let request = Request {
                cmd: cmd.into(),
                params,
            };
            let value =
                serde_json::to_value(handle_request(&request, &state, &test_context()).await)
                    .unwrap();
            assert_eq!(value["error"]["code"], "INVALID_ARGUMENT", "{cmd}");
        }
    }
}
