// Storage handlers: cookies get/set/clear, localStorage get/set, export, import

use std::sync::Arc;

use serde_json::json;

use cdpkit::Sender;

use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::DaemonState;
use crate::error::BkError;
use crate::page::exception_message;
use super::common::{handler, resolve_context, touch_workspace};

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StorageGetCookies {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) browser_context_id: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct StorageGetCookiesResponse {
    pub(crate) cookies: Vec<serde_json::Value>,
}

impl cdpkit::Method for StorageGetCookies {
    type Response = StorageGetCookiesResponse;
    const METHOD: &'static str = "Storage.getCookies";
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StorageSetCookies {
    pub(crate) cookies: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) browser_context_id: Option<String>,
}

impl cdpkit::Method for StorageSetCookies {
    type Response = serde_json::Value;
    const METHOD: &'static str = "Storage.setCookies";
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StorageClearCookies {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) browser_context_id: Option<String>,
}

impl cdpkit::Method for StorageClearCookies {
    type Response = serde_json::Value;
    const METHOD: &'static str = "Storage.clearCookies";
}

handler!(handle_storage_cookies_get, do_storage_cookies_get(req, state));

async fn do_storage_cookies_get(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "storage.cookies.get")?;
    let resp = ctx.cdp.send_cmd(StorageGetCookies { browser_context_id: ctx.browser_context_id }).await?;
    touch_workspace(state, &ctx.wid);
    Ok(Response::ok(json!({ "cookies": resp.cookies })))
}

handler!(handle_storage_cookies_set, do_storage_cookies_set(req, state));

async fn do_storage_cookies_set(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "storage.cookies.set")?;
    let cookies = req
        .params
        .get("cookies")
        .and_then(|v| v.as_array())
        .ok_or_else(|| BkError::InvalidRequest("storage.cookies.set requires 'cookies' array param".into()))?
        .clone();
    ctx.cdp.send_cmd(StorageSetCookies { cookies, browser_context_id: ctx.browser_context_id }).await?;
    touch_workspace(state, &ctx.wid);
    Ok(Response::ok(json!({ "status": "cookies set" })))
}

handler!(handle_storage_cookies_clear, do_storage_cookies_clear(req, state));

async fn do_storage_cookies_clear(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "storage.cookies.clear")?;
    ctx.cdp.send_cmd(StorageClearCookies { browser_context_id: ctx.browser_context_id }).await?;
    touch_workspace(state, &ctx.wid);
    Ok(Response::ok(json!({ "status": "cookies cleared" })))
}

handler!(handle_storage_local_get, do_storage_local_get(req, state));

async fn do_storage_local_get(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "storage.local.get")?;
    let key = req.params.get("key").and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("storage.local.get requires 'key' param".into()))?;
    // serde_json::to_string produces a quoted JS string literal — use directly
    let json_key = serde_json::to_string(key)
        .map_err(|e| BkError::Other(format!("failed to serialize key: {}", e)))?;
    let js = format!("window.localStorage.getItem({})", json_key);
    let session = ctx.cdp.session(&ctx.cdp_session_id);
    let resp = cdpkit::runtime::methods::Evaluate::new(&js).with_return_by_value(true).send(&session).await?;
    if let Some(details) = &resp.exception_details {
        return Err(BkError::Other(format!("JS exception: {}", exception_message(details))));
    }
    let value = resp.result.value.unwrap_or(serde_json::Value::Null);
    touch_workspace(state, &ctx.wid);
    Ok(Response::ok(json!({ "key": key, "value": value })))
}

handler!(handle_storage_local_set, do_storage_local_set(req, state));

async fn do_storage_local_set(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "storage.local.set")?;
    let key = req.params.get("key").and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("storage.local.set requires 'key' param".into()))?;
    let value = req.params.get("value").and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("storage.local.set requires 'value' param".into()))?;
    // serde_json::to_string produces quoted JS string literals — use directly
    let json_key = serde_json::to_string(key)
        .map_err(|e| BkError::Other(format!("failed to serialize key: {}", e)))?;
    let json_value = serde_json::to_string(value)
        .map_err(|e| BkError::Other(format!("failed to serialize value: {}", e)))?;
    let js = format!("window.localStorage.setItem({}, {})", json_key, json_value);
    let session = ctx.cdp.session(&ctx.cdp_session_id);
    let resp = cdpkit::runtime::methods::Evaluate::new(&js).with_return_by_value(true).send(&session).await?;
    if let Some(details) = &resp.exception_details {
        return Err(BkError::Other(format!("JS exception: {}", exception_message(details))));
    }
    touch_workspace(state, &ctx.wid);
    Ok(Response::ok(json!({ "key": key, "value": value, "status": "set" })))
}

handler!(handle_storage_export, do_storage_export(req, state));

async fn do_storage_export(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "storage.export")?;
    let cookie_resp = ctx.cdp.send_cmd(StorageGetCookies { browser_context_id: ctx.browser_context_id }).await?;
    let session = ctx.cdp.session(&ctx.cdp_session_id);
    let ls_resp = cdpkit::runtime::methods::Evaluate::new(
        "JSON.stringify(Object.fromEntries(Object.entries(window.localStorage)))",
    )
    .with_return_by_value(true)
    .send(&session)
    .await?;
    let local_storage = if let Some(details) = &ls_resp.exception_details {
        tracing::warn!("localStorage export failed: {}", exception_message(details));
        json!({})
    } else {
        match ls_resp.result.value {
            Some(serde_json::Value::String(ref s)) => serde_json::from_str(s).unwrap_or(json!({})),
            _ => json!({}),
        }
    };
    touch_workspace(state, &ctx.wid);
    Ok(Response::ok(json!({ "cookies": cookie_resp.cookies, "localStorage": local_storage })))
}

handler!(handle_storage_import, do_storage_import(req, state));

async fn do_storage_import(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "storage.import")?;
    let import_state = req
        .params
        .get("state")
        .ok_or_else(|| BkError::InvalidRequest("storage.import requires 'state' param".into()))?;

    if let Some(cookies) = import_state.get("cookies").and_then(|v| v.as_array()) {
        ctx.cdp.send_cmd(StorageClearCookies { browser_context_id: ctx.browser_context_id.clone() }).await?;
        if !cookies.is_empty() {
            ctx.cdp.send_cmd(StorageSetCookies { cookies: cookies.clone(), browser_context_id: ctx.browser_context_id }).await?;
        }
    }

    if let Some(local_storage) = import_state.get("localStorage") {
        if let Some(obj) = local_storage.as_object() {
            // Serialize the object to a JSON string, then embed that string as a
            // JS string literal via serde_json::to_string (which adds quotes and escapes).
            // In JS we need one JSON.parse to turn that string back into an object.
            let json_str = serde_json::to_string(obj)
                .map_err(|e| BkError::Other(format!("failed to serialize localStorage: {}", e)))?;
            let json_literal = serde_json::to_string(&json_str)
                .map_err(|e| BkError::Other(format!("failed to escape localStorage string: {}", e)))?;
            let js = format!(
                "(() => {{ window.localStorage.clear(); const d = JSON.parse({}); for (const [k, v] of Object.entries(d)) {{ window.localStorage.setItem(k, v); }} }})()",
                json_literal
            );
            let session = ctx.cdp.session(&ctx.cdp_session_id);
            let resp = cdpkit::runtime::methods::Evaluate::new(&js).with_return_by_value(true).send(&session).await?;
            if let Some(details) = &resp.exception_details {
                return Err(BkError::Other(format!("localStorage import failed: {}", exception_message(details))));
            }
        }
    }

    touch_workspace(state, &ctx.wid);
    Ok(Response::ok(json!({ "status": "imported" })))
}

#[cfg(test)]
mod tests {
    #[test]
    fn local_get_js_no_json_parse() {
        let key = "my_key";
        let json_key = serde_json::to_string(key).unwrap();
        let js = format!("window.localStorage.getItem({})", json_key);
        assert!(!js.contains("JSON.parse"), "should not use JSON.parse: {}", js);
        assert!(js.contains(r#"window.localStorage.getItem("my_key")"#), "got: {}", js);
    }

    #[test]
    fn local_set_js_no_json_parse() {
        let key = "session_id";
        let value = "abc-123";
        let json_key = serde_json::to_string(key).unwrap();
        let json_value = serde_json::to_string(value).unwrap();
        let js = format!("window.localStorage.setItem({}, {})", json_key, json_value);
        assert!(!js.contains("JSON.parse"), "should not use JSON.parse: {}", js);
        assert!(js.contains(r#"setItem("session_id", "abc-123")"#), "got: {}", js);
    }

    #[test]
    fn local_set_js_escapes_special_chars() {
        let key = "key with \"quotes\"";
        let value = "line1\nline2";
        let json_key = serde_json::to_string(key).unwrap();
        let json_value = serde_json::to_string(value).unwrap();
        let js = format!("window.localStorage.setItem({}, {})", json_key, json_value);
        assert!(!js.contains("JSON.parse"));
        assert!(js.contains(r#"key with \"quotes\""#), "got: {}", js);
        assert!(js.contains(r"line1\nline2"), "got: {}", js);
    }

    #[test]
    fn import_js_uses_single_json_parse() {
        use serde_json::json;
        let obj = json!({"token": "abc", "user": "test"});
        let json_str = serde_json::to_string(obj.as_object().unwrap()).unwrap();
        let json_literal = serde_json::to_string(&json_str).unwrap();
        let js = format!(
            "(() => {{ window.localStorage.clear(); const d = JSON.parse({}); for (const [k, v] of Object.entries(d)) {{ window.localStorage.setItem(k, v); }} }})()",
            json_literal
        );
        // Should have exactly one JSON.parse — to decode the serialized object
        let parse_count = js.matches("JSON.parse(").count();
        assert_eq!(parse_count, 1, "should have exactly one JSON.parse, got {}: {}", parse_count, js);
    }
}
