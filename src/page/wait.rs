// Page wait: composite condition waiting

use std::sync::Arc;
use std::time::Duration;

use cdpkit::CDP;
use serde_json::Value;

use crate::error::BkError;
use crate::page::exception_message;

/// Default timeout for page wait operations.
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Polling interval between condition checks.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// All conditions that `page.wait` can evaluate.
#[derive(Debug, Clone)]
pub struct WaitConditions {
    /// Fixed delay in milliseconds.
    pub time: Option<u64>,
    /// CSS selector to wait for (visible).
    pub selector: Option<String>,
    /// Text to wait for appearance.
    pub text: Option<String>,
    /// Text to wait for disappearance.
    pub text_gone: Option<String>,
    /// URL pattern to match (substring or glob with *).
    pub url: Option<String>,
    /// Load state: "load", "domcontentloaded".
    pub load_state: Option<String>,
    /// JS expression to wait for truthy result.
    pub js_fn: Option<String>,
    /// Overall timeout in milliseconds.
    pub timeout: u64,
}

/// Result of a successful wait.
#[derive(Debug, Clone)]
pub struct WaitResult {
    /// Milliseconds elapsed until all conditions were met.
    pub elapsed_ms: u64,
    /// List of condition descriptions that were satisfied.
    pub conditions_met: Vec<String>,
}

impl WaitConditions {
    /// Parse wait conditions from daemon request params.
    pub fn from_params(params: &Value) -> Result<Self, BkError> {
        let time = params.get("time").and_then(|v| v.as_u64());
        let selector = params.get("selector").and_then(|v| v.as_str()).map(|s| s.to_string());
        let text = params.get("text").and_then(|v| v.as_str()).map(|s| s.to_string());
        let text_gone = params.get("text_gone").and_then(|v| v.as_str()).map(|s| s.to_string());
        let url = params.get("url").and_then(|v| v.as_str()).map(|s| s.to_string());
        let load_state = params.get("load_state").and_then(|v| v.as_str()).map(|s| s.to_string());
        let js_fn = params.get("fn").and_then(|v| v.as_str()).map(|s| s.to_string());
        let timeout = params.get("timeout").and_then(|v| v.as_u64()).unwrap_or(DEFAULT_TIMEOUT_MS);

        // Validate load_state if provided
        if let Some(ref ls) = load_state {
            match ls.as_str() {
                "load" | "domcontentloaded" | "networkidle" => {}
                other => {
                    return Err(BkError::InvalidRequest(format!(
                        "invalid load_state '{}', expected: load, domcontentloaded, networkidle",
                        other
                    )));
                }
            }
            if ls == "networkidle" {
                return Err(BkError::InvalidRequest(
                    "load_state 'networkidle' is not yet supported (TODO: requires Network domain event tracking)".into()
                ));
            }
        }

        // Must have at least one condition
        if time.is_none()
            && selector.is_none()
            && text.is_none()
            && text_gone.is_none()
            && url.is_none()
            && load_state.is_none()
            && js_fn.is_none()
        {
            return Err(BkError::InvalidRequest(
                "page.wait requires at least one condition (--time, --selector, --text, --text-gone, --url, --load-state, --fn)".into()
            ));
        }

        Ok(Self {
            time,
            selector,
            text,
            text_gone,
            url,
            load_state,
            js_fn,
            timeout,
        })
    }

    /// Describe which conditions are configured (for error messages).
    fn pending_descriptions(&self) -> Vec<&'static str> {
        let mut descs = Vec::new();
        if self.selector.is_some() { descs.push("selector"); }
        if self.text.is_some() { descs.push("text"); }
        if self.text_gone.is_some() { descs.push("text_gone"); }
        if self.url.is_some() { descs.push("url"); }
        if self.load_state.is_some() { descs.push("load_state"); }
        if self.js_fn.is_some() { descs.push("fn"); }
        descs
    }
}

/// Wait for all conditions to be met, polling at regular intervals.
///
/// Conditions are checked sequentially each poll cycle. The `--time` condition
/// is handled as an initial fixed delay before polling begins.
pub async fn wait_for_conditions(
    cdp: &Arc<CDP>,
    session_id: &str,
    conditions: &WaitConditions,
) -> Result<WaitResult, BkError> {
    let start = tokio::time::Instant::now();
    let timeout = Duration::from_millis(conditions.timeout);

    // Handle fixed time delay first
    if let Some(time_ms) = conditions.time {
        let delay = Duration::from_millis(time_ms);
        if delay > timeout {
            return Err(BkError::Timeout(format!(
                "time delay {}ms exceeds timeout {}ms",
                time_ms, conditions.timeout
            )));
        }
        tokio::time::sleep(delay).await;
    }

    // If time was the only condition, we're done
    let has_poll_conditions = conditions.selector.is_some()
        || conditions.text.is_some()
        || conditions.text_gone.is_some()
        || conditions.url.is_some()
        || conditions.load_state.is_some()
        || conditions.js_fn.is_some();

    if !has_poll_conditions {
        let elapsed = start.elapsed().as_millis() as u64;
        return Ok(WaitResult {
            elapsed_ms: elapsed,
            conditions_met: vec!["time".to_string()],
        });
    }

    // Poll until all conditions met or timeout
    let session = cdp.session(session_id);
    loop {
        if start.elapsed() >= timeout {
            let pending = conditions.pending_descriptions();
            return Err(BkError::Timeout(format!(
                "page.wait timed out after {}ms; unsatisfied conditions: {}",
                conditions.timeout,
                pending.join(", ")
            )));
        }

        let js = build_condition_check_js(conditions);
        let resp = cdpkit::runtime::methods::Evaluate::new(&js)
            .with_return_by_value(true)
            .send(&session)
            .await;

        match resp {
            Ok(r) => {
                if let Some(details) = &r.exception_details {
                    // JS error during check — not a fatal error, keep polling
                    // (page might be navigating)
                    tracing::debug!(
                        "page.wait poll JS error: {}",
                        exception_message(details)
                    );
                } else if let Some(val) = r.result.value.as_ref() {
                    if let Some(result) = val.as_object() {
                        let all_met = result
                            .get("all_met")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        if all_met {
                            let elapsed = start.elapsed().as_millis() as u64;
                            let met = result
                                .get("met")
                                .and_then(|v| v.as_array())
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                        .collect()
                                })
                                .unwrap_or_default();
                            return Ok(WaitResult {
                                elapsed_ms: elapsed,
                                conditions_met: met,
                            });
                        }
                    }
                }
            }
            Err(_) => {
                // CDP error during poll (session not ready, etc.), keep trying
            }
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Build a JS expression that checks all poll-based conditions at once.
///
/// Returns a JSON object: `{ all_met: bool, met: string[] }`
fn build_condition_check_js(conditions: &WaitConditions) -> String {
    let mut checks = Vec::new();

    if let Some(ref selector) = conditions.selector {
        let sel_json = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".to_string());
        checks.push(format!(
            r#"(() => {{
                const el = document.querySelector({sel});
                if (!el) return false;
                const r = el.getBoundingClientRect();
                return r.width > 0 && r.height > 0;
            }})()"#,
            sel = sel_json
        ));
    }

    if let Some(ref text) = conditions.text {
        let text_json = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string());
        checks.push(format!(
            "(document.body && document.body.innerText || '').includes({t})",
            t = text_json
        ));
    }

    if let Some(ref text_gone) = conditions.text_gone {
        let text_json = serde_json::to_string(text_gone).unwrap_or_else(|_| "\"\"".to_string());
        checks.push(format!(
            "!(document.body && document.body.innerText || '').includes({t})",
            t = text_json
        ));
    }

    if let Some(ref url_pattern) = conditions.url {
        let pat_json = serde_json::to_string(url_pattern).unwrap_or_else(|_| "\"\"".to_string());
        // Support simple glob: * matches any sequence of chars
        checks.push(format!(
            r#"(() => {{
                const pat = {pat};
                const url = window.location.href;
                if (!pat.includes('*')) return url.includes(pat);
                const regex = new RegExp('^' + pat.replace(/[.+^${{}}()|[\]\\]/g, '\\$&').replace(/\*/g, '.*') + '$');
                return regex.test(url);
            }})()"#,
            pat = pat_json
        ));
    }

    if let Some(ref load_state) = conditions.load_state {
        let target_state = match load_state.as_str() {
            "domcontentloaded" => r#"(document.readyState === 'interactive' || document.readyState === 'complete')"#,
            // "load" or default
            _ => r#"(document.readyState === 'complete')"#,
        };
        checks.push(target_state.to_string());
    }

    if let Some(ref js_fn) = conditions.js_fn {
        // Wrap in a truthiness check — the user provides a JS expression
        checks.push(format!("(!!({expr}))", expr = js_fn));
    }

    // Build condition names matching the checks
    let mut names = Vec::new();
    if conditions.selector.is_some() { names.push("selector"); }
    if conditions.text.is_some() { names.push("text"); }
    if conditions.text_gone.is_some() { names.push("text_gone"); }
    if conditions.url.is_some() { names.push("url"); }
    if conditions.load_state.is_some() { names.push("load_state"); }
    if conditions.js_fn.is_some() { names.push("fn"); }

    // If time was also a condition, it was already satisfied before polling
    let time_met = if conditions.time.is_some() { r#""time","# } else { "" };

    let checks_js: String = checks
        .iter()
        .enumerate()
        .map(|(i, check)| format!("    const c{i} = {check};", i = i, check = check))
        .collect::<Vec<_>>()
        .join("\n");

    let all_check = (0..checks.len())
        .map(|i| format!("c{}", i))
        .collect::<Vec<_>>()
        .join(" && ");

    let met_array = checks
        .iter()
        .enumerate()
        .map(|(i, _)| format!("c{i} ? \"{name}\" : null", i = i, name = names[i]))
        .collect::<Vec<_>>()
        .join(", ");

    format!(
        r#"(() => {{
{checks}
    const all_met = {all};
    const met = [{time_met}{met}].filter(x => x !== null);
    return {{all_met, met}};
}})()"#,
        checks = checks_js,
        all = all_check,
        time_met = time_met,
        met = met_array
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_conditions_from_params_requires_at_least_one() {
        let params = serde_json::json!({"wid": "abc"});
        let result = WaitConditions::from_params(&params);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("at least one condition"), "got: {}", err);
    }

    #[test]
    fn wait_conditions_from_params_time_only() {
        let params = serde_json::json!({"time": 500});
        let conds = WaitConditions::from_params(&params).unwrap();
        assert_eq!(conds.time, Some(500));
        assert!(conds.selector.is_none());
        assert_eq!(conds.timeout, DEFAULT_TIMEOUT_MS);
    }

    #[test]
    fn wait_conditions_from_params_all_fields() {
        let params = serde_json::json!({
            "time": 100,
            "selector": "#btn",
            "text": "hello",
            "text_gone": "loading",
            "url": "*/dashboard*",
            "load_state": "load",
            "fn": "window.ready === true",
            "timeout": 5000,
        });
        let conds = WaitConditions::from_params(&params).unwrap();
        assert_eq!(conds.time, Some(100));
        assert_eq!(conds.selector.as_deref(), Some("#btn"));
        assert_eq!(conds.text.as_deref(), Some("hello"));
        assert_eq!(conds.text_gone.as_deref(), Some("loading"));
        assert_eq!(conds.url.as_deref(), Some("*/dashboard*"));
        assert_eq!(conds.load_state.as_deref(), Some("load"));
        assert_eq!(conds.js_fn.as_deref(), Some("window.ready === true"));
        assert_eq!(conds.timeout, 5000);
    }

    #[test]
    fn wait_conditions_rejects_networkidle() {
        let params = serde_json::json!({"load_state": "networkidle"});
        let result = WaitConditions::from_params(&params);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("networkidle"), "got: {}", err);
    }

    #[test]
    fn wait_conditions_rejects_invalid_load_state() {
        let params = serde_json::json!({"load_state": "bogus"});
        let result = WaitConditions::from_params(&params);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("invalid load_state"), "got: {}", err);
    }

    #[test]
    fn build_condition_check_js_selector_escaping() {
        let conds = WaitConditions {
            time: None,
            selector: Some(r#"div[data-x="hello"]"#.to_string()),
            text: None,
            text_gone: None,
            url: None,
            load_state: None,
            js_fn: None,
            timeout: 30000,
        };
        let js = build_condition_check_js(&conds);
        // Should contain the properly escaped selector
        assert!(js.contains(r#"div[data-x=\"hello\"]"#), "js: {}", js);
        assert!(js.contains("querySelector"), "js: {}", js);
        assert!(js.contains("all_met"), "js: {}", js);
    }

    #[test]
    fn build_condition_check_js_text_with_special_chars() {
        let conds = WaitConditions {
            time: None,
            selector: None,
            text: Some("it's \"quoted\"\nnewline".to_string()),
            text_gone: None,
            url: None,
            load_state: None,
            js_fn: None,
            timeout: 30000,
        };
        let js = build_condition_check_js(&conds);
        assert!(js.contains("includes("), "js: {}", js);
        // serde_json escapes these properly
        assert!(js.contains(r#"it's \"quoted\"\nnewline"#), "js: {}", js);
    }

    #[test]
    fn build_condition_check_js_url_glob() {
        let conds = WaitConditions {
            time: None,
            selector: None,
            text: None,
            text_gone: None,
            url: Some("https://example.com/*/page".to_string()),
            load_state: None,
            js_fn: None,
            timeout: 30000,
        };
        let js = build_condition_check_js(&conds);
        assert!(js.contains("includes('*')"), "js: {}", js);
        assert!(js.contains("replace"), "js: {}", js);
    }

    #[test]
    fn build_condition_check_js_fn_expression() {
        let conds = WaitConditions {
            time: None,
            selector: None,
            text: None,
            text_gone: None,
            url: None,
            load_state: None,
            js_fn: Some("document.querySelectorAll('.item').length > 3".to_string()),
            timeout: 30000,
        };
        let js = build_condition_check_js(&conds);
        assert!(js.contains("document.querySelectorAll('.item').length > 3"), "js: {}", js);
        assert!(js.contains("!!("), "js: {}", js);
    }

    #[test]
    fn build_condition_check_js_load_state_domcontentloaded() {
        let conds = WaitConditions {
            time: None,
            selector: None,
            text: None,
            text_gone: None,
            url: None,
            load_state: Some("domcontentloaded".to_string()),
            js_fn: None,
            timeout: 30000,
        };
        let js = build_condition_check_js(&conds);
        assert!(js.contains("interactive"), "js: {}", js);
        assert!(js.contains("complete"), "js: {}", js);
    }

    #[test]
    fn build_condition_check_js_multiple_conditions() {
        let conds = WaitConditions {
            time: Some(100),
            selector: Some("#app".to_string()),
            text: Some("Ready".to_string()),
            text_gone: None,
            url: None,
            load_state: None,
            js_fn: None,
            timeout: 30000,
        };
        let js = build_condition_check_js(&conds);
        // Should have both checks
        assert!(js.contains("querySelector"), "js: {}", js);
        assert!(js.contains("includes("), "js: {}", js);
        // Should combine with &&
        assert!(js.contains("c0 && c1"), "js: {}", js);
        // time already met before polling, should be in met array
        assert!(js.contains(r#""time","#), "js: {}", js);
    }

    #[test]
    fn build_condition_check_js_text_gone() {
        let conds = WaitConditions {
            time: None,
            selector: None,
            text: None,
            text_gone: Some("Loading...".to_string()),
            url: None,
            load_state: None,
            js_fn: None,
            timeout: 30000,
        };
        let js = build_condition_check_js(&conds);
        // text_gone uses negation
        assert!(js.contains("!"), "should negate: {}", js);
        assert!(js.contains("includes("), "js: {}", js);
    }

    #[test]
    fn default_timeout_is_30s() {
        assert_eq!(DEFAULT_TIMEOUT_MS, 30_000);
    }
}
