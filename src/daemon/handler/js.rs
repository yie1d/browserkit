// JavaScript execution handler: eval (unified js.eval + js.await)

use std::sync::Arc;

use serde_json::json;

use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::DaemonState;
use crate::error::BkError;
use crate::page::exception_message;
use super::common::{handler, resolve_context, touch_workspace};

handler!(handle_eval, do_eval(req, state));

async fn do_eval(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "js.eval")?;

    let expr = req
        .params
        .get("expr")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("js.eval requires 'expr' param".into()))?;

    let await_promise = req.params.get("await").and_then(|v| v.as_bool()).unwrap_or(true);
    let js_timeout_seconds = state.config.limits.js_timeout_seconds;

    let mut eval = cdpkit::runtime::methods::Evaluate::new(expr).with_return_by_value(true);
    if await_promise {
        eval = eval.with_await_promise(true);
    }
    if js_timeout_seconds > 0 {
        eval = eval.with_timeout(js_timeout_seconds as f64 * 1000.0);
    }

    let session = ctx.cdp.session(&ctx.cdp_session_id);
    let resp = eval.send(&session).await?;

    if let Some(details) = &resp.exception_details {
        return Err(BkError::JsError(exception_message(details)));
    }

    let result = resp.result.value.unwrap_or(serde_json::Value::Null);
    touch_workspace(state, &ctx.wid);

    Ok(Response::ok(json!({ "result": result })))
}
