// Page wait: composite condition waiting

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use cdpkit::CDP;
use futures::StreamExt;
use serde_json::Value;

use crate::error::BkError;
use crate::page::exception_message;

/// Default timeout for page wait operations.
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Polling interval between condition checks.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Quiet window for networkidle: in-flight count must remain 0 for this duration.
const NETWORK_IDLE_QUIET_MS: u64 = 500;

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
    /// Whether to wait for network idle (no in-flight requests for 500ms).
    pub networkidle: bool,
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
        let selector = params
            .get("selector")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let text = params
            .get("text")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let text_gone = params
            .get("text_gone")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let url = params
            .get("url")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let load_state = params
            .get("load_state")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let js_fn = params
            .get("fn")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let timeout = params
            .get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_MS);

        // Validate load_state if provided; "networkidle" is parsed into its own flag
        let mut networkidle = false;
        if let Some(ref ls) = load_state {
            match ls.as_str() {
                "load" | "domcontentloaded" => {}
                "networkidle" => {
                    networkidle = true;
                }
                other => {
                    return Err(BkError::InvalidRequest(format!(
                        "invalid load_state '{}', expected: load, domcontentloaded, networkidle",
                        other
                    )));
                }
            }
        }

        // Strip "networkidle" from load_state since it's handled separately
        let load_state = load_state.filter(|ls| ls != "networkidle");

        // Must have at least one condition
        if time.is_none()
            && selector.is_none()
            && text.is_none()
            && text_gone.is_none()
            && url.is_none()
            && load_state.is_none()
            && !networkidle
            && js_fn.is_none()
        {
            return Err(BkError::InvalidRequest(
                "wait requires at least one condition (--time, --selector, --text, --text-gone, --url, --load-state, --fn)".into()
            ));
        }

        Ok(Self {
            time,
            selector,
            text,
            text_gone,
            url,
            load_state,
            networkidle,
            js_fn,
            timeout,
        })
    }

    /// Describe which conditions are configured (for error messages).
    fn pending_descriptions(&self) -> Vec<&'static str> {
        let mut descs = Vec::new();
        if self.selector.is_some() {
            descs.push("selector");
        }
        if self.text.is_some() {
            descs.push("text");
        }
        if self.text_gone.is_some() {
            descs.push("text_gone");
        }
        if self.url.is_some() {
            descs.push("url");
        }
        if self.load_state.is_some() {
            descs.push("load_state");
        }
        if self.networkidle {
            descs.push("networkidle");
        }
        if self.js_fn.is_some() {
            descs.push("fn");
        }
        descs
    }
}

/// Wait for all conditions to be met, polling at regular intervals.
///
/// Conditions are checked sequentially each poll cycle. The `--time` condition
/// is handled as an initial fixed delay before polling begins.
///
/// If `networkidle` is requested, network events are subscribed and tracked
/// concurrently: the condition is satisfied when no in-flight requests exist
/// for a quiet window of 500ms.
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

    if !has_poll_conditions && !conditions.networkidle {
        let elapsed = start.elapsed().as_millis() as u64;
        return Ok(WaitResult {
            elapsed_ms: elapsed,
            conditions_met: vec!["time".to_string()],
        });
    }

    let session = cdp.session(session_id);

    // Set up networkidle tracking if requested.
    // Subscribe BEFORE any polling begins so we don't miss events.
    let mut req_stream = if conditions.networkidle {
        Some(cdpkit::network::events::RequestWillBeSent::subscribe(
            &session,
        ))
    } else {
        None
    };
    let mut fin_stream = if conditions.networkidle {
        Some(cdpkit::network::events::LoadingFinished::subscribe(
            &session,
        ))
    } else {
        None
    };
    let mut fail_stream = if conditions.networkidle {
        Some(cdpkit::network::events::LoadingFailed::subscribe(&session))
    } else {
        None
    };

    let mut idle_tracker = NetworkIdleCounter::new();

    // Poll until all conditions met or timeout
    let mut poll_interval = tokio::time::interval(POLL_INTERVAL);
    poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        if start.elapsed() >= timeout {
            let pending = conditions.pending_descriptions();
            return Err(BkError::Timeout(format!(
                "wait timed out after {}ms; unsatisfied conditions: {}",
                conditions.timeout,
                pending.join(", ")
            )));
        }

        // Use select to handle both network events and poll ticks
        tokio::select! {
            biased;

            // Drain network events (only if networkidle is requested)
            Some(ev) = async {
                match req_stream.as_mut() {
                    Some(s) => s.next().await,
                    None => std::future::pending::<Option<cdpkit::network::events::RequestWillBeSent>>().await,
                }
            } => {
                idle_tracker.request_start(&ev.request_id);
                continue;
            }
            Some(ev) = async {
                match fin_stream.as_mut() {
                    Some(s) => s.next().await,
                    None => std::future::pending::<Option<cdpkit::network::events::LoadingFinished>>().await,
                }
            } => {
                idle_tracker.request_end(&ev.request_id);
                continue;
            }
            Some(ev) = async {
                match fail_stream.as_mut() {
                    Some(s) => s.next().await,
                    None => std::future::pending::<Option<cdpkit::network::events::LoadingFailed>>().await,
                }
            } => {
                idle_tracker.request_end(&ev.request_id);
                continue;
            }

            // Poll tick: check conditions
            _ = poll_interval.tick() => {}
        }

        // Check poll-based conditions (selector/text/url/load_state/fn)
        let poll_met = if has_poll_conditions {
            check_poll_conditions(&session, conditions).await
        } else {
            // No poll conditions — consider them satisfied
            Some(Vec::new())
        };

        // Check networkidle
        let networkidle_met = if conditions.networkidle {
            idle_tracker.is_idle_for_duration(NETWORK_IDLE_QUIET_MS)
        } else {
            true
        };

        if let Some(mut met_list) = poll_met {
            if networkidle_met {
                let elapsed = start.elapsed().as_millis() as u64;
                if conditions.time.is_some() {
                    met_list.insert(0, "time".to_string());
                }
                if conditions.networkidle {
                    met_list.push("networkidle".to_string());
                }
                return Ok(WaitResult {
                    elapsed_ms: elapsed,
                    conditions_met: met_list,
                });
            }
        }
    }
}

/// Check poll-based conditions via Runtime.evaluate. Returns Some(met_list) if all
/// poll conditions are satisfied, None otherwise.
async fn check_poll_conditions(
    session: &cdpkit::Session<'_>,
    conditions: &WaitConditions,
) -> Option<Vec<String>> {
    let js = build_condition_check_js(conditions);
    let resp = cdpkit::runtime::methods::Evaluate::new(&js)
        .with_return_by_value(true)
        .send(session)
        .await;

    match resp {
        Ok(r) => {
            if let Some(details) = &r.exception_details {
                tracing::debug!("wait poll JS error: {}", exception_message(details));
                None
            } else if let Some(val) = r.result.value.as_ref() {
                if let Some(result) = val.as_object() {
                    let all_met = result
                        .get("all_met")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if all_met {
                        let met = result
                            .get("met")
                            .and_then(|v| v.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                    .collect()
                            })
                            .unwrap_or_default();
                        Some(met)
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        }
        Err(_) => None,
    }
}

// ─── Network Idle Tracking ──────────────────────────────────────────────────

/// Tracks in-flight network requests and determines when network is idle.
///
/// Uses a `HashSet<RequestId>` to track in-flight requests. Idle is determined
/// by the in-flight set being empty for a continuous duration (quiet window).
pub struct NetworkIdleCounter {
    in_flight: HashSet<String>,
    /// Instant at which in_flight last became empty (None if currently non-empty).
    idle_since: Option<tokio::time::Instant>,
}

impl Default for NetworkIdleCounter {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkIdleCounter {
    /// Create a new counter. Starts as idle (no pending requests).
    pub fn new() -> Self {
        Self {
            in_flight: HashSet::new(),
            idle_since: Some(tokio::time::Instant::now()),
        }
    }

    /// Record a new request starting.
    pub fn request_start(&mut self, request_id: &str) {
        self.in_flight.insert(request_id.to_string());
        self.idle_since = None;
    }

    /// Record a request completing (finished or failed). Clamps at 0.
    pub fn request_end(&mut self, request_id: &str) {
        self.in_flight.remove(request_id);
        if self.in_flight.is_empty() && self.idle_since.is_none() {
            self.idle_since = Some(tokio::time::Instant::now());
        }
    }

    /// Returns the number of in-flight requests.
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    /// Check if idle for at least `quiet_ms` milliseconds.
    pub fn is_idle_for_duration(&self, quiet_ms: u64) -> bool {
        match self.idle_since {
            Some(since) => since.elapsed() >= Duration::from_millis(quiet_ms),
            None => false,
        }
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
            "domcontentloaded" => {
                r#"(document.readyState === 'interactive' || document.readyState === 'complete')"#
            }
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
    if conditions.selector.is_some() {
        names.push("selector");
    }
    if conditions.text.is_some() {
        names.push("text");
    }
    if conditions.text_gone.is_some() {
        names.push("text_gone");
    }
    if conditions.url.is_some() {
        names.push("url");
    }
    if conditions.load_state.is_some() {
        names.push("load_state");
    }
    if conditions.js_fn.is_some() {
        names.push("fn");
    }

    // If time was also a condition, it was already satisfied before polling
    let time_met = if conditions.time.is_some() {
        r#""time","#
    } else {
        ""
    };

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
        let params = serde_json::json!({"session": "agent"});
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
        assert!(!conds.networkidle);
        assert_eq!(conds.js_fn.as_deref(), Some("window.ready === true"));
        assert_eq!(conds.timeout, 5000);
    }

    #[test]
    fn wait_conditions_accepts_networkidle() {
        let params = serde_json::json!({"load_state": "networkidle"});
        let conds = WaitConditions::from_params(&params).unwrap();
        assert!(conds.networkidle);
        // load_state is filtered out (handled as separate flag)
        assert!(conds.load_state.is_none());
    }

    #[test]
    fn wait_conditions_networkidle_with_other_conditions() {
        let params = serde_json::json!({
            "load_state": "networkidle",
            "selector": "#app",
            "timeout": 10000,
        });
        let conds = WaitConditions::from_params(&params).unwrap();
        assert!(conds.networkidle);
        assert_eq!(conds.selector.as_deref(), Some("#app"));
        assert!(conds.load_state.is_none());
        assert_eq!(conds.timeout, 10000);
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
    fn wait_conditions_pending_descriptions_includes_networkidle() {
        let conds = WaitConditions {
            time: None,
            selector: Some("#x".into()),
            text: None,
            text_gone: None,
            url: None,
            load_state: None,
            networkidle: true,
            js_fn: None,
            timeout: 30000,
        };
        let descs = conds.pending_descriptions();
        assert!(descs.contains(&"selector"));
        assert!(descs.contains(&"networkidle"));
    }

    // ─── NetworkIdleCounter unit tests ──────────────────────────────────────

    #[tokio::test]
    async fn network_idle_counter_starts_idle() {
        let counter = NetworkIdleCounter::new();
        assert_eq!(counter.in_flight_count(), 0);
        // After 500ms+ the quiet window is satisfied
        tokio::time::sleep(Duration::from_millis(510)).await;
        assert!(counter.is_idle_for_duration(500));
    }

    #[tokio::test]
    async fn network_idle_counter_request_increments() {
        let mut counter = NetworkIdleCounter::new();
        counter.request_start("r1");
        assert_eq!(counter.in_flight_count(), 1);
        assert!(!counter.is_idle_for_duration(50));

        counter.request_start("r2");
        assert_eq!(counter.in_flight_count(), 2);
    }

    #[tokio::test]
    async fn network_idle_counter_request_end_decrements() {
        let mut counter = NetworkIdleCounter::new();
        counter.request_start("r1");
        counter.request_start("r2");
        counter.request_end("r1");
        assert_eq!(counter.in_flight_count(), 1);
        assert!(!counter.is_idle_for_duration(0));

        counter.request_end("r2");
        assert_eq!(counter.in_flight_count(), 0);
        // Just became idle — quiet window hasn't elapsed yet
        assert!(!counter.is_idle_for_duration(500));
    }

    #[tokio::test]
    async fn network_idle_counter_clamps_at_zero() {
        let mut counter = NetworkIdleCounter::new();
        // End without start — should not go negative, just stays at 0
        counter.request_end("unknown");
        assert_eq!(counter.in_flight_count(), 0);
    }

    #[tokio::test]
    async fn network_idle_counter_duplicate_request_id() {
        let mut counter = NetworkIdleCounter::new();
        counter.request_start("r1");
        counter.request_start("r1"); // duplicate
        assert_eq!(counter.in_flight_count(), 1); // HashSet deduplicates
        counter.request_end("r1");
        assert_eq!(counter.in_flight_count(), 0);
    }

    #[tokio::test]
    async fn network_idle_counter_quiet_window() {
        let mut counter = NetworkIdleCounter::new();
        counter.request_start("r1");
        counter.request_end("r1");

        // Not enough time elapsed
        assert!(!counter.is_idle_for_duration(500));

        // Wait for quiet window
        tokio::time::sleep(Duration::from_millis(510)).await;
        assert!(counter.is_idle_for_duration(500));
    }

    #[tokio::test]
    async fn network_idle_counter_quiet_window_resets_on_new_request() {
        let mut counter = NetworkIdleCounter::new();
        counter.request_start("r1");
        counter.request_end("r1");

        tokio::time::sleep(Duration::from_millis(300)).await;

        // New request arrives — breaks the quiet window
        counter.request_start("r2");
        assert!(!counter.is_idle_for_duration(500));

        counter.request_end("r2");
        // Quiet window restarts from now
        assert!(!counter.is_idle_for_duration(500));

        tokio::time::sleep(Duration::from_millis(510)).await;
        assert!(counter.is_idle_for_duration(500));
    }

    #[tokio::test]
    async fn network_idle_counter_interleaved_requests() {
        let mut counter = NetworkIdleCounter::new();
        counter.request_start("r1");
        counter.request_start("r2");
        counter.request_start("r3");
        assert_eq!(counter.in_flight_count(), 3);

        counter.request_end("r2");
        assert_eq!(counter.in_flight_count(), 2);
        // Not idle yet
        assert!(!counter.is_idle_for_duration(0));

        counter.request_end("r1");
        counter.request_end("r3");
        assert_eq!(counter.in_flight_count(), 0);

        tokio::time::sleep(Duration::from_millis(510)).await;
        assert!(counter.is_idle_for_duration(500));
    }

    // ─── build_condition_check_js tests ─────────────────────────────────────

    #[test]
    fn build_condition_check_js_selector_escaping() {
        let conds = WaitConditions {
            time: None,
            selector: Some(r#"div[data-x="hello"]"#.to_string()),
            text: None,
            text_gone: None,
            url: None,
            load_state: None,
            networkidle: false,
            js_fn: None,
            timeout: 30000,
        };
        let js = build_condition_check_js(&conds);
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
            networkidle: false,
            js_fn: None,
            timeout: 30000,
        };
        let js = build_condition_check_js(&conds);
        assert!(js.contains("includes("), "js: {}", js);
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
            networkidle: false,
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
            networkidle: false,
            js_fn: Some("document.querySelectorAll('.item').length > 3".to_string()),
            timeout: 30000,
        };
        let js = build_condition_check_js(&conds);
        assert!(
            js.contains("document.querySelectorAll('.item').length > 3"),
            "js: {}",
            js
        );
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
            networkidle: false,
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
            networkidle: false,
            js_fn: None,
            timeout: 30000,
        };
        let js = build_condition_check_js(&conds);
        assert!(js.contains("querySelector"), "js: {}", js);
        assert!(js.contains("includes("), "js: {}", js);
        assert!(js.contains("c0 && c1"), "js: {}", js);
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
            networkidle: false,
            js_fn: None,
            timeout: 30000,
        };
        let js = build_condition_check_js(&conds);
        assert!(js.contains("!"), "should negate: {}", js);
        assert!(js.contains("includes("), "js: {}", js);
    }

    #[test]
    fn default_timeout_is_30s() {
        assert_eq!(DEFAULT_TIMEOUT_MS, 30_000);
    }

    #[test]
    fn network_idle_quiet_window_is_500ms() {
        assert_eq!(NETWORK_IDLE_QUIET_MS, 500);
    }
}
