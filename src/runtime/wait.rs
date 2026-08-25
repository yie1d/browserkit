use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use cdpkit::dom::methods::ResolveNode;
use cdpkit::page::methods::CreateIsolatedWorld;
use cdpkit::runtime::methods::{CallFunctionOn, Evaluate, ReleaseObjectGroup};
use serde_json::Value;
use tokio::time::{sleep, Instant};

use super::lifecycle::PendingOwnershipGuard;
use super::{
    ActionCompletion, BrowserError, CleanupFailure, Frame, Locator, LocatorFailure, OperationPhase,
    OwnershipCleanupError, Page, TextMatcher, WaitFailure,
};

static NEXT_WAIT_OBJECT_GROUP: AtomicU64 = AtomicU64::new(1);
static NEXT_DOM_OBSERVER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadState {
    DomContentLoaded,
    Load,
    NetworkIdle,
}

#[derive(Debug, Clone, Copy)]
pub struct WaitOptions {
    timeout: Duration,
    poll_interval: Duration,
    stability: Duration,
}

impl Default for WaitOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            poll_interval: Duration::from_millis(50),
            stability: Duration::from_millis(200),
        }
    }
}

impl WaitOptions {
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
    pub fn poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }
    pub fn stability(mut self, duration: Duration) -> Self {
        self.stability = duration;
        self
    }
    pub fn timeout_value(&self) -> Duration {
        self.timeout
    }
    pub fn poll_interval_value(&self) -> Duration {
        self.poll_interval
    }
    pub fn stability_value(&self) -> Duration {
        self.stability
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocatorCondition {
    Attached,
    Detached,
    Visible,
    Hidden,
    Enabled,
    Disabled,
    Editable,
    Text(TextMatcher),
    Attribute {
        name: String,
        value: Option<TextMatcher>,
    },
    Count(usize),
}

pub(crate) async fn wait_load_state(
    page: &Page,
    state: LoadState,
    options: WaitOptions,
) -> Result<(), BrowserError> {
    let _operation = page.admit_operation("wait for load state")?;
    wait_load_state_admitted(page, state, options, &_operation, Instant::now()).await
}

pub(super) async fn wait_load_state_admitted(
    page: &Page,
    state: LoadState,
    options: WaitOptions,
    _operation: &super::page::PageOperation,
    started: Instant,
) -> Result<(), BrowserError> {
    if state == LoadState::NetworkIdle {
        let elapsed = started.elapsed();
        let timeout = options.timeout.saturating_sub(elapsed);
        return super::network::wait_idle(
            page,
            super::NetworkIdleOptions::default()
                .timeout(timeout)
                .quiet_window(options.stability),
        )
        .await;
    }
    let condition = format!("load state {state:?}");
    wait_page_value(
        page,
        &condition,
        options,
        started,
        move |ready| match state {
            LoadState::DomContentLoaded => matches!(ready, "interactive" | "complete"),
            LoadState::Load => ready == "complete",
            LoadState::NetworkIdle => false,
        },
        "document.readyState",
    )
    .await
}

pub(crate) async fn wait_url(
    page: &Page,
    matcher: TextMatcher,
    options: WaitOptions,
) -> Result<(), BrowserError> {
    let condition = format!("URL {}", matcher_description(&matcher));
    let matcher = PreparedMatcher::new(matcher)?;
    let _operation = page.admit_operation("wait for URL")?;
    wait_page_value(
        page,
        &condition,
        options,
        Instant::now(),
        move |value| matcher.is_match(value),
        "location.href",
    )
    .await
}

pub(crate) async fn wait_title(
    page: &Page,
    matcher: TextMatcher,
    options: WaitOptions,
) -> Result<(), BrowserError> {
    let condition = format!("title {}", matcher_description(&matcher));
    let matcher = PreparedMatcher::new(matcher)?;
    let _operation = page.admit_operation("wait for title")?;
    wait_page_value(
        page,
        &condition,
        options,
        Instant::now(),
        move |value| matcher.is_match(value),
        "document.title",
    )
    .await
}

async fn wait_page_value(
    page: &Page,
    condition: &str,
    options: WaitOptions,
    started: Instant,
    predicate: impl Fn(&str) -> bool,
    expression: &str,
) -> Result<(), BrowserError> {
    let mut last = None;
    loop {
        let remaining = options.timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(wait_timeout(
                condition,
                format!("page:{}", page.target_id()),
                started.elapsed(),
                last,
            ));
        }
        let response = tokio::time::timeout(
            remaining,
            Evaluate::new(expression)
                .with_return_by_value(true)
                .send(page.cdp_session()),
        )
        .await
        .map_err(|_| {
            wait_timeout(
                condition,
                format!("page:{}", page.target_id()),
                started.elapsed(),
                last.clone(),
            )
        })?
        .map_err(|error| {
            BrowserError::cdp_operation(
                "observe page wait condition",
                OperationPhase::Observation,
                error,
            )
        })?;
        if let Some(exception) = response.exception_details {
            return Err(BrowserError::operation(
                "observe page wait condition",
                OperationPhase::Observation,
            )
            .with_message(format!("wait observation failed: {}", exception.text)));
        }
        let value = response
            .result
            .value
            .as_ref()
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        last = Some(value.clone());
        if predicate(&value) {
            return Ok(());
        }
        if started.elapsed() >= options.timeout {
            return Err(wait_timeout(
                condition,
                format!("page:{}", page.target_id()),
                started.elapsed(),
                Some(value),
            ));
        }
        sleep(
            options
                .poll_interval
                .min(options.timeout.saturating_sub(started.elapsed())),
        )
        .await;
    }
}

pub(crate) async fn wait_locator(
    locator: &Locator,
    condition: LocatorCondition,
    options: WaitOptions,
) -> Result<(), BrowserError> {
    let prepared_matcher = condition_matcher(&condition)
        .cloned()
        .map(PreparedMatcher::new)
        .transpose()?;
    let page = locator.page_for_action().clone();
    let operation = page.admit_operation("wait for locator")?;
    let started = Instant::now();
    let description = format!("locator {condition:?}");
    loop {
        let observation =
            observe_locator(locator, &operation, &condition, prepared_matcher.as_ref()).await;
        let last = match observation {
            Ok((satisfied, facts)) => {
                if satisfied {
                    return Ok(());
                }
                Some(facts)
            }
            Err(error) if missing_satisfies(&condition, &error) => return Ok(()),
            Err(error) if retryable_locator_error(&error) => Some(error.to_string()),
            Err(error) => return Err(error),
        };
        if started.elapsed() >= options.timeout {
            return Err(wait_timeout(
                &description,
                format!("page:{}", page.target_id()),
                started.elapsed(),
                last,
            ));
        }
        sleep(options.poll_interval).await;
    }
}

async fn observe_locator(
    locator: &Locator,
    operation: &super::page::PageOperation,
    condition: &LocatorCondition,
    prepared_matcher: Option<&PreparedMatcher>,
) -> Result<(bool, String), BrowserError> {
    if let LocatorCondition::Count(expected) = condition {
        let count = locator.count_admitted(operation).await?;
        return Ok((*expected == count, format!("count={count}")));
    }
    let resolved = locator.resolve_admitted(operation).await?;
    let facts = resolved.facts;
    let basic = match condition {
        LocatorCondition::Attached => Some(facts.attached),
        LocatorCondition::Detached => Some(!facts.attached),
        LocatorCondition::Visible => Some(facts.visible),
        LocatorCondition::Hidden => Some(!facts.visible),
        LocatorCondition::Enabled => Some(facts.enabled),
        LocatorCondition::Disabled => Some(!facts.enabled),
        LocatorCondition::Editable => Some(facts.editable),
        LocatorCondition::Count(_) => unreachable!("count has a dedicated final-query path"),
        LocatorCondition::Text(_) | LocatorCondition::Attribute { .. } => None,
    };
    if let Some(satisfied) = basic {
        return Ok((
            satisfied,
            format!(
                "attached={}, visible={}, enabled={}, editable={}",
                facts.attached, facts.visible, facts.enabled, facts.editable
            ),
        ));
    }

    let sequence = NEXT_WAIT_OBJECT_GROUP.fetch_add(1, Ordering::Relaxed);
    let object_group = format!(
        "browserkit-wait-{}-{sequence}",
        locator.page_for_action().target_id()
    );
    let release_session = resolved.session.clone();
    let release_group = object_group.clone();
    let cleanup =
        locator
            .page_for_action()
            .track_locator_cleanup(object_group.clone(), move || async move {
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
        let remote = ResolveNode::new()
            .with_backend_node_id(resolved.backend_node_id)
            .with_object_group(object_group.clone())
            .send(&resolved.session)
            .await
            .map_err(|error| {
                BrowserError::cdp_operation(
                    "resolve locator wait value",
                    OperationPhase::Observation,
                    error,
                )
            })?
            .object;
        let object_id = remote.object_id.ok_or_else(|| {
            BrowserError::operation("resolve locator wait value", OperationPhase::Observation)
                .with_message("resolved locator has no remote object")
        })?;
        let expression = match condition {
            LocatorCondition::Text(_) => "function(){ return this.textContent || ''; }",
            LocatorCondition::Attribute { name, .. } => {
                return observe_attribute(
                    &resolved.session,
                    &object_id,
                    name,
                    condition,
                    prepared_matcher,
                )
                .await;
            }
            _ => unreachable!(),
        };
        let response = CallFunctionOn::new(expression)
            .with_object_id(object_id.clone())
            .with_return_by_value(true)
            .send(&resolved.session)
            .await
            .map_err(|error| {
                BrowserError::cdp_operation(
                    "read locator wait value",
                    OperationPhase::Observation,
                    error,
                )
            });
        let response = response?;
        let value = response
            .result
            .value
            .as_ref()
            .and_then(Value::as_str)
            .unwrap_or_default();
        let satisfied = match condition {
            LocatorCondition::Text(_) => prepared_matcher
                .expect("text matcher prepared")
                .is_match(value),
            _ => false,
        };
        Ok((satisfied, format!("text={value:?}")))
    }
    .await;
    merge_wait_cleanup(primary, cleanup.cleanup().await, object_group)
}

async fn observe_attribute(
    session: &cdpkit::Session,
    object_id: &str,
    name: &str,
    condition: &LocatorCondition,
    prepared_matcher: Option<&PreparedMatcher>,
) -> Result<(bool, String), BrowserError> {
    let function = format!(
        "function(){{ return this.getAttribute({}); }}",
        serde_json::to_string(name).expect("string serialization")
    );
    let response = CallFunctionOn::new(function)
        .with_object_id(object_id.to_owned())
        .with_return_by_value(true)
        .send(session)
        .await
        .map_err(|error| {
            BrowserError::cdp_operation(
                "read locator attribute",
                OperationPhase::Observation,
                error,
            )
        });
    let response = response?;
    let value = response.result.value.as_ref().and_then(Value::as_str);
    let satisfied = match condition {
        LocatorCondition::Attribute { value: None, .. } => value.is_some(),
        LocatorCondition::Attribute { value: Some(_), .. } => value.is_some_and(|value| {
            prepared_matcher
                .expect("attribute matcher prepared")
                .is_match(value)
        }),
        _ => false,
    };
    Ok((satisfied, format!("attribute={value:?}")))
}

fn missing_satisfies(condition: &LocatorCondition, error: &BrowserError) -> bool {
    error.cleanup_failures().is_empty()
        && matches!(error.locator_failure(), Some(LocatorFailure::NotFound))
        && matches!(
            condition,
            LocatorCondition::Detached | LocatorCondition::Hidden | LocatorCondition::Count(0)
        )
}

fn retryable_locator_error(error: &BrowserError) -> bool {
    error.cleanup_failures().is_empty()
        && matches!(
            error.locator_failure(),
            Some(LocatorFailure::NotFound | LocatorFailure::Ambiguous { .. })
        )
}

fn condition_matcher(condition: &LocatorCondition) -> Option<&TextMatcher> {
    match condition {
        LocatorCondition::Text(matcher) => Some(matcher),
        LocatorCondition::Attribute {
            value: Some(matcher),
            ..
        } => Some(matcher),
        _ => None,
    }
}

fn merge_wait_cleanup<T>(
    primary: Result<T, BrowserError>,
    cleanup: Result<(), OwnershipCleanupError>,
    object_group: String,
) -> Result<T, BrowserError> {
    match (primary, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(error)) => Err(BrowserError::operation(
            "release locator wait object group",
            OperationPhase::Cleanup,
        )
        .with_message(format!(
            "failed to release locator wait object group: {error}"
        ))
        .with_cleanup_failure(CleanupFailure::new(object_group, error.to_string()))),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup_error)) => Err(error
            .with_cleanup_failure(CleanupFailure::new(object_group, cleanup_error.to_string()))),
    }
}

async fn merge_wait_cleanup_before_deadline<T>(
    primary: Result<T, BrowserError>,
    cleanup: PendingOwnershipGuard,
    resource: String,
    started: Instant,
    timeout: Duration,
    timeout_error: impl Fn() -> BrowserError,
) -> Result<T, BrowserError> {
    let remaining = timeout.saturating_sub(started.elapsed());
    if remaining.is_zero() {
        drop(cleanup);
        return match primary {
            Ok(_) => Err(timeout_error()),
            Err(error) => Err(error),
        };
    }
    match tokio::time::timeout(remaining, cleanup.cleanup()).await {
        Ok(cleanup) => merge_wait_cleanup(primary, cleanup, resource),
        Err(_) => match primary {
            Ok(_) => Err(timeout_error()),
            Err(error) => Err(error),
        },
    }
}

pub(crate) async fn wait_frame_stability(
    frame: &Frame,
    options: WaitOptions,
) -> Result<(), BrowserError> {
    let operation = frame.page().admit_operation("wait for DOM stability")?;
    wait_frame_stability_admitted(frame, options, &operation).await
}

async fn wait_frame_stability_admitted(
    frame: &Frame,
    options: WaitOptions,
    operation: &super::page::PageOperation,
) -> Result<(), BrowserError> {
    let started = Instant::now();
    let timeout_error = || {
        wait_timeout(
            "DOM stability",
            format!("frame:{}", frame.id()),
            started.elapsed(),
            Some("DOM stability observation did not complete".into()),
        )
    };
    let store = tokio::time::timeout(options.timeout, frame.page().locator_frame_store(operation))
        .await
        .map_err(|_| timeout_error())??;
    let route = store.locator_route(frame)?;
    let remaining = options.timeout.saturating_sub(started.elapsed());
    let world = tokio::time::timeout(
        remaining,
        CreateIsolatedWorld::new(route.frame_id.as_str().to_owned())
            .with_world_name("browserkit-dom-stability")
            .with_grant_univeral_access(false)
            .send(&route.session),
    )
    .await
    .map_err(|_| timeout_error())?
    .map_err(|error| {
        BrowserError::cdp_operation(
            "prepare DOM stability observer",
            OperationPhase::Observation,
            error,
        )
    })?;
    store.validate_locator_route(&route)?;
    let sequence = NEXT_DOM_OBSERVER.fetch_add(1, Ordering::Relaxed);
    let key = format!("__browserkitDomStability{sequence}");
    let encoded_key = serde_json::to_string(&key).expect("observer key serialization");
    let install = DOM_STABILITY_INSTALL.replace("__KEY__", &encoded_key);
    let observe = format!("globalThis[{encoded_key}]?.scan()");
    let cleanup_expression = format!("(() => {{ const key = {encoded_key}; const state = globalThis[key]; if (state) state.observer.disconnect(); delete globalThis[key]; }})()");
    let cleanup_session = route.session.clone();
    let cleanup_context = world.execution_context_id;
    let cleanup_resource = format!("dom-stability:{}:{sequence}", route.frame_id);
    let cleanup =
        frame
            .page()
            .track_locator_cleanup(cleanup_resource.clone(), move || async move {
                Evaluate::new(cleanup_expression)
                    .with_context_id(cleanup_context)
                    .with_return_by_value(true)
                    .send(&cleanup_session)
                    .await
                    .map_err(OwnershipCleanupError::from)
                    .and_then(|response| {
                        response.exception_details.map_or(Ok(()), |exception| {
                            Err(OwnershipCleanupError::Other(format!(
                                "DOM stability observer cleanup failed: {exception:?}"
                            )))
                        })
                    })
            });
    let primary = async {
        let remaining = options.timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(timeout_error());
        }
        let installed = tokio::time::timeout(
            remaining,
            Evaluate::new(install)
                .with_context_id(world.execution_context_id)
                .with_return_by_value(true)
                .send(&route.session),
        )
        .await
        .map_err(|_| timeout_error())?
        .map_err(|error| {
            BrowserError::cdp_operation(
                "install DOM stability observer",
                OperationPhase::Observation,
                error,
            )
        })?;
        if let Some(exception) = installed.exception_details {
            return Err(BrowserError::operation(
                "install DOM stability observer",
                OperationPhase::Observation,
            )
            .with_message(format!(
                "DOM stability observer installation failed: {exception:?}"
            )));
        }
        let mut stable_since = None;
        let mut previous: Option<u64> = None;
        loop {
            store.validate_locator_route(&route)?;
            let remaining = options.timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(timeout_error());
            }
            let response = tokio::time::timeout(
                remaining,
                Evaluate::new(observe.clone())
                    .with_context_id(world.execution_context_id)
                    .with_return_by_value(true)
                    .send(&route.session),
            )
            .await
            .map_err(|_| timeout_error())?
            .map_err(|error| {
                BrowserError::cdp_operation(
                    "observe DOM stability",
                    OperationPhase::Observation,
                    error,
                )
            })?;
            if let Some(exception) = response.exception_details {
                return Err(BrowserError::operation(
                    "observe DOM stability",
                    OperationPhase::Observation,
                )
                .with_message(format!("DOM stability observation failed: {exception:?}")));
            }
            let current = response
                .result
                .value
                .as_ref()
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    BrowserError::operation("observe DOM stability", OperationPhase::Observation)
                        .with_message("DOM stability observer became unavailable")
                })?;
            if previous == Some(current) {
                stable_since.get_or_insert_with(Instant::now);
            } else {
                previous = Some(current);
                stable_since = None;
            }
            if stable_since.is_some_and(|quiet| quiet.elapsed() >= options.stability) {
                return Ok(());
            }
            if started.elapsed() >= options.timeout {
                return Err(wait_timeout(
                    "DOM stability",
                    format!("frame:{}", frame.id()),
                    started.elapsed(),
                    Some("DOM continued changing".into()),
                ));
            }
            sleep(
                options
                    .poll_interval
                    .min(options.timeout.saturating_sub(started.elapsed())),
            )
            .await;
        }
    }
    .await;
    merge_wait_cleanup_before_deadline(
        primary,
        cleanup,
        cleanup_resource,
        started,
        options.timeout,
        timeout_error,
    )
    .await
}

const DOM_STABILITY_INSTALL: &str = r#"(() => {
  const key = __KEY__;
  const state = { generation: 0, observer: null, scan: null };
  const observed = new WeakSet();
  const roots = [];
  const observer = new MutationObserver(records => {
    state.generation += 1;
    for (const record of records) for (const node of record.addedNodes) observeTree(node);
  });
  const observeTree = root => {
    if (!root || observed.has(root)) return;
    observed.add(root);
    roots.push(root);
    state.generation += 1;
    observer.observe(root, { subtree: true, childList: true, attributes: true, characterData: true });
    const elements = root.querySelectorAll ? [root, ...root.querySelectorAll('*')] : [];
    for (const element of elements) if (element.shadowRoot) observeTree(element.shadowRoot);
  };
  state.observer = observer;
  state.scan = () => {
    observeTree(document);
    for (let index = 0; index < roots.length; index += 1) {
      const root = roots[index];
      if (!root.querySelectorAll) continue;
      for (const element of root.querySelectorAll('*')) if (element.shadowRoot) observeTree(element.shadowRoot);
    }
    return state.generation;
  };
  state.scan();
  globalThis[key] = state;
  return state.generation;
})()"#;

pub(crate) async fn wait_page_stability(
    page: &Page,
    options: WaitOptions,
) -> Result<(), BrowserError> {
    let operation = page.admit_operation("wait for DOM stability")?;
    let store = page.locator_frame_store(&operation).await?;
    let id = store.main_frame_id().ok_or_else(|| {
        BrowserError::operation("wait for DOM stability", OperationPhase::Preparation)
            .with_message("page has no main frame")
    })?;
    let frame = store.handle(&id).ok_or_else(|| {
        BrowserError::operation("wait for DOM stability", OperationPhase::Preparation)
            .with_message("page main frame disappeared")
    })?;
    wait_frame_stability_admitted(&frame, options, &operation).await
}

fn wait_timeout(
    condition: &str,
    scope: String,
    elapsed: Duration,
    last: Option<String>,
) -> BrowserError {
    BrowserError::operation("wait for condition", OperationPhase::Confirmation)
        .with_action_completion(ActionCompletion::NotStarted)
        .with_message(format!(
            "timed out waiting for {condition} in {scope} after {elapsed:?}"
        ))
        .with_wait_failure(WaitFailure::new(condition, scope, elapsed, last))
}

#[cfg(test)]
pub(crate) fn text_matches(matcher: &TextMatcher, actual: &str) -> bool {
    PreparedMatcher::new(matcher.clone()).is_ok_and(|matcher| matcher.is_match(actual))
}

#[derive(Debug)]
enum PreparedMatcher {
    Exact { value: String, case_sensitive: bool },
    Contains { value: String, case_sensitive: bool },
    Regex(regex::Regex),
}

impl PreparedMatcher {
    fn new(matcher: TextMatcher) -> Result<Self, BrowserError> {
        match matcher {
            TextMatcher::Exact {
                value,
                case_sensitive,
            } => Ok(Self::Exact {
                value,
                case_sensitive,
            }),
            TextMatcher::Contains {
                value,
                case_sensitive,
            } => Ok(Self::Contains {
                value,
                case_sensitive,
            }),
            TextMatcher::Regex {
                pattern,
                case_sensitive,
            } => {
                let source = if case_sensitive {
                    pattern
                } else {
                    format!("(?i:{pattern})")
                };
                regex::Regex::new(&source)
                    .map(Self::Regex)
                    .map_err(|error| {
                        BrowserError::operation("prepare text matcher", OperationPhase::Preparation)
                            .with_message(format!("invalid regular expression: {error}"))
                    })
            }
        }
    }

    fn is_match(&self, actual: &str) -> bool {
        match self {
            Self::Exact {
                value,
                case_sensitive,
            } => compare(value, actual, *case_sensitive, |a, b| a == b),
            Self::Contains {
                value,
                case_sensitive,
            } => compare(value, actual, *case_sensitive, |needle, haystack| {
                haystack.contains(needle)
            }),
            Self::Regex(regex) => regex.is_match(actual),
        }
    }
}

fn compare(
    expected: &str,
    actual: &str,
    case_sensitive: bool,
    predicate: impl Fn(&str, &str) -> bool,
) -> bool {
    if case_sensitive {
        predicate(expected, actual)
    } else {
        predicate(&expected.to_lowercase(), &actual.to_lowercase())
    }
}

fn matcher_description(matcher: &TextMatcher) -> String {
    format!("matches {matcher:?}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::lifecycle::PendingOwnershipRegistry;

    #[test]
    fn text_matchers_cover_exact_contains_and_regex_shape() {
        assert!(text_matches(&TextMatcher::exact("Ready", true), "Ready"));
        assert!(text_matches(
            &TextMatcher::contains("orders", false),
            "My Orders"
        ));
        assert!(text_matches(
            &TextMatcher::regex("^ord.*ready$", false),
            "Orders Ready"
        ));
        assert!(text_matches(
            &TextMatcher::regex(r"^ord(er|ers)\s+ready$", false),
            "Orders Ready",
        ));
    }

    #[test]
    fn invalid_regex_is_rejected_before_polling() {
        let error = PreparedMatcher::new(TextMatcher::regex("[", true)).unwrap_err();
        assert_eq!(error.phase(), OperationPhase::Preparation);
        assert!(error.to_string().contains("invalid regular expression"));
    }

    #[test]
    fn wait_cleanup_failure_is_never_silently_discarded() {
        let cleanup = OwnershipCleanupError::from(cdpkit::CdpError::Protocol {
            code: -32_000,
            message: "release failed".into(),
            data: None,
        });
        let error = merge_wait_cleanup::<()>(Ok(()), Err(cleanup), "group-1".into()).unwrap_err();
        assert_eq!(error.phase(), OperationPhase::Cleanup);
        assert_eq!(error.cleanup_failures().len(), 1);
    }

    #[test]
    fn locator_failure_with_cleanup_failure_is_not_success_or_retryable() {
        let error = BrowserError::operation("resolve locator", OperationPhase::Observation)
            .with_locator_failure(LocatorFailure::NotFound)
            .with_cleanup_failure(CleanupFailure::new("group", "release failed"));
        assert!(!missing_satisfies(&LocatorCondition::Hidden, &error));
        assert!(!retryable_locator_error(&error));
    }

    #[test]
    fn dom_stability_script_rescans_for_late_open_shadow_roots() {
        assert!(DOM_STABILITY_INSTALL.contains("root.querySelectorAll('*')"));
        assert!(DOM_STABILITY_INSTALL.contains("state.generation += 1"));
        assert!(DOM_STABILITY_INSTALL.contains("state.observer = observer"));
    }

    #[tokio::test]
    async fn stalled_cleanup_cannot_extend_the_wait_deadline() {
        let registry = PendingOwnershipRegistry::new();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let guard = registry.register("dom-observer", move || async move {
            let _ = entered_tx.send(());
            let _ = release_rx.await;
            Ok(())
        });
        let started = Instant::now();
        let task = tokio::spawn(async move {
            merge_wait_cleanup_before_deadline(
                Err::<(), _>(wait_timeout(
                    "DOM stability",
                    "frame:main".into(),
                    Duration::from_millis(1),
                    Some("DOM continued changing".into()),
                )),
                guard,
                "dom-observer".into(),
                started,
                Duration::from_millis(20),
                || {
                    wait_timeout(
                        "DOM stability",
                        "frame:main".into(),
                        started.elapsed(),
                        Some("observer cleanup exceeded the wait deadline".into()),
                    )
                },
            )
            .await
        });
        entered_rx.await.expect("cleanup started");
        let error = tokio::time::timeout(Duration::from_millis(100), task)
            .await
            .expect("wait returned within its deadline")
            .expect("wait task completed")
            .unwrap_err();
        assert_eq!(error.phase(), OperationPhase::Confirmation);
        assert_eq!(
            error.wait_failure().unwrap().last_observation(),
            Some("DOM continued changing")
        );

        release_tx.send(()).expect("release managed cleanup");
        let outcomes = tokio::time::timeout(Duration::from_millis(100), registry.cleanup_all())
            .await
            .expect("managed cleanup completed");
        assert_eq!(outcomes, vec![("dom-observer".into(), Ok(()))]);
    }

    #[tokio::test]
    async fn successful_wait_at_zero_remaining_budget_times_out_and_cleans_once() {
        let registry = PendingOwnershipRegistry::new();
        let cleanup_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let cleanup_counter = cleanup_count.clone();
        let guard = registry.register("dom-observer-zero", move || async move {
            cleanup_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        });
        let started = Instant::now();
        let error = merge_wait_cleanup_before_deadline(
            Ok(()),
            guard,
            "dom-observer-zero".into(),
            started,
            Duration::ZERO,
            || {
                wait_timeout(
                    "DOM stability",
                    "frame:main".into(),
                    started.elapsed(),
                    Some("observer cleanup exceeded the wait deadline".into()),
                )
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error.phase(), OperationPhase::Confirmation);
        assert_eq!(
            error.wait_failure().unwrap().last_observation(),
            Some("observer cleanup exceeded the wait deadline")
        );

        let outcomes = tokio::time::timeout(Duration::from_millis(100), registry.cleanup_all())
            .await
            .expect("scheduled cleanup completed");
        assert_eq!(outcomes, vec![("dom-observer-zero".into(), Ok(()))]);
        assert_eq!(cleanup_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
