use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};

use cdpkit::dom::methods::DescribeNode;
use cdpkit::dom::types::BackendNodeId;
use cdpkit::page::methods::CreateIsolatedWorld;
use cdpkit::runtime::methods::{CallFunctionOn, Evaluate, ReleaseObjectGroup};
use serde_json::{json, Value};

use crate::runtime::{
    BrowserError, CleanupFailure, LocatorFailure, OperationPhase, OwnershipCleanupError, Page,
};

use super::actionability::ActionabilityFacts;
use super::{LocatorMatch, LocatorPlan, LocatorQuery, TextMatcher};

const ERROR_PREFIX: &str = "__browserkit_locator__:";
const ISOLATED_WORLD_NAME: &str = "browserkit-locator";
static NEXT_OBJECT_GROUP: AtomicU64 = AtomicU64::new(1);

pub(in crate::runtime) struct ResolvedElement<'operation> {
    pub(in crate::runtime) session: cdpkit::Session,
    pub(in crate::runtime) backend_node_id: BackendNodeId,
    pub(in crate::runtime) facts: ActionabilityFacts,
    pub(in crate::runtime) route: super::super::frame::LocatorFrameRoute,
    _operation: PhantomData<&'operation super::super::page::PageOperation>,
}

#[cfg(test)]
#[derive(Debug)]
pub(super) struct ResolutionSummary {
    pub(super) backend_node_id: BackendNodeId,
    pub(super) session_id: String,
    pub(super) facts: ActionabilityFacts,
}

pub(super) async fn resolve<'operation>(
    page: &Page,
    store: &super::super::FrameStore,
    route: &super::super::frame::LocatorFrameRoute,
    plan: &LocatorPlan,
    _operation: &'operation super::super::page::PageOperation,
) -> Result<ResolvedElement<'operation>, BrowserError> {
    let world = CreateIsolatedWorld::new(route.frame_id.as_str().to_owned())
        .with_world_name(ISOLATED_WORLD_NAME)
        .with_grant_univeral_access(false)
        .send(&route.session)
        .await
        .map_err(|error| {
            route_or_cdp_error(
                store,
                route,
                "create locator isolated world",
                OperationPhase::Observation,
                error,
            )
        })?;
    store.validate_locator_route(route)?;

    let sequence = NEXT_OBJECT_GROUP.fetch_add(1, Ordering::Relaxed);
    let object_group = format!("browserkit-locator-{}-{sequence}", page.target_id());
    let release_session = route.session.clone();
    let release_group = object_group.clone();
    let cleanup = page.track_locator_cleanup(object_group.clone(), move || async move {
        match ReleaseObjectGroup::new(release_group)
            .send(&release_session)
            .await
            .map_err(OwnershipCleanupError::from)
        {
            Err(error) if error.is_missing_session() || error.is_missing_target() => Ok(()),
            result => result,
        }
    });

    let primary = async {
        let response = Evaluate::new(resolution_expression(plan))
            .with_object_group(object_group.clone())
            .with_context_id(world.execution_context_id)
            .send(&route.session)
            .await
            .map_err(|error| {
                BrowserError::cdp_operation("resolve locator", OperationPhase::Observation, error)
            })?;
        if let Some(exception) = response.exception_details {
            return Err(
                BrowserError::operation("resolve locator", OperationPhase::Observation)
                    .with_message(format!("locator evaluation failed: {}", exception.text)),
            );
        }
        if let Some(value) = response.result.value.as_ref().and_then(Value::as_str) {
            if let Some(payload) = value.strip_prefix(ERROR_PREFIX) {
                return Err(resolution_failure(payload));
            }
        }
        let object_id = response.result.object_id.ok_or_else(|| {
            BrowserError::operation("resolve locator", OperationPhase::Observation)
                .with_message("locator evaluation did not return an element")
        })?;

        let response = CallFunctionOn::new(ACTIONABILITY_FUNCTION)
            .with_object_id(object_id.clone())
            .with_return_by_value(true)
            .with_await_promise(true)
            .send(&route.session)
            .await
            .map_err(|error| {
                BrowserError::cdp_operation(
                    "observe locator actionability",
                    OperationPhase::Observation,
                    error,
                )
            })?;
        if let Some(exception) = response.exception_details {
            return Err(BrowserError::operation(
                "observe locator actionability",
                OperationPhase::Observation,
            )
            .with_message(format!(
                "actionability observation failed: {}",
                exception.text
            )));
        }
        let facts = serde_json::from_value::<ActionabilityFacts>(
            response.result.value.ok_or_else(|| {
                BrowserError::operation(
                    "observe locator actionability",
                    OperationPhase::Observation,
                )
                .with_message("actionability observation returned no facts")
            })?,
        )
        .map_err(|error| {
            BrowserError::operation("observe locator actionability", OperationPhase::Observation)
                .with_message(format!("invalid actionability facts: {error}"))
        })?;
        let node = DescribeNode::new()
            .with_object_id(object_id.clone())
            .send(&route.session)
            .await
            .map_err(|error| {
                BrowserError::cdp_operation(
                    "describe resolved locator",
                    OperationPhase::Observation,
                    error,
                )
            })?
            .node;
        Ok((node.backend_node_id, facts))
    }
    .await;
    let primary = match store.validate_locator_route(route) {
        Ok(()) => primary,
        Err(stale) => Err(stale),
    };
    let cleanup_result = cleanup.cleanup().await;
    let (backend_node_id, facts) = match (primary, cleanup_result) {
        (Ok(resolved), Ok(())) => resolved,
        (Ok(_), Err(cleanup_error)) => {
            return Err(BrowserError::operation(
                "release locator object group",
                OperationPhase::Cleanup,
            )
            .with_message(format!(
                "failed to release locator object group: {cleanup_error}"
            )));
        }
        (Err(error), Ok(())) => return Err(error),
        (Err(error), Err(cleanup_error)) => {
            return Err(error.with_cleanup_failure(CleanupFailure::new(
                object_group,
                cleanup_error.to_string(),
            )));
        }
    };

    Ok(ResolvedElement {
        session: route.session.clone(),
        backend_node_id,
        facts,
        route: route.clone(),
        _operation: PhantomData,
    })
}

pub(super) async fn count(
    store: &super::super::FrameStore,
    route: &super::super::frame::LocatorFrameRoute,
    plan: &LocatorPlan,
) -> Result<usize, BrowserError> {
    let world = CreateIsolatedWorld::new(route.frame_id.as_str().to_owned())
        .with_world_name(ISOLATED_WORLD_NAME)
        .with_grant_univeral_access(false)
        .send(&route.session)
        .await
        .map_err(|error| {
            route_or_cdp_error(
                store,
                route,
                "create locator count world",
                OperationPhase::Observation,
                error,
            )
        })?;
    store.validate_locator_route(route)?;
    let response = Evaluate::new(count_expression(plan))
        .with_context_id(world.execution_context_id)
        .with_return_by_value(true)
        .send(&route.session)
        .await
        .map_err(|error| {
            BrowserError::cdp_operation("count locator matches", OperationPhase::Observation, error)
        })?;
    if let Some(exception) = response.exception_details {
        return Err(
            BrowserError::operation("count locator matches", OperationPhase::Observation)
                .with_message(format!(
                    "locator count evaluation failed: {}",
                    exception.text
                )),
        );
    }
    if let Some(value) = response.result.value.as_ref().and_then(Value::as_str) {
        if let Some(payload) = value.strip_prefix(ERROR_PREFIX) {
            return Err(resolution_failure(payload));
        }
    }
    response
        .result
        .value
        .as_ref()
        .and_then(Value::as_u64)
        .map(|count| count as usize)
        .ok_or_else(|| {
            BrowserError::operation("count locator matches", OperationPhase::Observation)
                .with_message("locator count evaluation did not return a count")
        })
}

fn route_or_cdp_error(
    store: &super::super::FrameStore,
    route: &super::super::frame::LocatorFrameRoute,
    operation: &'static str,
    phase: OperationPhase,
    error: cdpkit::CdpError,
) -> BrowserError {
    store
        .validate_locator_route(route)
        .err()
        .unwrap_or_else(|| BrowserError::cdp_operation(operation, phase, error))
}

fn resolution_failure(payload: &str) -> BrowserError {
    let parsed = serde_json::from_str::<Value>(payload).unwrap_or(Value::Null);
    let count = parsed
        .get("count")
        .and_then(Value::as_u64)
        .unwrap_or_default() as usize;
    let step = parsed
        .get("step")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let (failure, message) = match parsed.get("kind").and_then(Value::as_str) {
        Some("ambiguous") => (
            LocatorFailure::Ambiguous { match_count: count },
            format!("locator step {step} matched {count} elements in strict mode"),
        ),
        _ => (
            LocatorFailure::NotFound,
            format!("locator step {step} matched no element"),
        ),
    };
    BrowserError::operation("resolve locator", OperationPhase::Observation)
        .with_message(message)
        .with_locator_failure(failure)
}

fn resolution_expression(plan: &LocatorPlan) -> String {
    expression(plan, false)
}

fn count_expression(plan: &LocatorPlan) -> String {
    expression(plan, true)
}

fn expression(plan: &LocatorPlan, count_final: bool) -> String {
    let last = plan.queries().len().saturating_sub(1);
    let steps = plan
        .queries()
        .iter()
        .zip(plan.match_policies())
        .enumerate()
        .map(|(index, (query, policy))| {
            json!({
                "query": query_value(query),
                "match_policy": if count_final && index == last { json!("count") } else { match_policy_value(*policy) },
            })
        })
        .collect::<Vec<_>>();
    RESOLUTION_SCRIPT.replace("__PLAN__", &Value::Array(steps).to_string())
}

fn match_policy_value(policy: LocatorMatch) -> Value {
    match policy {
        LocatorMatch::Strict => json!("strict"),
        LocatorMatch::First => json!("first"),
        LocatorMatch::Last => json!("last"),
        LocatorMatch::Nth(index) => json!({ "nth": index }),
    }
}

fn query_value(query: &LocatorQuery) -> Value {
    match query {
        LocatorQuery::Css(selector) => json!({ "kind": "css", "value": selector }),
        LocatorQuery::XPath(expression) => {
            json!({ "kind": "xpath", "value": expression })
        }
        LocatorQuery::Text(matcher) => {
            json!({ "kind": "text", "matcher": matcher_value(matcher) })
        }
        LocatorQuery::Role(role) => json!({
            "kind": "role",
            "role": role.role(),
            "name": role.name().map(matcher_value),
        }),
        LocatorQuery::Label(matcher) => {
            json!({ "kind": "label", "matcher": matcher_value(matcher) })
        }
        LocatorQuery::Placeholder(matcher) => {
            json!({ "kind": "placeholder", "matcher": matcher_value(matcher) })
        }
        LocatorQuery::TestId(query) => {
            json!({ "kind": "test_id", "value": query.value() })
        }
    }
}

fn matcher_value(matcher: &TextMatcher) -> Value {
    match matcher {
        TextMatcher::Exact {
            value,
            case_sensitive,
        } => json!({
            "kind": "exact", "value": value, "case_sensitive": case_sensitive
        }),
        TextMatcher::Contains {
            value,
            case_sensitive,
        } => json!({
            "kind": "contains", "value": value, "case_sensitive": case_sensitive
        }),
        TextMatcher::Regex {
            pattern,
            case_sensitive,
        } => json!({
            "kind": "regex", "value": pattern, "case_sensitive": case_sensitive
        }),
    }
}

const RESOLUTION_SCRIPT: &str = r#"(() => {
  const plan = __PLAN__;
  const normalize = value => String(value ?? '').replace(/\s+/g, ' ').trim();
  const matches = (value, matcher) => {
    const actual = normalize(value);
    const expected = normalize(matcher.value);
    if (matcher.kind === 'regex') {
      return new RegExp(matcher.value, matcher.case_sensitive ? '' : 'i').test(actual);
    }
    const left = matcher.case_sensitive ? actual : actual.toLocaleLowerCase();
    const right = matcher.case_sensitive ? expected : expected.toLocaleLowerCase();
    return matcher.kind === 'exact' ? left === right : left.includes(right);
  };
  const queryRoots = root => {
    const roots = [root];
    for (let index = 0; index < roots.length; index += 1) {
      for (const element of roots[index].querySelectorAll('*')) {
        if (element.shadowRoot) roots.push(element.shadowRoot);
      }
      if (roots[index] instanceof Element && roots[index].shadowRoot) roots.push(roots[index].shadowRoot);
    }
    return Array.from(new Set(roots));
  };
  const elements = root => queryRoots(root).flatMap(queryRoot => Array.from(queryRoot.querySelectorAll('*')));
  const accessibleName = element => {
    const labelledBy = element.getAttribute('aria-labelledby');
    if (labelledBy) {
      const tree = element.getRootNode();
      return labelledBy.split(/\s+/).map(id => tree.getElementById?.(id)?.textContent ?? '').join(' ');
    }
    if (element.getAttribute('aria-label')) return element.getAttribute('aria-label');
    if (element.labels?.length) return Array.from(element.labels).map(label => label.textContent ?? '').join(' ');
    return element.innerText ?? element.textContent ?? '';
  };
  const implicitRole = element => {
    const tag = element.localName;
    if (tag === 'button') return 'button';
    if (tag === 'a' && element.hasAttribute('href')) return 'link';
    if (tag === 'select') return element.multiple ? 'listbox' : 'combobox';
    if (tag === 'textarea') return 'textbox';
    if (tag === 'input') {
      const type = (element.type || 'text').toLowerCase();
      if (type === 'checkbox') return 'checkbox';
      if (type === 'radio') return 'radio';
      if (type === 'button' || type === 'submit' || type === 'reset') return 'button';
      if (!['hidden', 'file'].includes(type)) return 'textbox';
    }
    return null;
  };
  const query = (root, spec) => {
    if (spec.kind === 'css') return queryRoots(root).flatMap(queryRoot => Array.from(queryRoot.querySelectorAll(spec.value)));
    if (spec.kind === 'xpath') {
      const expression = root === document || !spec.value.startsWith('//') ? spec.value : `.${spec.value}`;
      const result = document.evaluate(expression, root, null, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE, null);
      return Array.from({ length: result.snapshotLength }, (_, index) => result.snapshotItem(index))
        .filter(node => node instanceof Element && (root === document || root.contains(node)));
    }
    if (spec.kind === 'text') {
      const candidates = elements(root).filter(element => matches(element.innerText ?? element.textContent, spec.matcher));
      const matching = new Set(candidates);
      return candidates.filter(element => !Array.from(element.querySelectorAll('*')).some(child => matching.has(child)));
    }
    if (spec.kind === 'role') return elements(root).filter(element =>
      (element.getAttribute('role') || implicitRole(element)) === spec.role &&
      (!spec.name || matches(accessibleName(element), spec.name)));
    if (spec.kind === 'label') return elements(root).filter(element =>
      element.labels?.length && Array.from(element.labels).some(label => matches(label.textContent, spec.matcher)));
    if (spec.kind === 'placeholder') return elements(root).filter(element =>
      element.hasAttribute('placeholder') && matches(element.getAttribute('placeholder'), spec.matcher));
    if (spec.kind === 'test_id') return elements(root).filter(element => element.getAttribute('data-testid') === spec.value);
    return [];
  };
  let roots = [document];
  for (let step = 0; step < plan.length; step += 1) {
    const entry = plan[step];
    const seen = new Set();
    const candidates = roots.flatMap(root => query(root, entry.query)).filter(element => {
      if (seen.has(element)) return false;
      seen.add(element);
      return true;
    });
    const policy = entry.match_policy;
    if (policy === 'count') return candidates.length;
    if (policy === 'strict') {
      if (candidates.length === 0) return '__browserkit_locator__:' + JSON.stringify({ kind: 'not_found', step, count: 0 });
      if (candidates.length > 1) return '__browserkit_locator__:' + JSON.stringify({ kind: 'ambiguous', step, count: candidates.length });
      roots = candidates;
    } else {
      const index = policy === 'first' ? 0 : policy === 'last' ? candidates.length - 1 : policy.nth;
      if (index < 0 || index >= candidates.length) return '__browserkit_locator__:' + JSON.stringify({ kind: 'not_found', step, count: candidates.length });
      roots = [candidates[index]];
    }
  }
  return roots[0];
})()"#;

const ACTIONABILITY_FUNCTION: &str = r#"async function() {
  const measure = element => {
    const rect = element.getBoundingClientRect();
    return { x: rect.x, y: rect.y, width: rect.width, height: rect.height };
  };
  const samples = [measure(this)];
  for (let frame = 0; frame < 3; frame += 1) {
    await new Promise(resolve => requestAnimationFrame(resolve));
    samples.push(measure(this));
  }
  const composedParent = element => element.parentElement || element.getRootNode()?.host || null;
  let visible = this.isConnected;
  for (let element = this; visible && element; element = composedParent(element)) {
    const style = getComputedStyle(element);
    visible = style.display !== 'none' && style.visibility !== 'hidden' &&
      style.contentVisibility !== 'hidden' && Number(style.opacity) > 0;
  }
  const last = samples[samples.length - 1];
  visible = visible && last.width > 0 && last.height > 0;
  let enabled = !this.matches(':disabled');
  for (let element = this; enabled && element; element = composedParent(element)) {
    enabled = element.getAttribute?.('aria-disabled') !== 'true';
  }
  const stable = samples.slice(1).every((sample, index) => {
    const previous = samples[index];
    return sample.x === previous.x && sample.y === previous.y &&
      sample.width === previous.width && sample.height === previous.height;
  });
  const x = last.x + last.width / 2;
  const y = last.y + last.height / 2;
  let hit = visible ? document.elementFromPoint(x, y) : null;
  while (hit?.shadowRoot?.elementFromPoint) {
    const nested = hit.shadowRoot.elementFromPoint(x, y);
    if (!nested || nested === hit) break;
    hit = nested;
  }
  let composedAncestor = this;
  let hitContainsTarget = false;
  while (composedAncestor) {
    if (composedAncestor === hit) hitContainsTarget = true;
    composedAncestor = composedParent(composedAncestor);
  }
  const obscured = visible && hit !== null && hit !== this && !this.contains(hit) && !hitContainsTarget;
  const tag = this.localName;
  const inputType = tag === 'input' ? (this.type || 'text').toLowerCase() : '';
  const editable = !this.readOnly &&
    (tag === 'textarea' || (tag === 'input' && !['button', 'checkbox', 'file', 'hidden', 'radio', 'reset', 'submit'].includes(inputType)) || this.isContentEditable);
  const checkable = tag === 'input' && ['checkbox', 'radio'].includes(inputType);
  const radio = tag === 'input' && inputType === 'radio';
  const selectable = tag === 'select';
  const fileInput = tag === 'input' && inputType === 'file';
  return {
    attached: this.isConnected, visible, enabled, stable, obscured, editable, checkable,
    selectable, file_input: fileInput, checked: checkable && this.checked, radio
  };
}"#;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::runtime::{
        BrowserError, BrowserRuntime, BrowserSessionId, LocatorFailure, LocatorQuery, Page,
        PageOwnership, RoleQuery, TextMatcher,
    };
    use futures::{SinkExt, StreamExt};
    use serde_json::{json, Value};
    use std::sync::{Arc, Weak};
    use tokio_tungstenite::tungstenite::Message;

    #[derive(Clone)]
    pub(crate) struct Fixture {
        pub(crate) evaluation: Value,
        pub(crate) facts: Value,
        pub(crate) block_evaluation: Option<Arc<tokio::sync::Notify>>,
        pub(crate) stall_method: Option<&'static str>,
        pub(crate) stall_release: Option<Arc<tokio::sync::Notify>>,
        pub(crate) stall_started: Option<Arc<tokio::sync::Notify>>,
        pub(crate) stall_occurrence: usize,
        pub(crate) command_error: Option<(&'static str, i64, &'static str)>,
        pub(crate) command_error_occurrence: usize,
        pub(crate) command_error_additional_occurrence: Option<usize>,
        pub(crate) release_error: Option<(i64, &'static str)>,
    }

    async fn start_server(
        fixture: Fixture,
    ) -> (
        String,
        Arc<parking_lot::Mutex<Vec<Value>>>,
        Arc<tokio::sync::Notify>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let commands = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let server_commands = Arc::clone(&commands);
        let evaluation_started = Arc::new(tokio::sync::Notify::new());
        let server_started = Arc::clone(&evaluation_started);
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut write, mut read) = websocket.split();
            let mut method_counts = std::collections::HashMap::<String, usize>::new();
            while let Some(Ok(Message::Text(text))) = read.next().await {
                let command: Value = serde_json::from_str(&text).unwrap();
                server_commands.lock().push(command.clone());
                let id = command["id"].as_u64().unwrap();
                let method = command["method"].as_str().unwrap();
                let occurrence = method_counts.entry(method.to_owned()).or_default();
                *occurrence += 1;
                if fixture.stall_method == Some(method) && *occurrence == fixture.stall_occurrence {
                    server_started.notify_one();
                    if let Some(started) = &fixture.stall_started {
                        started.notify_one();
                    }
                    if let Some(release) = &fixture.stall_release {
                        release.notified().await;
                    } else {
                        continue;
                    }
                }
                if method == "Runtime.releaseObjectGroup" {
                    if let Some((code, message)) = fixture.release_error {
                        let mut response = json!({
                            "id": id,
                            "error": {"code": code, "message": message}
                        });
                        if let Some(session_id) = command.get("sessionId") {
                            response["sessionId"] = session_id.clone();
                        }
                        write
                            .send(Message::Text(response.to_string().into()))
                            .await
                            .unwrap();
                        continue;
                    }
                }
                if let Some((error_method, code, message)) = fixture.command_error {
                    if method == error_method
                        && (*occurrence == fixture.command_error_occurrence
                            || fixture.command_error_additional_occurrence == Some(*occurrence))
                    {
                        if method == "Runtime.evaluate" {
                            server_started.notify_one();
                            if let Some(release) = &fixture.block_evaluation {
                                release.notified().await;
                            }
                        }
                        let mut response = json!({
                            "id": id,
                            "error": {"code": code, "message": message}
                        });
                        if let Some(session_id) = command.get("sessionId") {
                            response["sessionId"] = session_id.clone();
                        }
                        write
                            .send(Message::Text(response.to_string().into()))
                            .await
                            .unwrap();
                        continue;
                    }
                }
                let result = match method {
                    "Browser.getVersion" => crate::runtime::test_browser_version_result(),
                    "Target.getBrowserContexts" => json!({"browserContextIds": []}),
                    "Target.setDiscoverTargets"
                    | "Page.enable"
                    | "Page.disable"
                    | "Target.setAutoAttach"
                    | "Target.detachFromTarget" => json!({}),
                    "Page.getFrameTree" => json!({
                        "frameTree": {
                            "frame": {
                                "id": "main",
                                "loaderId": "loader-main",
                                "url": "about:blank",
                                "domainAndRegistry": "",
                                "securityOrigin": "null",
                                "mimeType": "text/html",
                                "secureContextType": "InsecureScheme",
                                "crossOriginIsolatedContextType": "NotIsolated",
                                "gatedAPIFeatures": []
                            }
                        }
                    }),
                    "Page.createIsolatedWorld" => json!({"executionContextId": 91}),
                    "Runtime.evaluate" => {
                        server_started.notify_one();
                        if let Some(release) = &fixture.block_evaluation {
                            release.notified().await;
                        }
                        json!({"result": fixture.evaluation})
                    }
                    "Runtime.callFunctionOn" => json!({
                        "result": {"type": "object", "value": fixture.facts}
                    }),
                    "DOM.scrollIntoViewIfNeeded"
                    | "DOM.setFileInputFiles"
                    | "DOM.focus"
                    | "Input.dispatchMouseEvent"
                    | "Input.dispatchKeyEvent" => json!({}),
                    "DOM.getBoxModel" => json!({
                        "model": {
                            "content": [10, 20, 30, 20, 30, 40, 10, 40],
                            "padding": [10, 20, 30, 20, 30, 40, 10, 40],
                            "border": [10, 20, 30, 20, 30, 40, 10, 40],
                            "margin": [10, 20, 30, 20, 30, 40, 10, 40],
                            "width": 20, "height": 20
                        }
                    }),
                    "DOM.resolveNode" => json!({
                        "object": {"type": "object", "subtype": "node", "objectId": "action-element"}
                    }),
                    "Runtime.releaseObject" | "Runtime.releaseObjectGroup" => json!({}),
                    "DOM.describeNode" => json!({
                        "node": {
                            "nodeId": 7,
                            "backendNodeId": 41,
                            "nodeType": 1,
                            "nodeName": "BUTTON",
                            "localName": "button",
                            "nodeValue": ""
                        }
                    }),
                    other => panic!("unexpected locator test command: {other}"),
                };
                let mut response = json!({"id": id, "result": result});
                if let Some(session_id) = command.get("sessionId") {
                    response["sessionId"] = session_id.clone();
                }
                write
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .unwrap();
                if method == "Page.disable" {
                    write
                        .send(Message::Text(
                            json!({
                                "method": "Page.frameDetached",
                                "params": {"frameId": "main", "reason": "remove"},
                                "sessionId": "frame-session"
                            })
                            .to_string()
                            .into(),
                        ))
                        .await
                        .unwrap();
                }
            }
        });
        (format!("ws://{address}"), commands, evaluation_started)
    }

    pub(crate) fn success_fixture() -> Fixture {
        Fixture {
            evaluation: json!({
                "type": "object",
                "subtype": "node",
                "className": "HTMLButtonElement",
                "description": "button#save",
                "objectId": "element-1"
            }),
            facts: json!({
                "attached": true,
                "visible": true,
                "enabled": true,
                "stable": true,
                "obscured": false
            }),
            block_evaluation: None,
            stall_method: None,
            stall_release: None,
            stall_started: None,
            stall_occurrence: 1,
            command_error: None,
            command_error_occurrence: 1,
            command_error_additional_occurrence: None,
            release_error: None,
        }
    }

    pub(crate) async fn page_for(
        fixture: Fixture,
    ) -> (
        Page,
        Arc<parking_lot::Mutex<Vec<Value>>>,
        Arc<tokio::sync::Notify>,
    ) {
        let (url, commands, evaluation_started) = start_server(fixture).await;
        let runtime = BrowserRuntime::connect(url).await.unwrap();
        let page = Page::new(
            runtime.clone(),
            BrowserSessionId::new("owner"),
            Weak::new(),
            "target-1".to_owned(),
            PageOwnership::Attached,
            runtime.cdp().session("frame-session"),
        );
        (page, commands, evaluation_started)
    }

    async fn resolve(locator: &crate::runtime::Locator) -> Result<ResolutionSummary, BrowserError> {
        locator.resolve_for_test().await
    }

    #[tokio::test]
    async fn zero_matches_is_structured_not_found() {
        let mut fixture = success_fixture();
        fixture.evaluation = json!({
            "type": "string",
            "value": "__browserkit_locator__:{\"kind\":\"not_found\",\"step\":0,\"count\":0}"
        });
        let (page, _, _) = page_for(fixture).await;
        let error = resolve(&page.locator("button")).await.unwrap_err();
        assert_eq!(error.locator_failure(), Some(&LocatorFailure::NotFound));
    }

    #[tokio::test]
    async fn strict_multiple_matches_is_structured_ambiguity() {
        let mut fixture = success_fixture();
        fixture.evaluation = json!({
            "type": "string",
            "value": "__browserkit_locator__:{\"kind\":\"ambiguous\",\"step\":0,\"count\":3}"
        });
        let (page, _, _) = page_for(fixture).await;
        let error = resolve(&page.locator("button")).await.unwrap_err();
        assert_eq!(
            error.locator_failure(),
            Some(&LocatorFailure::Ambiguous { match_count: 3 })
        );
    }

    #[tokio::test]
    async fn one_match_returns_ephemeral_backend_identity_and_facts() {
        let (page, _, _) = page_for(success_fixture()).await;
        let resolved = resolve(&page.locator("#save")).await.unwrap();
        assert_eq!(resolved.backend_node_id, 41);
        assert_eq!(resolved.session_id, "frame-session");
        assert!(resolved.facts.visible);
        assert!(resolved.facts.enabled);
        assert!(resolved.facts.stable);
        assert!(!resolved.facts.obscured);
    }

    #[tokio::test]
    async fn every_operation_resolves_again_instead_of_caching_a_dom_node() {
        let (page, commands, _) = page_for(success_fixture()).await;
        let locator = page.locator("#save");
        resolve(&locator).await.unwrap();
        resolve(&locator).await.unwrap();

        let commands = commands.lock();
        assert_eq!(
            commands
                .iter()
                .filter(|command| command["method"] == "Runtime.evaluate")
                .count(),
            2
        );
        assert_eq!(
            commands
                .iter()
                .filter(|command| command["method"] == "DOM.describeNode")
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn fake_cdp_actionability_facts_map_to_structured_failures() {
        let cases = [
            ("visible", false, LocatorFailure::NotVisible),
            ("enabled", false, LocatorFailure::Disabled),
            ("stable", false, LocatorFailure::Unstable),
            ("obscured", true, LocatorFailure::Obscured),
        ];
        for (field, value, expected) in cases {
            let mut fixture = success_fixture();
            fixture.facts[field] = json!(value);
            let (page, _, _) = page_for(fixture).await;
            let error = resolve(&page.locator("button")).await.unwrap_err();
            assert_eq!(error.locator_failure(), Some(&expected));
        }
    }

    #[tokio::test]
    async fn detached_frame_locator_fails_closed_before_dom_resolution() {
        use cdpkit::page::methods::Disable;

        let (page, commands, _) = page_for(success_fixture()).await;
        let frame = page.main_frame().await.unwrap();
        Disable::new().send(page.cdp_session()).await.unwrap();
        loop {
            if frame.cdp_session().await.is_err() {
                break;
            }
            tokio::task::yield_now().await;
        }
        let before = commands
            .lock()
            .iter()
            .filter(|command| command["method"] == "Runtime.evaluate")
            .count();
        let error = resolve(&frame.locator("button")).await.unwrap_err();
        assert!(error.to_string().contains("detached"));
        let after = commands
            .lock()
            .iter()
            .filter(|command| command["method"] == "Runtime.evaluate")
            .count();
        assert_eq!(after, before);
    }

    #[tokio::test]
    async fn descendant_plan_keeps_an_ordinal_for_every_step_and_uses_frame_route() {
        let (page, commands, _) = page_for(success_fixture()).await;
        let frame = page.main_frame().await.unwrap();
        let locator = frame
            .locator(LocatorQuery::role(RoleQuery::new("dialog")))
            .first()
            .locator(LocatorQuery::text(TextMatcher::contains("Save", false)))
            .nth(2);
        resolve(&locator).await.unwrap();

        let commands = commands.lock();
        let evaluate = commands
            .iter()
            .find(|command| command["method"] == "Runtime.evaluate")
            .unwrap();
        assert_eq!(evaluate["sessionId"], "frame-session");
        let expression = evaluate["params"]["expression"].as_str().unwrap();
        assert!(expression.contains("\"match_policy\":\"first\""));
        assert!(expression.contains("\"match_policy\":{\"nth\":2}"));
        assert!(expression.contains("\"kind\":\"role\""));
        assert!(expression.contains("\"kind\":\"text\""));
    }

    #[test]
    fn resolver_script_traverses_only_open_shadow_roots_for_supported_queries() {
        let mut plan = LocatorPlan::new(LocatorQuery::css("#save"));
        for query in [
            LocatorQuery::text(TextMatcher::exact("Save", true)),
            LocatorQuery::role(RoleQuery::new("button")),
            LocatorQuery::label(TextMatcher::exact("Email", true)),
            LocatorQuery::placeholder(TextMatcher::exact("Address", true)),
            LocatorQuery::test_id(crate::runtime::TestIdQuery::new("save")),
        ] {
            plan = plan.descendant(query);
        }
        let expression = resolution_expression(&plan);
        assert!(expression.contains("if (element.shadowRoot) roots.push(element.shadowRoot)"));
        assert!(expression.contains("queryRoots(root).flatMap"));
        for kind in ["css", "text", "role", "label", "placeholder", "test_id"] {
            assert!(expression.contains(&format!(r#""kind":"{kind}""#)));
        }
        assert!(!expression.contains("closedShadowRoot"));

        let xpath =
            resolution_expression(&LocatorPlan::new(LocatorQuery::xpath("//*[@id='save']")));
        assert!(xpath.contains("document.evaluate(expression, root"));
        assert!(!xpath.contains("queryRoots(root).flatMap(queryRoot => document.evaluate"));
    }

    #[tokio::test]
    async fn resolution_uses_a_named_non_universal_isolated_world_on_the_frame_route() {
        let (page, commands, _) = page_for(success_fixture()).await;
        let frame = page.main_frame().await.unwrap();
        resolve(&frame.locator("button")).await.unwrap();

        let commands = commands.lock();
        let world = commands
            .iter()
            .find(|command| command["method"] == "Page.createIsolatedWorld")
            .expect("locator must create an isolated world");
        assert_eq!(world["sessionId"], "frame-session");
        assert_eq!(world["params"]["frameId"], "main");
        assert_eq!(world["params"]["worldName"], "browserkit-locator");
        assert_eq!(world["params"]["grantUniveralAccess"], false);
        let evaluate = commands
            .iter()
            .find(|command| command["method"] == "Runtime.evaluate")
            .unwrap();
        assert_eq!(evaluate["params"]["contextId"], 91);
    }

    #[tokio::test]
    async fn every_resolution_uses_a_unique_object_group_and_releases_it() {
        let (page, commands, _) = page_for(success_fixture()).await;
        let locator = page.locator("button");
        resolve(&locator).await.unwrap();
        resolve(&locator).await.unwrap();

        let commands = commands.lock();
        let groups = commands
            .iter()
            .filter(|command| command["method"] == "Runtime.evaluate")
            .map(|command| command["params"]["objectGroup"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_ne!(groups[0], groups[1]);
        for group in groups {
            assert!(commands.iter().any(|command| {
                command["method"] == "Runtime.releaseObjectGroup"
                    && command["params"]["objectGroup"] == group
            }));
        }
    }

    #[tokio::test]
    async fn cancellation_at_each_cdp_stage_releases_the_object_group() {
        for method in [
            "Runtime.evaluate",
            "Runtime.callFunctionOn",
            "DOM.describeNode",
        ] {
            let mut fixture = success_fixture();
            fixture.stall_method = Some(method);
            let (page, commands, stalled) = page_for(fixture).await;
            let locator = page.locator("button");
            let task = tokio::spawn(async move { resolve(&locator).await });
            stalled.notified().await;
            task.abort();
            let _ = task.await;
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                loop {
                    if commands
                        .lock()
                        .iter()
                        .any(|command| command["method"] == "Runtime.releaseObjectGroup")
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("{method} cancellation did not release object group"));
        }
    }

    #[tokio::test]
    async fn successful_resolution_with_release_failure_is_a_cleanup_error() {
        let mut fixture = success_fixture();
        fixture.release_error = Some((-32000, "injected release failure"));
        let (page, _, _) = page_for(fixture).await;
        let error = resolve(&page.locator("button")).await.unwrap_err();
        assert_eq!(error.operation_name(), Some("release locator object group"));
        assert_eq!(error.phase(), OperationPhase::Cleanup);
        assert_eq!(
            error.action_completed(),
            crate::runtime::ActionCompletion::NotStarted
        );
    }

    #[tokio::test]
    async fn primary_resolution_failure_preserves_cleanup_failure() {
        let mut fixture = success_fixture();
        fixture.evaluation = json!({
            "type": "string",
            "value": "__browserkit_locator__:{\"kind\":\"not_found\",\"step\":0,\"count\":0}"
        });
        fixture.release_error = Some((-32000, "injected release failure"));
        let (page, _, _) = page_for(fixture).await;
        let error = resolve(&page.locator("button")).await.unwrap_err();
        assert_eq!(error.locator_failure(), Some(&LocatorFailure::NotFound));
        assert_eq!(error.phase(), OperationPhase::Observation);
        assert_eq!(error.cleanup_failures().len(), 1);
    }

    #[tokio::test]
    async fn release_after_destroyed_session_is_already_complete() {
        let mut fixture = success_fixture();
        fixture.release_error = Some((-32000, "No session with given id"));
        let (page, _, _) = page_for(fixture).await;
        resolve(&page.locator("button")).await.unwrap();
    }

    #[tokio::test]
    async fn release_after_destroyed_target_is_already_complete() {
        let mut fixture = success_fixture();
        fixture.release_error = Some((-32000, "No target with given id"));
        let (page, _, _) = page_for(fixture).await;
        resolve(&page.locator("button")).await.unwrap();
    }

    #[tokio::test]
    async fn cdp_failures_identify_the_observation_that_did_not_start_an_action() {
        let cases = [
            ("Page.createIsolatedWorld", "create locator isolated world"),
            ("Runtime.evaluate", "resolve locator"),
            ("Runtime.callFunctionOn", "observe locator actionability"),
            ("DOM.describeNode", "describe resolved locator"),
        ];
        for (method, operation) in cases {
            let mut fixture = success_fixture();
            fixture.command_error = Some((method, -32000, "injected command failure"));
            let (page, _, _) = page_for(fixture).await;
            let error = resolve(&page.locator("button")).await.unwrap_err();
            assert_eq!(error.operation_name(), Some(operation), "{method}");
            assert_eq!(error.phase(), OperationPhase::Observation, "{method}");
            assert_eq!(
                error.action_completed(),
                crate::runtime::ActionCompletion::NotStarted,
                "{method}"
            );
            assert!(std::error::Error::source(&error).is_some(), "{method}");
        }
    }

    #[tokio::test]
    async fn stale_lifecycle_wins_over_cdp_failure_without_losing_cleanup_failure() {
        let release = Arc::new(tokio::sync::Notify::new());
        let mut fixture = success_fixture();
        fixture.block_evaluation = Some(Arc::clone(&release));
        fixture.command_error = Some(("Runtime.evaluate", -32000, "context destroyed"));
        fixture.release_error = Some((-32000, "injected release failure"));
        let (page, _, started) = page_for(fixture).await;
        let locator = page.locator("button");
        let resolving = tokio::spawn(async move { resolve(&locator).await });
        started.notified().await;
        page.lifecycle().commit_new_document();
        release.notify_one();

        let error = resolving.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("stale"));
        assert_eq!(error.operation_name(), Some("use locator"));
        assert_eq!(error.cleanup_failures().len(), 1);
    }

    #[tokio::test]
    async fn page_close_drains_cancelled_resolution_before_detach() {
        let mut fixture = success_fixture();
        fixture.stall_method = Some("Runtime.evaluate");
        let (page, commands, stalled) = page_for(fixture).await;
        let locator = page.locator("button");
        let task = tokio::spawn(async move { resolve(&locator).await });
        stalled.notified().await;
        task.abort();
        let _ = task.await;
        assert!(page.close().await.is_complete());

        let methods = commands
            .lock()
            .iter()
            .filter_map(|command| command["method"].as_str().map(str::to_owned))
            .collect::<Vec<_>>();
        let release = methods
            .iter()
            .position(|method| method == "Runtime.releaseObjectGroup")
            .unwrap();
        let detach = methods
            .iter()
            .position(|method| method == "Target.detachFromTarget")
            .unwrap();
        assert!(release < detach);
    }

    #[tokio::test]
    async fn document_replacement_fails_closed_after_resolution() {
        let release = Arc::new(tokio::sync::Notify::new());
        let mut fixture = success_fixture();
        fixture.block_evaluation = Some(Arc::clone(&release));
        let (page, _, started) = page_for(fixture).await;
        let locator = page.locator("button");
        let resolving = tokio::spawn(async move { resolve(&locator).await });
        started.notified().await;
        page.lifecycle().commit_new_document();
        release.notify_one();
        let error = resolving.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("stale"));
    }

    #[tokio::test]
    async fn close_waits_for_an_admitted_resolution() {
        let release = Arc::new(tokio::sync::Notify::new());
        let mut fixture = success_fixture();
        fixture.block_evaluation = Some(Arc::clone(&release));
        let (page, _, started) = page_for(fixture).await;
        let locator = page.locator("button");
        let resolving = tokio::spawn(async move { resolve(&locator).await });
        started.notified().await;
        let closing_page = page.clone();
        let closing = tokio::spawn(async move { closing_page.close().await });
        tokio::task::yield_now().await;
        assert!(!closing.is_finished());
        release.notify_one();
        resolving.await.unwrap().unwrap();
        assert!(closing.await.unwrap().is_complete());
    }

    #[tokio::test]
    async fn close_waits_for_an_admitted_frame_locator_resolution() {
        let release = Arc::new(tokio::sync::Notify::new());
        let mut fixture = success_fixture();
        fixture.block_evaluation = Some(Arc::clone(&release));
        let (page, _, started) = page_for(fixture).await;
        let frame = page.main_frame().await.unwrap();
        let locator = frame.locator("button");
        let resolving = tokio::spawn(async move { resolve(&locator).await });
        started.notified().await;
        let closing_page = page.clone();
        let closing = tokio::spawn(async move { closing_page.close().await });
        tokio::task::yield_now().await;
        assert!(!closing.is_finished());
        release.notify_one();
        resolving.await.unwrap().unwrap();
        assert!(closing.await.unwrap().is_complete());
    }

    #[tokio::test]
    async fn resolution_is_rejected_after_close_starts() {
        let release = Arc::new(tokio::sync::Notify::new());
        let mut fixture = success_fixture();
        fixture.block_evaluation = Some(Arc::clone(&release));
        let (page, _, started) = page_for(fixture).await;
        let first = page.locator("button");
        let resolving = tokio::spawn(async move { resolve(&first).await });
        started.notified().await;
        let closing_page = page.clone();
        let closing = tokio::spawn(async move { closing_page.close().await });
        while let Ok(operation) = page.admit_operation("probe close state") {
            drop(operation);
            tokio::task::yield_now().await;
        }
        let error = resolve(&page.locator("button")).await.unwrap_err();
        assert_eq!(error.operation_name(), Some("resolve locator"));
        assert_eq!(error.phase(), crate::runtime::OperationPhase::Preparation);
        release.notify_one();
        resolving.await.unwrap().unwrap();
        assert!(closing.await.unwrap().is_complete());
    }

    pub(crate) async fn serve_live_locator_fixture(
        listener: tokio::net::TcpListener,
        root_body: String,
        frame_body: String,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let root_body = root_body.clone();
            let frame_body = frame_body.clone();
            tokio::spawn(async move {
                let mut request = [0_u8; 4096];
                let read = stream.read(&mut request).await.unwrap_or_default();
                let request = String::from_utf8_lossy(&request[..read]);
                let body = if request.starts_with("GET /same ") {
                    frame_body
                } else {
                    root_body
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    }

    #[tokio::test]
    #[ignore = "requires installed Chrome and loopback sockets"]
    async fn live_chrome_resolves_top_level_same_process_and_oopif_semantics() {
        use crate::runtime::{ActionCompletion, LaunchOptions};
        use std::time::Duration;

        let oopif_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind OOPIF origin");
        let oopif_port = oopif_listener.local_addr().unwrap().port();
        let parent_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind parent origin");
        let parent_port = parent_listener.local_addr().unwrap().port();

        let frame_body = |id: &str| {
            format!(
                r#"<!doctype html><style>button {{ width: 120px; height: 30px }}</style><button id="{id}">{id}</button>"#
            )
        };
        let parent_body = format!(
            r#"<!doctype html>
<style>
  button {{ width: 120px; height: 30px }}
  #overlay-wrap {{ position: relative; width: 120px; height: 30px }}
  #cover {{ position: absolute; inset: 0; background: black; z-index: 2 }}
  #moving {{ position: relative }}
</style>
<button id="text-only">Unique Text</button>
<button class="choice" id="choice-a">A</button>
<button class="choice" id="choice-b">B</button>
<section id="card"><button id="inside">Inside</button></section>
<button id="outside">Outside</button>
<div style="opacity:0"><button id="transparent">Transparent</button></div>
<button id="disabled" disabled>Disabled</button>
<div aria-disabled="true"><button id="aria-disabled">Aria disabled</button></div>
<button id="moving">Moving</button>
<script>
  const moving = document.getElementById('moving');
  let alternate = false;
  function moveEveryFrame() {{
    alternate = !alternate;
    moving.style.transform = `translateX(${{alternate ? 0 : 30}}px)`;
    requestAnimationFrame(moveEveryFrame);
  }}
  requestAnimationFrame(moveEveryFrame);
</script>
<div id="overlay-wrap"><button id="covered">Covered</button><div id="cover"></div></div>
<div id="open-host"></div><div id="closed-host"></div>
<script>
  const openRoot = document.getElementById('open-host').attachShadow({{mode:'open'}});
  openRoot.innerHTML = `<button id="shadow-button" data-testid="shadow-save">Shadow Save</button>
    <label>Shadow Email<input id="shadow-input" placeholder="Shadow Placeholder"></label>`;
  const shadowName = document.createElement('span'); shadowName.id = 'shadow-role-name'; shadowName.textContent = 'Shadow Named Action';
  const shadowLabelled = document.createElement('button'); shadowLabelled.id = 'shadow-labelled'; shadowLabelled.setAttribute('aria-labelledby', shadowName.id);
  openRoot.append(shadowName, shadowLabelled);
  const closedRoot = document.getElementById('closed-host').attachShadow({{mode:'closed'}});
  closedRoot.innerHTML = `<button id="closed-button">Closed</button>`;
</script>
<iframe src="/same"></iframe>
<iframe src="http://child.test:{oopif_port}/"></iframe>"#
        );
        let parent_server = tokio::spawn(serve_live_locator_fixture(
            parent_listener,
            parent_body,
            frame_body("same-button"),
        ));
        let oopif_server = tokio::spawn(serve_live_locator_fixture(
            oopif_listener,
            frame_body("oopif-button"),
            String::new(),
        ));

        let runtime = BrowserRuntime::launch(
            LaunchOptions::default()
                .headless(true)
                .arg("--site-per-process")
                .arg("--host-resolver-rules=MAP *.test 127.0.0.1"),
        )
        .await
        .expect("launch private Chrome");
        let session = runtime.default_session().await.expect("default session");
        let page = session
            .new_page(format!("http://parent.test:{parent_port}/"))
            .await
            .expect("open locator fixture");
        let main = page.main_frame().await.expect("main frame");
        let main_session_id = main.cdp_session().await.unwrap().id().to_owned();
        let routed_frames = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let frames = page.frames().await.unwrap();
                let mut routed_frames = Vec::new();
                for frame in frames {
                    if let Ok(route) = frame.cdp_session().await {
                        routed_frames.push((frame, route.id().to_owned()));
                    }
                }
                if routed_frames.len() >= 3
                    && routed_frames
                        .iter()
                        .any(|(_, route_id)| route_id != &main_session_id)
                {
                    break routed_frames;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("iframe routes did not appear");
        let same = routed_frames
            .iter()
            .find(|(frame, route_id)| frame.id() != main.id() && route_id == &main_session_id)
            .map(|(frame, _)| frame.clone())
            .expect("same-process iframe");
        let oopif = routed_frames
            .iter()
            .find(|(frame, route_id)| frame.id() != main.id() && route_id != &main_session_id)
            .map(|(frame, _)| frame.clone())
            .expect("OOPIF");

        let text =
            resolve(&page.locator(LocatorQuery::text(TextMatcher::exact("Unique Text", true))))
                .await
                .unwrap();
        assert_eq!(
            text.backend_node_id,
            resolve(&page.locator("#text-only"))
                .await
                .unwrap()
                .backend_node_id
        );
        let missing = resolve(&page.locator(".missing")).await.unwrap_err();
        assert_eq!(missing.locator_failure(), Some(&LocatorFailure::NotFound));
        let ambiguous = resolve(&page.locator(".choice")).await.unwrap_err();
        assert_eq!(
            ambiguous.locator_failure(),
            Some(&LocatorFailure::Ambiguous { match_count: 2 })
        );
        let choices = page.locator(".choice");
        let first = resolve(&choices.first()).await.unwrap().backend_node_id;
        let last = resolve(&choices.last()).await.unwrap().backend_node_id;
        assert_ne!(first, last);
        assert_eq!(
            resolve(&choices.nth(1)).await.unwrap().backend_node_id,
            last
        );
        let card = page.locator("#card");
        assert_eq!(
            resolve(&card.locator("button"))
                .await
                .unwrap()
                .backend_node_id,
            resolve(&page.locator("#inside"))
                .await
                .unwrap()
                .backend_node_id
        );
        let escaped = resolve(&card.locator(LocatorQuery::xpath("//button[@id='outside']")))
            .await
            .unwrap_err();
        assert_eq!(escaped.locator_failure(), Some(&LocatorFailure::NotFound));

        let shadow_button = resolve(&page.locator("#shadow-button")).await.unwrap();
        assert_eq!(
            shadow_button.backend_node_id,
            resolve(&page.locator("#open-host").locator("#shadow-button"))
                .await
                .unwrap()
                .backend_node_id
        );
        assert_eq!(
            shadow_button.backend_node_id,
            resolve(&page.locator(LocatorQuery::text(TextMatcher::exact("Shadow Save", true))))
                .await
                .unwrap()
                .backend_node_id
        );
        assert_eq!(
            shadow_button.backend_node_id,
            resolve(&page.locator(LocatorQuery::role(
                RoleQuery::new("button").with_name(TextMatcher::exact("Shadow Save", true))
            )))
            .await
            .unwrap()
            .backend_node_id
        );
        assert_eq!(
            shadow_button.backend_node_id,
            resolve(
                &page.locator(LocatorQuery::test_id(crate::runtime::TestIdQuery::new(
                    "shadow-save"
                )))
            )
            .await
            .unwrap()
            .backend_node_id
        );
        let shadow_input = resolve(&page.locator("#shadow-input")).await.unwrap();
        assert_eq!(
            resolve(&page.locator("#shadow-labelled"))
                .await
                .unwrap()
                .backend_node_id,
            resolve(
                &page.locator(LocatorQuery::role(
                    RoleQuery::new("button")
                        .with_name(TextMatcher::exact("Shadow Named Action", true,)),
                ))
            )
            .await
            .unwrap()
            .backend_node_id
        );
        assert_eq!(
            shadow_input.backend_node_id,
            resolve(&page.locator(LocatorQuery::label(TextMatcher::exact(
                "Shadow Email",
                true
            ))))
            .await
            .unwrap()
            .backend_node_id
        );
        assert_eq!(
            shadow_input.backend_node_id,
            resolve(&page.locator(LocatorQuery::placeholder(TextMatcher::exact(
                "Shadow Placeholder",
                true
            ))))
            .await
            .unwrap()
            .backend_node_id
        );
        assert_eq!(
            resolve(&page.locator(LocatorQuery::xpath("//*[@id='shadow-button']")))
                .await
                .unwrap_err()
                .locator_failure(),
            Some(&LocatorFailure::NotFound)
        );
        assert_eq!(
            resolve(&page.locator("#closed-button"))
                .await
                .unwrap_err()
                .locator_failure(),
            Some(&LocatorFailure::NotFound)
        );

        for (selector, failure) in [
            ("#transparent", LocatorFailure::NotVisible),
            ("#disabled", LocatorFailure::Disabled),
            ("#aria-disabled", LocatorFailure::Disabled),
            ("#moving", LocatorFailure::Unstable),
            ("#covered", LocatorFailure::Obscured),
        ] {
            let error = match resolve(&page.locator(selector)).await {
                Err(error) => error,
                Ok(summary) => panic!("{selector} unexpectedly actionable: {summary:?}"),
            };
            assert_eq!(error.locator_failure(), Some(&failure), "{selector}");
            assert_eq!(error.action_completed(), ActionCompletion::NotStarted);
        }

        let same_resolved = resolve(&same.locator("#same-button")).await.unwrap();
        assert_eq!(same_resolved.session_id, main_session_id);
        let oopif_resolved = resolve(&oopif.locator("#oopif-button")).await.unwrap();
        assert_ne!(oopif_resolved.session_id, main_session_id);

        let detached_frame = same.locator("#same-button");
        cdpkit::runtime::methods::Evaluate::new(
            "document.querySelector('iframe[src=\"/same\"]').remove()",
        )
        .send(page.cdp_session())
        .await
        .expect("detach same-process iframe");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if detached_frame.validate_scope().await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("frame detach invalidation did not arrive");
        let frame_error = resolve(&detached_frame).await.unwrap_err();
        assert!(
            frame_error.to_string().contains("stale")
                || frame_error.to_string().contains("detached"),
            "unexpected frame invalidation: {frame_error}"
        );

        let stale_document = page.locator("#text-only");
        cdpkit::page::methods::Navigate::new(format!(
            "http://parent.test:{parent_port}/?replacement"
        ))
        .send(page.cdp_session())
        .await
        .expect("replace the top-level document");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if stale_document.validate_scope().await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("document/frame invalidation did not arrive");
        let document_error = resolve(&stale_document).await.unwrap_err();
        assert!(document_error.to_string().contains("stale"));

        assert!(runtime.close().await.is_complete());
        parent_server.abort();
        oopif_server.abort();
    }
}
