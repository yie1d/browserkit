mod accessibility;
mod dom;

use std::sync::atomic::{AtomicU64, Ordering};

use cdpkit::accessibility::methods::GetPartialAxTree;
use cdpkit::dom::methods::DescribeNode;
use cdpkit::dom::methods::ResolveNode;
use cdpkit::page::methods::CreateIsolatedWorld;
use cdpkit::runtime::methods::{CallFunctionOn, Evaluate, GetProperties, ReleaseObjectGroup};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{BrowserError, Frame, Locator, OperationPhase, Page};
pub use accessibility::{AccessibilityAvailability, AccessibilityFacts, AccessibilityState};

const SNAPSHOT_WORLD: &str = "browserkit-snapshot";
static NEXT_SNAPSHOT_GROUP: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotOptions {
    max_bytes: usize,
    max_elements: usize,
}

impl Default for SnapshotOptions {
    fn default() -> Self {
        Self {
            max_bytes: 256 * 1024,
            max_elements: 200,
        }
    }
}

impl SnapshotOptions {
    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }
    pub fn with_max_elements(mut self, max_elements: usize) -> Self {
        self.max_elements = max_elements;
        self
    }
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }
    pub fn max_elements(&self) -> usize {
        self.max_elements
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotTruncation {
    pub max_bytes: usize,
    pub max_elements: usize,
    pub emitted_bytes: usize,
    pub emitted_elements: usize,
    pub omitted_bytes: usize,
    pub omitted_elements: usize,
    #[serde(default)]
    pub omitted_frames: usize,
    #[serde(default)]
    pub unavailable_accessibility: usize,
}

impl SnapshotTruncation {
    pub fn is_truncated(&self) -> bool {
        self.omitted_bytes > 0 || self.omitted_elements > 0 || self.omitted_frames > 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ElementBounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElementSnapshot {
    pub tag_name: String,
    pub id: Option<String>,
    pub test_id: Option<String>,
    pub text: String,
    pub bounds: ElementBounds,
    pub accessibility: AccessibilityFacts,
    pub focused: bool,
    #[serde(default)]
    pub descendants: Vec<ElementSnapshot>,
    #[serde(default)]
    pub truncation: SnapshotTruncation,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewportSnapshot {
    pub width: f64,
    pub height: f64,
    pub scroll_x: f64,
    pub scroll_y: f64,
    pub document_width: f64,
    pub document_height: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DocumentLoadState {
    Loading,
    Interactive,
    Complete,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameSnapshotView {
    pub id: String,
    pub parent_id: Option<String>,
    pub child_ids: Vec<String>,
    pub url: String,
    pub title: String,
    pub load_state: DocumentLoadState,
    pub visible_text: String,
    pub elements: Vec<ElementSnapshot>,
    /// Index of the focused element in `elements`, when focus is represented.
    pub focus: Option<usize>,
    pub viewport: ViewportSnapshot,
    pub truncation: SnapshotTruncation,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PageSnapshot {
    pub main_frame_id: String,
    pub url: String,
    pub title: String,
    pub load_state: DocumentLoadState,
    pub visible_text: String,
    pub elements: Vec<ElementSnapshot>,
    /// Index of the focused element in `elements`, when focus is represented.
    pub focus: Option<usize>,
    pub viewport: ViewportSnapshot,
    pub frames: Vec<FrameSnapshotView>,
    pub truncation: SnapshotTruncation,
}

pub(super) async fn capture_page(
    page: &Page,
    options: SnapshotOptions,
) -> Result<PageSnapshot, BrowserError> {
    let mut metrics = PageCaptureMetrics::default();
    capture_page_impl(page, options, &mut metrics).await
}

#[derive(Default)]
struct PageCaptureMetrics {
    #[allow(dead_code)]
    indexed_frame_nodes: usize,
}

async fn capture_page_impl(
    page: &Page,
    options: SnapshotOptions,
    metrics: &mut PageCaptureMetrics,
) -> Result<PageSnapshot, BrowserError> {
    let _ = &metrics;
    validate_options(options)?;
    let operation = page.admit_operation("capture page snapshot")?;
    let store = page.locator_frame_store(&operation).await?;
    let main_id = store.main_frame_id().ok_or_else(|| {
        BrowserError::operation("capture page snapshot", OperationPhase::Preparation)
            .with_message("page has no main frame")
    })?;
    let mut frame_ids = store.frame_ids();
    frame_ids.sort();
    let initial_frame_ids = frame_ids.clone();
    frame_ids.sort_by_key(|id| id.as_str() != main_id);
    let mut initial_frames = Vec::with_capacity(frame_ids.len());
    for id in frame_ids {
        let frame = store.handle(&id).ok_or_else(|| {
            BrowserError::operation("capture page snapshot", OperationPhase::Confirmation)
                .with_message(format!("frame {id} disappeared"))
        })?;
        let route = store.locator_route(&frame)?;
        initial_frames.push((frame, route));
    }
    let mut remaining_bytes = options.max_bytes;
    let mut remaining_elements = options.max_elements;
    let mut captured_frames = Vec::with_capacity(initial_frames.len());
    let frame_count = initial_frames.len();
    for (frame_index, (_, route)) in initial_frames.iter().enumerate() {
        let frames_remaining = frame_count - frame_index;
        let frame_byte_limit = remaining_bytes / frames_remaining;
        let frame_element_limit = if remaining_bytes == 0 {
            0
        } else {
            remaining_elements
        };
        let payload = capture_frame_admitted(
            page,
            store,
            route,
            SnapshotOptions {
                max_bytes: frame_byte_limit,
                max_elements: frame_element_limit,
            },
            false,
        )
        .await?;
        remaining_bytes = remaining_bytes.saturating_sub(payload.truncation.emitted_bytes);
        remaining_elements = remaining_elements.saturating_sub(payload.truncation.emitted_elements);
        captured_frames.push((payload, frame_element_limit));
    }
    let route_refs = initial_frames
        .iter()
        .map(|(_, route)| route)
        .collect::<Vec<_>>();
    let mut authoritative = store
        .validate_locator_routes_authoritative(&route_refs)
        .await?;
    #[cfg(test)]
    {
        metrics.indexed_frame_nodes = authoritative.indexed_frame_nodes;
    }
    let expected_frame_ids = initial_frame_ids.iter().cloned().collect();
    if authoritative.frame_ids != expected_frame_ids {
        return Err(
            BrowserError::operation("capture page snapshot", OperationPhase::Confirmation)
                .with_message("page frame set became stale during snapshot"),
        );
    }
    let mut frames = Vec::with_capacity(initial_frames.len());
    for ((frame, _), (payload, frame_element_limit)) in initial_frames.iter().zip(captured_frames) {
        let identity = authoritative
            .identities
            .remove(frame.id().as_str())
            .ok_or_else(|| {
                BrowserError::operation("capture page snapshot", OperationPhase::Confirmation)
                    .with_message(format!("frame {} is stale: document is absent", frame.id()))
            })?;
        let view = finalize_frame_snapshot(
            FrameSnapshotView {
                id: frame.id().as_str().to_owned(),
                parent_id: identity.parent_id,
                child_ids: identity.child_ids,
                url: payload.url,
                title: payload.title,
                load_state: payload.load_state,
                visible_text: payload.visible_text,
                elements: payload.elements,
                focus: payload.focus,
                viewport: payload.viewport,
                truncation: payload.truncation,
            },
            SnapshotOptions {
                max_bytes: options.max_bytes,
                max_elements: frame_element_limit,
            },
        )?;
        frames.push(view);
    }
    let truncation = combine_truncation(options, &frames);
    let main_index = frames
        .iter()
        .position(|frame| frame.id == main_id)
        .ok_or_else(|| {
            BrowserError::operation("capture page snapshot", OperationPhase::Confirmation)
                .with_message("main frame disappeared during snapshot")
        })?;
    let mut final_frame_ids = store.frame_ids();
    final_frame_ids.sort();
    if final_frame_ids != initial_frame_ids {
        return Err(
            BrowserError::operation("capture page snapshot", OperationPhase::Confirmation)
                .with_message("page frame set became stale during snapshot"),
        );
    }
    let main = frames.remove(main_index);
    finalize_page_snapshot(
        PageSnapshot {
            main_frame_id: main.id,
            url: main.url,
            title: main.title,
            load_state: main.load_state,
            visible_text: main.visible_text,
            elements: main.elements,
            focus: main.focus,
            viewport: main.viewport,
            frames,
            truncation,
        },
        options,
    )
}

pub(super) async fn capture_frame(
    frame: &Frame,
    options: SnapshotOptions,
) -> Result<FrameSnapshotView, BrowserError> {
    validate_options(options)?;
    let page = frame.page();
    let operation = page.admit_operation("capture frame snapshot")?;
    let store = page.locator_frame_store(&operation).await?;
    let route = store.locator_route(frame)?;
    let payload = capture_frame_admitted(page, store, &route, options, true).await?;
    finalize_frame_snapshot(
        FrameSnapshotView {
            id: frame.id().as_str().to_owned(),
            parent_id: payload.parent_id,
            child_ids: payload.child_ids,
            url: payload.url,
            title: payload.title,
            load_state: payload.load_state,
            visible_text: payload.visible_text,
            elements: payload.elements,
            focus: payload.focus,
            viewport: payload.viewport,
            truncation: payload.truncation,
        },
        options,
    )
}

pub(super) async fn capture_locator(
    locator: &Locator,
    options: SnapshotOptions,
) -> Result<ElementSnapshot, BrowserError> {
    validate_options(options)?;
    let page = locator.page_for_snapshot();
    let operation = page.admit_operation("capture locator snapshot")?;
    let resolved = locator.resolve_admitted(&operation).await?;
    let store = page.locator_frame_store(&operation).await?;
    let resolved_route = resolved.route.clone();
    let sequence = NEXT_SNAPSHOT_GROUP.fetch_add(1, Ordering::Relaxed);
    let group = format!("browserkit-snapshot-{}-{sequence}", page.target_id());
    let cleanup_session = resolved.session.clone();
    let cleanup_group = group.clone();
    let cleanup = page.track_locator_cleanup(group.clone(), move || async move {
        match ReleaseObjectGroup::new(cleanup_group)
            .send(&cleanup_session)
            .await
            .map_err(super::OwnershipCleanupError::from)
        {
            Err(error) if error.is_missing_session() || error.is_missing_target() => Ok(()),
            result => result,
        }
    });
    let primary = async {
        let object = ResolveNode::new()
            .with_backend_node_id(resolved.backend_node_id)
            .with_object_group(group.clone())
            .send(&resolved.session)
            .await
            .map_err(|error| {
                BrowserError::cdp_operation(
                    "resolve locator snapshot root",
                    OperationPhase::Observation,
                    error,
                )
            })?
            .object;
        let object_id = object.object_id.ok_or_else(|| {
            BrowserError::operation("capture locator snapshot", OperationPhase::Observation)
                .with_message("resolved locator has no remote object")
        })?;
        let function = format!(
            "function() {{ return ({function}).call(this, {max_elements}, true); }}",
            function = dom::CANDIDATES_FUNCTION,
            max_elements = options.max_elements
        );
        let response = CallFunctionOn::new(function)
            .with_object_id(object_id)
            .send(&resolved.session)
            .await
            .map_err(|error| {
                BrowserError::cdp_operation(
                    "collect locator snapshot candidates",
                    OperationPhase::Observation,
                    error,
                )
            })?;
        let candidates = response.result.object_id.ok_or_else(|| {
            BrowserError::operation("capture locator snapshot", OperationPhase::Observation)
                .with_message("candidate collection returned no remote array")
        })?;
        let (mut elements, truncation) =
            capture_elements(&resolved.session, candidates, options, 0, true).await?;
        let mut root = elements.drain(..1).next().ok_or_else(|| {
            BrowserError::operation("capture locator snapshot", OperationPhase::Observation)
                .with_message("locator root disappeared during snapshot")
        })?;
        root.descendants = elements;
        root.truncation = truncation;
        locator.validate_scope().await?;
        Ok(root)
    }
    .await;
    let cleanup_result = cleanup.cleanup().await;
    match (primary, cleanup_result) {
        (Ok(value), Ok(())) => {
            store
                .validate_locator_route_authoritative(&resolved_route)
                .await?;
            locator.validate_scope().await?;
            finalize_element_snapshot(value, options)
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(BrowserError::operation(
            "release snapshot object group",
            OperationPhase::Cleanup,
        )
        .with_message(error.to_string())),
        (Err(error), Err(cleanup)) => {
            Err(error.with_cleanup_failure(super::CleanupFailure::new(group, cleanup.to_string())))
        }
    }
}

async fn capture_frame_admitted(
    page: &Page,
    store: &super::FrameStore,
    route: &super::LocatorFrameRoute,
    options: SnapshotOptions,
    authoritative: bool,
) -> Result<FramePayload, BrowserError> {
    let world = CreateIsolatedWorld::new(route.frame_id.as_str().to_owned())
        .with_world_name(SNAPSHOT_WORLD)
        .with_grant_univeral_access(false)
        .send(&route.session)
        .await
        .map_err(|error| {
            BrowserError::cdp_operation(
                "create snapshot isolated world",
                OperationPhase::Observation,
                error,
            )
        })?;
    store.validate_locator_route(route)?;
    let metadata = Evaluate::new(dom::document_expression(options.max_bytes))
        .with_context_id(world.execution_context_id)
        .with_return_by_value(true)
        .send(&route.session)
        .await
        .map_err(|error| {
            BrowserError::cdp_operation(
                "capture document facts",
                OperationPhase::Observation,
                error,
            )
        })?;
    let metadata = remote_value(
        metadata.result.value,
        metadata.exception_details.map(|details| details.text),
        "capture document facts",
    )?;
    let metadata: dom::DocumentFacts = serde_json::from_value(metadata).map_err(invalid_payload)?;
    if !metadata.required_fields_fit {
        return Err(budget_too_small("frame URL/title", options.max_bytes));
    }
    let metadata_url = metadata
        .url
        .ok_or_else(|| invalid_required_document_fact("URL"))?;
    let metadata_title = metadata
        .title
        .ok_or_else(|| invalid_required_document_fact("title"))?;
    let sequence = NEXT_SNAPSHOT_GROUP.fetch_add(1, Ordering::Relaxed);
    let group = format!("browserkit-snapshot-{}-{sequence}", page.target_id());
    let cleanup_session = route.session.clone();
    let cleanup_group = group.clone();
    let cleanup = page.track_locator_cleanup(group.clone(), move || async move {
        match ReleaseObjectGroup::new(cleanup_group)
            .send(&cleanup_session)
            .await
            .map_err(super::OwnershipCleanupError::from)
        {
            Err(error) if error.is_missing_session() || error.is_missing_target() => Ok(()),
            result => result,
        }
    });
    let primary = async {
        let candidates = Evaluate::new(dom::candidates_expression(options.max_elements, false))
            .with_object_group(group.clone())
            .with_context_id(world.execution_context_id)
            .send(&route.session)
            .await
            .map_err(|error| {
                BrowserError::cdp_operation(
                    "collect snapshot candidates",
                    OperationPhase::Observation,
                    error,
                )
            })?;
        let candidates = candidates.result.object_id.ok_or_else(|| {
            BrowserError::operation("capture structured snapshot", OperationPhase::Observation)
                .with_message("candidate collection returned no remote array")
        })?;
        let (elements, mut truncation) = capture_elements(
            &route.session,
            candidates,
            options,
            metadata.truncation.emitted_bytes,
            false,
        )
        .await?;
        truncation.omitted_bytes += metadata.truncation.omitted_bytes;
        let focus = elements.iter().position(|element| element.focused);
        store.validate_locator_route(route)?;
        Ok(FramePayload {
            url: metadata_url,
            title: metadata_title,
            load_state: metadata.load_state,
            visible_text: metadata.visible_text,
            elements,
            focus,
            viewport: metadata.viewport,
            truncation,
            parent_id: None,
            child_ids: Vec::new(),
        })
    }
    .await;
    let cleanup_result = cleanup.cleanup().await;
    match (primary, cleanup_result) {
        (Ok(mut payload), Ok(())) => {
            if authoritative {
                let identity = store.validate_locator_route_authoritative(route).await?;
                payload.parent_id = identity.parent_id;
                payload.child_ids = identity.child_ids;
            } else {
                store.validate_locator_route(route)?;
            }
            Ok(payload)
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(BrowserError::operation(
            "release snapshot object group",
            OperationPhase::Cleanup,
        )
        .with_message(error.to_string())),
        (Err(error), Err(cleanup)) => {
            Err(error.with_cleanup_failure(super::CleanupFailure::new(group, cleanup.to_string())))
        }
    }
}

struct FramePayload {
    url: String,
    title: String,
    load_state: DocumentLoadState,
    visible_text: String,
    elements: Vec<ElementSnapshot>,
    focus: Option<usize>,
    viewport: ViewportSnapshot,
    truncation: SnapshotTruncation,
    parent_id: Option<String>,
    child_ids: Vec<String>,
}

async fn capture_elements(
    session: &cdpkit::Session,
    candidates: cdpkit::runtime::types::RemoteObjectId,
    options: SnapshotOptions,
    initial_bytes: usize,
    required_first: bool,
) -> Result<(Vec<ElementSnapshot>, SnapshotTruncation), BrowserError> {
    let properties = GetProperties::new(candidates)
        .with_own_properties(true)
        .send(session)
        .await
        .map_err(|error| {
            BrowserError::cdp_operation(
                "read snapshot candidates",
                OperationPhase::Observation,
                error,
            )
        })?;
    if let Some(exception) = properties.exception_details {
        return Err(BrowserError::operation(
            "read snapshot candidates",
            OperationPhase::Observation,
        )
        .with_message(exception.text));
    }
    let total = properties
        .result
        .iter()
        .find(|property| property.name == "__browserkitTotal")
        .and_then(|property| property.value.as_ref())
        .and_then(|value| value.value.as_ref())
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let mut nodes = properties
        .result
        .into_iter()
        .filter_map(|property| {
            let index = property.name.parse::<usize>().ok()?;
            let object_id = property.value?.object_id?;
            Some((index, object_id))
        })
        .collect::<Vec<_>>();
    nodes.sort_by_key(|(index, _)| *index);
    let mut elements = Vec::new();
    let mut renderer_bytes = initial_bytes;
    let mut omitted_bytes = 0;
    let mut unavailable_accessibility = 0;
    for (_, object_id) in nodes {
        let remaining = options.max_bytes.saturating_sub(renderer_bytes);
        let function = format!(
            "function() {{ return ({function}).call(this, {remaining}); }}",
            function = dom::ELEMENT_FACTS_FUNCTION
        );
        let response = CallFunctionOn::new(function)
            .with_object_id(object_id.clone())
            .with_return_by_value(true)
            .send(session)
            .await
            .map_err(|error| {
                BrowserError::cdp_operation(
                    "capture element facts",
                    OperationPhase::Observation,
                    error,
                )
            })?;
        let value = remote_value(
            response.result.value,
            response.exception_details.map(|details| details.text),
            "capture element facts",
        )?;
        let facts: dom::ElementFacts = serde_json::from_value(value).map_err(invalid_payload)?;
        omitted_bytes += facts.omitted_bytes;
        renderer_bytes += facts.tag_name.len()
            + facts.id.as_ref().map_or(0, String::len)
            + facts.test_id.as_ref().map_or(0, String::len)
            + facts.text.len();
        let backend_node_id = DescribeNode::new()
            .with_object_id(object_id)
            .send(session)
            .await
            .map_err(|error| {
                BrowserError::cdp_operation(
                    "identify snapshot element",
                    OperationPhase::Observation,
                    error,
                )
            })?
            .node
            .backend_node_id;
        let (accessibility, unavailable) = match GetPartialAxTree::new()
            .with_backend_node_id(backend_node_id)
            .with_fetch_relatives(false)
            .send(session)
            .await
        {
            Ok(response) => {
                let node = response
                    .nodes
                    .iter()
                    .find(|node| node.backend_dom_node_id == Some(backend_node_id));
                let facts = accessibility::facts_from_ax_node(node, false);
                let unavailable = facts.availability != AccessibilityAvailability::Available;
                (facts, unavailable)
            }
            Err(error) => {
                let mut facts = accessibility::facts_from_ax_node(None, false);
                facts.unavailable_reason =
                    Some(format!("Chrome accessibility query failed: {error}"));
                (facts, true)
            }
        };
        unavailable_accessibility += usize::from(unavailable);
        let element = ElementSnapshot {
            tag_name: facts.tag_name,
            id: facts.id,
            test_id: facts.test_id,
            text: facts.text,
            bounds: facts.bounds,
            accessibility,
            focused: facts.focused,
            descendants: Vec::new(),
            truncation: SnapshotTruncation::default(),
        };
        elements.push(element);
    }
    if required_first && elements.is_empty() {
        return Err(BrowserError::operation(
            "capture locator snapshot",
            OperationPhase::Observation,
        )
        .with_message("locator root disappeared during snapshot"));
    }
    let emitted_elements = elements.len();
    Ok((
        elements,
        SnapshotTruncation {
            max_bytes: options.max_bytes,
            max_elements: options.max_elements,
            emitted_bytes: renderer_bytes,
            emitted_elements,
            omitted_bytes,
            omitted_elements: total.saturating_sub(emitted_elements),
            omitted_frames: 0,
            unavailable_accessibility,
        },
    ))
}

fn serialized_len(value: &impl Serialize) -> Result<usize, BrowserError> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .map_err(invalid_payload)
}

fn settle_page(snapshot: &mut PageSnapshot) -> Result<usize, BrowserError> {
    for _ in 0..8 {
        let length = serialized_len(snapshot)?;
        if snapshot.truncation.emitted_bytes == length {
            return Ok(length);
        }
        snapshot.truncation.emitted_bytes = length;
    }
    serialized_len(snapshot)
}

fn settle_frame(snapshot: &mut FrameSnapshotView) -> Result<usize, BrowserError> {
    for _ in 0..8 {
        let length = serialized_len(snapshot)?;
        if snapshot.truncation.emitted_bytes == length {
            return Ok(length);
        }
        snapshot.truncation.emitted_bytes = length;
    }
    serialized_len(snapshot)
}

fn settle_element(snapshot: &mut ElementSnapshot) -> Result<usize, BrowserError> {
    for _ in 0..8 {
        let length = serialized_len(snapshot)?;
        if snapshot.truncation.emitted_bytes == length {
            return Ok(length);
        }
        snapshot.truncation.emitted_bytes = length;
    }
    serialized_len(snapshot)
}

fn trim_utf8_to(value: &mut String, keep_bytes: usize) -> usize {
    if value.len() <= keep_bytes {
        return 0;
    }
    let original = value.len();
    let mut boundary = keep_bytes.min(value.len());
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    original - value.len()
}

fn trim_for_overflow(value: &mut String, overflow: usize) -> usize {
    let keep = value.len().saturating_sub(overflow.max(1));
    trim_utf8_to(value, keep)
}

fn truncate_element_variables(element: &mut ElementSnapshot) -> usize {
    let mut omitted = trim_utf8_to(&mut element.text, 0);
    omitted += trim_utf8_to(&mut element.tag_name, 0);
    omitted += element.id.take().map_or(0, |value| value.len());
    omitted += element.test_id.take().map_or(0, |value| value.len());
    let accessibility = &mut element.accessibility;
    let mut accessibility_omitted = 0;
    accessibility_omitted += accessibility.role.take().map_or(0, |value| value.len());
    accessibility_omitted += accessibility.name.take().map_or(0, |value| value.len());
    accessibility_omitted += accessibility
        .description
        .take()
        .map_or(0, |value| value.len());
    accessibility_omitted += accessibility.value.take().map_or(0, |value| value.len());
    accessibility_omitted += accessibility
        .unavailable_reason
        .take()
        .map_or(0, |value| value.len());
    if accessibility_omitted > 0
        && accessibility.availability == AccessibilityAvailability::Available
    {
        accessibility.availability = AccessibilityAvailability::Truncated;
        accessibility.unavailable_reason =
            Some("snapshot accessibility budget was exhausted".to_owned());
    }
    omitted + accessibility_omitted
}

fn budget_too_small(kind: &'static str, max_bytes: usize) -> BrowserError {
    BrowserError::operation("capture snapshot", OperationPhase::Preparation).with_message(format!(
        "snapshot byte budget {max_bytes} is too small for the fixed {kind} envelope"
    ))
}

fn finalize_page_snapshot(
    mut snapshot: PageSnapshot,
    options: SnapshotOptions,
) -> Result<PageSnapshot, BrowserError> {
    snapshot.truncation.max_bytes = options.max_bytes;
    snapshot.truncation.max_elements = options.max_elements;
    snapshot.truncation.emitted_elements = snapshot.elements.len()
        + snapshot
            .frames
            .iter()
            .map(|frame| frame.elements.len())
            .sum::<usize>();
    while settle_page(&mut snapshot)? > options.max_bytes && !snapshot.frames.is_empty() {
        let frame = snapshot.frames.pop().expect("checked non-empty");
        snapshot.truncation.omitted_frames += 1;
        snapshot.truncation.omitted_elements += frame.elements.len();
        snapshot.truncation.emitted_elements = snapshot
            .truncation
            .emitted_elements
            .saturating_sub(frame.elements.len());
        snapshot.truncation.omitted_bytes += serialized_len(&frame)?;
    }
    while settle_page(&mut snapshot)? > options.max_bytes && snapshot.elements.len() > 1 {
        let element = snapshot.elements.pop().expect("checked non-empty");
        snapshot.truncation.omitted_elements += 1;
        snapshot.truncation.emitted_elements -= 1;
        snapshot.truncation.omitted_bytes += serialized_len(&element)?;
        if snapshot
            .focus
            .is_some_and(|index| index >= snapshot.elements.len())
        {
            snapshot.focus = None;
        }
    }
    let length = settle_page(&mut snapshot)?;
    if length > options.max_bytes {
        let removed = trim_for_overflow(
            &mut snapshot.visible_text,
            length.saturating_sub(options.max_bytes) + 16,
        );
        snapshot.truncation.omitted_bytes += removed;
    }
    if settle_page(&mut snapshot)? > options.max_bytes {
        let removed = snapshot
            .elements
            .iter_mut()
            .map(truncate_element_variables)
            .sum::<usize>();
        snapshot.truncation.omitted_bytes += removed;
    }
    if settle_page(&mut snapshot)? > options.max_bytes {
        snapshot.truncation.omitted_bytes += trim_utf8_to(&mut snapshot.visible_text, 0);
        snapshot.truncation.omitted_bytes += trim_utf8_to(&mut snapshot.title, 0);
        snapshot.truncation.omitted_bytes += trim_utf8_to(&mut snapshot.url, 0);
    }
    while settle_page(&mut snapshot)? > options.max_bytes && !snapshot.elements.is_empty() {
        let element = snapshot.elements.pop().expect("checked non-empty");
        snapshot.truncation.omitted_elements += 1;
        snapshot.truncation.emitted_elements =
            snapshot.truncation.emitted_elements.saturating_sub(1);
        snapshot.truncation.omitted_bytes += serialized_len(&element)?;
        snapshot.focus = None;
    }
    let length = settle_page(&mut snapshot)?;
    if length > options.max_bytes {
        return Err(budget_too_small("page snapshot", options.max_bytes));
    }
    debug_assert_eq!(
        snapshot.truncation.emitted_bytes,
        serialized_len(&snapshot)?
    );
    Ok(snapshot)
}

fn finalize_frame_snapshot(
    mut snapshot: FrameSnapshotView,
    options: SnapshotOptions,
) -> Result<FrameSnapshotView, BrowserError> {
    snapshot.truncation.max_bytes = options.max_bytes;
    snapshot.truncation.max_elements = options.max_elements;
    snapshot.truncation.emitted_elements = snapshot.elements.len();
    while settle_frame(&mut snapshot)? > options.max_bytes && snapshot.elements.len() > 1 {
        let element = snapshot.elements.pop().expect("checked non-empty");
        snapshot.truncation.omitted_elements += 1;
        snapshot.truncation.emitted_elements -= 1;
        snapshot.truncation.omitted_bytes += serialized_len(&element)?;
        if snapshot
            .focus
            .is_some_and(|index| index >= snapshot.elements.len())
        {
            snapshot.focus = None;
        }
    }
    let length = settle_frame(&mut snapshot)?;
    if length > options.max_bytes {
        snapshot.truncation.omitted_bytes += trim_for_overflow(
            &mut snapshot.visible_text,
            length.saturating_sub(options.max_bytes) + 16,
        );
    }
    if settle_frame(&mut snapshot)? > options.max_bytes {
        snapshot.truncation.omitted_bytes += snapshot
            .elements
            .iter_mut()
            .map(truncate_element_variables)
            .sum::<usize>();
        snapshot.truncation.omitted_bytes += trim_utf8_to(&mut snapshot.visible_text, 0);
        snapshot.truncation.omitted_bytes += trim_utf8_to(&mut snapshot.title, 0);
        snapshot.truncation.omitted_bytes += trim_utf8_to(&mut snapshot.url, 0);
    }
    while settle_frame(&mut snapshot)? > options.max_bytes && !snapshot.elements.is_empty() {
        let element = snapshot.elements.pop().expect("checked non-empty");
        snapshot.truncation.omitted_elements += 1;
        snapshot.truncation.emitted_elements =
            snapshot.truncation.emitted_elements.saturating_sub(1);
        snapshot.truncation.omitted_bytes += serialized_len(&element)?;
        snapshot.focus = None;
    }
    if settle_frame(&mut snapshot)? > options.max_bytes {
        return Err(budget_too_small("frame snapshot", options.max_bytes));
    }
    Ok(snapshot)
}

fn finalize_element_snapshot(
    mut snapshot: ElementSnapshot,
    options: SnapshotOptions,
) -> Result<ElementSnapshot, BrowserError> {
    snapshot.truncation.max_bytes = options.max_bytes;
    snapshot.truncation.max_elements = options.max_elements;
    snapshot.truncation.emitted_elements = 1 + snapshot.descendants.len();
    while settle_element(&mut snapshot)? > options.max_bytes && !snapshot.descendants.is_empty() {
        let descendant = snapshot.descendants.pop().expect("checked non-empty");
        snapshot.truncation.omitted_elements += 1;
        snapshot.truncation.emitted_elements -= 1;
        snapshot.truncation.omitted_bytes += serialized_len(&descendant)?;
    }
    let length = settle_element(&mut snapshot)?;
    if length > options.max_bytes {
        snapshot.truncation.omitted_bytes += trim_for_overflow(
            &mut snapshot.text,
            length.saturating_sub(options.max_bytes) + 16,
        );
    }
    if settle_element(&mut snapshot)? > options.max_bytes {
        snapshot.truncation.omitted_bytes += truncate_element_variables(&mut snapshot);
    }
    if settle_element(&mut snapshot)? > options.max_bytes {
        return Err(budget_too_small("element snapshot", options.max_bytes));
    }
    Ok(snapshot)
}

fn remote_value(
    value: Option<Value>,
    exception: Option<String>,
    operation: &'static str,
) -> Result<Value, BrowserError> {
    if let Some(exception) = exception {
        return Err(
            BrowserError::operation(operation, OperationPhase::Observation).with_message(exception),
        );
    }
    value.ok_or_else(|| {
        BrowserError::operation(operation, OperationPhase::Observation)
            .with_message("snapshot evaluation returned no value")
    })
}

fn invalid_payload(error: serde_json::Error) -> BrowserError {
    BrowserError::operation("decode structured snapshot", OperationPhase::Observation)
        .with_message(error.to_string())
}

fn invalid_required_document_fact(field: &'static str) -> BrowserError {
    BrowserError::operation("decode structured snapshot", OperationPhase::Observation)
        .with_message(format!("snapshot document omitted required {field}"))
}
fn validate_options(options: SnapshotOptions) -> Result<(), BrowserError> {
    if options.max_bytes < 512 || options.max_elements == 0 {
        return Err(
            BrowserError::operation("capture snapshot", OperationPhase::Preparation)
                .with_message("snapshot byte limit must be at least 512 and element limit must be greater than zero"),
        );
    }
    Ok(())
}
fn combine_truncation(
    options: SnapshotOptions,
    frames: &[FrameSnapshotView],
) -> SnapshotTruncation {
    SnapshotTruncation {
        max_bytes: options.max_bytes,
        max_elements: options.max_elements,
        emitted_bytes: frames.iter().map(|f| f.truncation.emitted_bytes).sum(),
        emitted_elements: frames.iter().map(|f| f.truncation.emitted_elements).sum(),
        omitted_bytes: frames.iter().map(|f| f.truncation.omitted_bytes).sum(),
        omitted_elements: frames.iter().map(|f| f.truncation.omitted_elements).sum(),
        omitted_frames: 0,
        unavailable_accessibility: frames
            .iter()
            .map(|f| f.truncation.unavailable_accessibility)
            .sum(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use serde_json::{json, Value};
    use std::sync::{Arc, Weak};
    use tokio_tungstenite::tungstenite::Message;
    #[test]
    fn truncation_is_explicit() {
        let value = SnapshotTruncation {
            max_bytes: 10,
            max_elements: 1,
            emitted_bytes: 10,
            emitted_elements: 1,
            omitted_bytes: 4,
            omitted_elements: 2,
            omitted_frames: 0,
            unavailable_accessibility: 1,
        };
        assert!(value.is_truncated());
    }
    #[test]
    fn raw_html_is_not_part_of_structured_snapshot_contract() {
        let element = ElementSnapshot {
            tag_name: "button".to_owned(),
            id: Some("save".to_owned()),
            test_id: None,
            text: "Save".to_owned(),
            bounds: ElementBounds {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
            },
            accessibility: AccessibilityFacts::default(),
            focused: false,
            descendants: Vec::new(),
            truncation: SnapshotTruncation::default(),
        };
        let frame = FrameSnapshotView {
            id: "main".to_owned(),
            parent_id: None,
            child_ids: Vec::new(),
            url: "https://example.test".to_owned(),
            title: "Example".to_owned(),
            load_state: DocumentLoadState::Complete,
            visible_text: "Save".to_owned(),
            elements: vec![element.clone()],
            focus: None,
            viewport: ViewportSnapshot {
                width: 1.0,
                height: 1.0,
                scroll_x: 0.0,
                scroll_y: 0.0,
                document_width: 1.0,
                document_height: 1.0,
            },
            truncation: SnapshotTruncation::default(),
        };
        let page = PageSnapshot {
            main_frame_id: frame.id.clone(),
            url: frame.url.clone(),
            title: frame.title.clone(),
            load_state: frame.load_state,
            visible_text: frame.visible_text.clone(),
            elements: vec![element.clone()],
            focus: None,
            viewport: frame.viewport,
            frames: vec![frame],
            truncation: SnapshotTruncation::default(),
        };
        for json in [
            serde_json::to_value(element).unwrap(),
            serde_json::to_value(page.frames[0].clone()).unwrap(),
            serde_json::to_value(page).unwrap(),
        ] {
            let encoded = json.to_string().to_ascii_lowercase();
            assert!(!encoded.contains("html"));
            assert!(!encoded.contains("workflow"));
            assert!(!encoded.contains("assertion"));
            assert!(!encoded.contains("agent"));
        }
    }

    #[test]
    fn element_budget_truncates_chrome_accessibility_without_guessing() {
        let mut accessibility = AccessibilityFacts {
            availability: AccessibilityAvailability::Available,
            ..AccessibilityFacts::default()
        };
        accessibility.name = Some("Chrome computed name ".repeat(128));
        let element = ElementSnapshot {
            tag_name: "button".to_owned(),
            id: Some("save".to_owned()),
            test_id: None,
            text: "Save".repeat(128),
            bounds: ElementBounds {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
            },
            accessibility,
            focused: false,
            descendants: Vec::new(),
            truncation: SnapshotTruncation::default(),
        };
        let snapshot =
            finalize_element_snapshot(element, SnapshotOptions::default().with_max_bytes(700))
                .unwrap();
        assert_eq!(
            snapshot.accessibility.availability,
            AccessibilityAvailability::Truncated
        );
        assert!(serialized_len(&snapshot).unwrap() <= 700);
        assert_eq!(
            snapshot.truncation.emitted_bytes,
            serialized_len(&snapshot).unwrap()
        );
    }

    #[test]
    fn fixed_snapshot_envelope_reports_budget_too_small() {
        let snapshot = PageSnapshot {
            main_frame_id: String::new(),
            url: String::new(),
            title: String::new(),
            load_state: DocumentLoadState::Complete,
            visible_text: String::new(),
            elements: Vec::new(),
            focus: None,
            viewport: ViewportSnapshot {
                width: 0.0,
                height: 0.0,
                scroll_x: 0.0,
                scroll_y: 0.0,
                document_width: 0.0,
                document_height: 0.0,
            },
            frames: Vec::new(),
            truncation: SnapshotTruncation::default(),
        };
        let error = finalize_page_snapshot(
            snapshot,
            SnapshotOptions {
                max_bytes: 1,
                max_elements: 1,
            },
        )
        .unwrap_err();
        assert_eq!(error.phase(), OperationPhase::Preparation);
        assert!(error.to_string().contains("fixed page snapshot envelope"));
    }

    #[test]
    fn frame_budget_never_turns_known_relationships_into_empty_facts() {
        let snapshot = FrameSnapshotView {
            id: "child".repeat(64),
            parent_id: Some("parent".repeat(64)),
            child_ids: vec!["grandchild".repeat(64)],
            url: "https://example.test/".repeat(32),
            title: "Title".repeat(64),
            load_state: DocumentLoadState::Complete,
            visible_text: "Visible".repeat(128),
            elements: Vec::new(),
            focus: None,
            viewport: ViewportSnapshot {
                width: 1.0,
                height: 1.0,
                scroll_x: 0.0,
                scroll_y: 0.0,
                document_width: 1.0,
                document_height: 1.0,
            },
            truncation: SnapshotTruncation::default(),
        };
        match finalize_frame_snapshot(snapshot, SnapshotOptions::default().with_max_bytes(512)) {
            Ok(snapshot) => {
                assert!(snapshot.parent_id.is_some());
                assert!(!snapshot.child_ids.is_empty());
            }
            Err(error) => assert!(error.to_string().contains("fixed frame snapshot envelope")),
        }
    }

    #[test]
    fn page_budget_never_clears_the_known_main_frame_identity() {
        let main_frame_id = "main-frame-id".repeat(128);
        let snapshot = PageSnapshot {
            main_frame_id: main_frame_id.clone(),
            url: String::new(),
            title: String::new(),
            load_state: DocumentLoadState::Complete,
            visible_text: String::new(),
            elements: Vec::new(),
            focus: None,
            viewport: ViewportSnapshot {
                width: 0.0,
                height: 0.0,
                scroll_x: 0.0,
                scroll_y: 0.0,
                document_width: 0.0,
                document_height: 0.0,
            },
            frames: Vec::new(),
            truncation: SnapshotTruncation::default(),
        };
        match finalize_page_snapshot(snapshot, SnapshotOptions::default().with_max_bytes(512)) {
            Ok(snapshot) => assert_eq!(snapshot.main_frame_id, main_frame_id),
            Err(error) => assert!(error.to_string().contains("fixed page snapshot envelope")),
        }
    }

    async fn fake_snapshot_page(
        ax_error: bool,
        stall_ax: bool,
        release_error: bool,
        navigate_during_release: bool,
        include_child_frame: bool,
        add_child_during_snapshot_cleanup: bool,
    ) -> (
        Page,
        Arc<parking_lot::Mutex<Vec<Value>>>,
        Arc<tokio::sync::Notify>,
    ) {
        use crate::runtime::{BrowserRuntime, BrowserSessionId, PageOwnership};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let commands = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let server_commands = Arc::clone(&commands);
        let ax_started = Arc::new(tokio::sync::Notify::new());
        let server_ax_started = Arc::clone(&ax_started);
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut write, mut read) = websocket.split();
            let mut authoritative_loader = "loader";
            let mut authoritative_child = include_child_frame;
            while let Some(Ok(Message::Text(text))) = read.next().await {
                let command: Value = serde_json::from_str(&text).unwrap();
                server_commands.lock().push(command.clone());
                let id = command["id"].as_u64().unwrap();
                let method = command["method"].as_str().unwrap();
                if method == "Accessibility.getPartialAXTree" {
                    server_ax_started.notify_one();
                    if stall_ax {
                        continue;
                    }
                    if ax_error {
                        let mut response = json!({"id": id, "error": {"code": -32000, "message": "AX unavailable"}});
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
                if method == "Runtime.releaseObjectGroup" && release_error {
                    let mut response =
                        json!({"id": id, "error": {"code": -32000, "message": "release failed"}});
                    if let Some(session_id) = command.get("sessionId") {
                        response["sessionId"] = session_id.clone();
                    }
                    write
                        .send(Message::Text(response.to_string().into()))
                        .await
                        .unwrap();
                    continue;
                }
                if method == "Runtime.releaseObjectGroup"
                    && navigate_during_release
                    && command["params"]["objectGroup"]
                        .as_str()
                        .is_some_and(|group| group.starts_with("browserkit-snapshot-"))
                {
                    authoritative_loader = "replacement-loader";
                }
                if method == "Runtime.releaseObjectGroup"
                    && add_child_during_snapshot_cleanup
                    && command["params"]["objectGroup"]
                        .as_str()
                        .is_some_and(|group| group.starts_with("browserkit-snapshot-"))
                {
                    authoritative_child = true;
                }
                let result = match method {
                    "Browser.getVersion" => crate::runtime::test_browser_version_result(),
                    "Target.getBrowserContexts" => json!({"browserContextIds": []}),
                    "Target.setDiscoverTargets"
                    | "Page.enable"
                    | "Page.disable"
                    | "Target.setAutoAttach"
                    | "Target.detachFromTarget"
                    | "Runtime.releaseObjectGroup" => json!({}),
                    "Page.getFrameTree" => {
                        let mut tree = json!({"frame": {"id": "main", "loaderId": authoritative_loader, "url": "https://example.test/", "domainAndRegistry": "example.test", "securityOrigin": "https://example.test", "mimeType": "text/html", "secureContextType": "Secure", "crossOriginIsolatedContextType": "NotIsolated", "gatedAPIFeatures": []}});
                        if authoritative_child {
                            tree["childFrames"] = json!([{"frame": {"id": "child", "parentId": "main", "loaderId": "loader-child", "url": "https://example.test/child", "domainAndRegistry": "example.test", "securityOrigin": "https://example.test", "mimeType": "text/html", "secureContextType": "Secure", "crossOriginIsolatedContextType": "NotIsolated", "gatedAPIFeatures": []}}]);
                        }
                        json!({"frameTree": tree})
                    }
                    "Page.createIsolatedWorld" => json!({"executionContextId": 91}),
                    "Runtime.evaluate" if command["params"]["returnByValue"] == json!(true) => {
                        json!({"result": {"type": "object", "value": {"url": "https://example.test/", "title": "Example", "requiredFieldsFit": true, "loadState": "complete", "visibleText": "Save", "viewport": {"width": 800.0, "height": 600.0, "scrollX": 0.0, "scrollY": 0.0, "documentWidth": 800.0, "documentHeight": 900.0}, "truncation": {"maxBytes": 4096, "maxElements": 0, "emittedBytes": 4, "emittedElements": 0, "omittedBytes": 0, "omittedElements": 0, "omittedFrames": 0, "unavailableAccessibility": 0}}}})
                    }
                    "Runtime.evaluate" => {
                        json!({"result": {"type": "object", "subtype": "array", "objectId": "candidates"}})
                    }
                    "Runtime.getProperties" => json!({"result": [
                        {"name": "0", "value": {"type": "object", "subtype": "node", "objectId": "button"}, "configurable": true, "enumerable": true},
                        {"name": "__browserkitTotal", "value": {"type": "number", "value": 1}, "configurable": false, "enumerable": true}
                    ], "internalProperties": []}),
                    "Runtime.callFunctionOn" if command["params"]["awaitPromise"] == true => {
                        json!({"result": {"type": "object", "value": {"attached": true, "visible": true, "enabled": true, "stable": true, "obscured": false}}})
                    }
                    "Runtime.callFunctionOn" if command["params"]["returnByValue"] != true => {
                        json!({"result": {"type": "object", "subtype": "array", "objectId": "candidates"}})
                    }
                    "Runtime.callFunctionOn" => {
                        json!({"result": {"type": "object", "value": {"tagName": "button", "id": "save", "testId": null, "text": "Save", "focused": true, "omittedBytes": 0, "bounds": {"x": 10.0, "y": 20.0, "width": 80.0, "height": 30.0}}}})
                    }
                    "DOM.resolveNode" => {
                        json!({"object": {"type": "object", "subtype": "node", "objectId": "root"}})
                    }
                    "DOM.describeNode" => {
                        json!({"node": {"nodeId": 7, "backendNodeId": 41, "nodeType": 1, "nodeName": "BUTTON", "localName": "button", "nodeValue": ""}})
                    }
                    "Accessibility.getPartialAXTree" => {
                        json!({"nodes": [{"nodeId": "ax-1", "ignored": false, "role": {"type": "role", "value": "button"}, "name": {"type": "computedString", "value": "Save profile"}, "backendDOMNodeId": 41}]})
                    }
                    other => panic!("unexpected snapshot test command: {other}"),
                };
                let mut response = json!({"id": id, "result": result});
                if let Some(session_id) = command.get("sessionId") {
                    response["sessionId"] = session_id.clone();
                }
                write
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .unwrap();
            }
        });
        let runtime = BrowserRuntime::connect(format!("ws://{address}"))
            .await
            .unwrap();
        let page = Page::new(
            runtime.clone(),
            BrowserSessionId::new("owner"),
            Weak::new(),
            "target-1".to_owned(),
            PageOwnership::Attached,
            runtime.cdp().session("frame-session"),
        );
        (page, commands, ax_started)
    }

    #[tokio::test]
    async fn fake_cdp_ax_unavailable_is_explicit_and_object_group_is_released() {
        let (page, commands, _) = fake_snapshot_page(true, false, false, false, false, false).await;
        let snapshot = page
            .snapshot(SnapshotOptions::default().with_max_bytes(4096))
            .await
            .unwrap();
        assert_eq!(
            snapshot.elements[0].accessibility.availability,
            AccessibilityAvailability::Unavailable
        );
        assert!(snapshot.elements[0]
            .accessibility
            .unavailable_reason
            .as_deref()
            .unwrap()
            .contains("AX unavailable"));
        let methods = commands
            .lock()
            .iter()
            .filter_map(|command| command["method"].as_str())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert!(methods.contains(&"Runtime.releaseObjectGroup".to_owned()));
    }

    #[tokio::test]
    async fn cancelled_snapshot_hands_remote_group_to_page_cleanup() {
        let (page, commands, ax_started) =
            fake_snapshot_page(false, true, false, false, false, false).await;
        let snapshot_page = page.clone();
        let task = tokio::spawn(async move {
            snapshot_page
                .snapshot(SnapshotOptions::default().with_max_bytes(4096))
                .await
        });
        ax_started.notified().await;
        task.abort();
        let _ = task.await;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
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
        .expect("cancelled snapshot did not release its object group");
        assert_eq!(
            commands
                .lock()
                .iter()
                .filter(|command| command["method"] == "Runtime.releaseObjectGroup")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn cancelled_snapshot_release_failure_is_reported_by_page_close() {
        let (page, _, ax_started) =
            fake_snapshot_page(false, true, true, false, false, false).await;
        let snapshot_page = page.clone();
        let task = tokio::spawn(async move {
            snapshot_page
                .snapshot(SnapshotOptions::default().with_max_bytes(4096))
                .await
        });
        ax_started.notified().await;
        task.abort();
        let _ = task.await;
        let report = page.close().await;
        assert!(!report.is_complete());
        assert!(report.failures().iter().any(|failure| {
            failure.resource().starts_with("browserkit-snapshot-")
                && failure.message().contains("release failed")
        }));
    }

    #[tokio::test]
    async fn max_bytes_bounds_the_final_serialized_page_snapshot() {
        let (page, _, _) = fake_snapshot_page(false, false, false, false, false, false).await;
        let snapshot = page
            .snapshot(
                SnapshotOptions::default()
                    .with_max_bytes(700)
                    .with_max_elements(1),
            )
            .await
            .unwrap();
        let bytes = serde_json::to_vec(&snapshot).unwrap().len();
        assert!(bytes <= 700, "serialized snapshot used {bytes} bytes");
        assert_eq!(snapshot.truncation.emitted_bytes, bytes);
    }

    #[tokio::test]
    async fn page_distributes_one_global_renderer_budget_across_frames() {
        let (page, commands, _) = fake_snapshot_page(false, false, false, false, true, false).await;
        let mut metrics = PageCaptureMetrics::default();
        capture_page_impl(
            &page,
            SnapshotOptions::default().with_max_bytes(4096),
            &mut metrics,
        )
        .await
        .unwrap();
        let budgets = commands
            .lock()
            .iter()
            .filter(|command| {
                command["method"] == "Runtime.evaluate"
                    && command["params"]["returnByValue"] == true
            })
            .map(|command| {
                let expression = command["params"]["expression"].as_str().unwrap();
                expression
                    .trim_end_matches(')')
                    .rsplit_once('(')
                    .unwrap()
                    .1
                    .parse::<usize>()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(budgets.len(), 2);
        assert!(budgets.iter().all(|budget| *budget < 4096));
        assert_eq!(budgets[0], 2048);
        let frame_tree_reads = commands
            .lock()
            .iter()
            .filter(|command| command["method"] == "Page.getFrameTree")
            .count();
        assert_eq!(
            frame_tree_reads, 2,
            "one initialization read plus one batched authoritative read"
        );
        assert_eq!(
            metrics.indexed_frame_nodes, 2,
            "the real capture batch must index each authoritative node once"
        );
    }

    #[tokio::test]
    async fn page_snapshot_revalidates_routes_after_remote_cleanup() {
        let (page, _, _) = fake_snapshot_page(false, false, false, true, false, false).await;
        let error = page
            .snapshot(SnapshotOptions::default().with_max_bytes(4096))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("stale"));
    }

    #[tokio::test]
    async fn locator_snapshot_fences_the_resolved_route_after_remote_cleanup() {
        let (page, _, _) = fake_snapshot_page(false, false, false, true, false, false).await;
        let error = page
            .locator("#save")
            .snapshot(SnapshotOptions::default().with_max_bytes(4096))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("stale"));
    }

    #[tokio::test]
    async fn standalone_frame_uses_authoritative_relationships_when_reducer_lags() {
        let (page, _, _) = fake_snapshot_page(false, false, false, false, false, true).await;
        let snapshot = page
            .main_frame()
            .await
            .unwrap()
            .snapshot(SnapshotOptions::default().with_max_bytes(4096))
            .await
            .unwrap();
        assert_eq!(snapshot.parent_id, None);
        assert_eq!(snapshot.child_ids, vec!["child"]);
    }

    async fn serve_fixture(listener: tokio::net::TcpListener, root: String, child: String) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let root = root.clone();
            let child = child.clone();
            tokio::spawn(async move {
                let mut request = [0_u8; 4096];
                let read = stream.read(&mut request).await.unwrap_or_default();
                let request = String::from_utf8_lossy(&request[..read]);
                let body = if request.starts_with("GET /same ") {
                    child
                } else {
                    root
                };
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    }

    #[tokio::test]
    #[ignore = "requires installed Chrome and loopback sockets"]
    async fn live_chrome_snapshot_uses_computed_ax_across_frame_routes_and_shadow_dom() {
        use crate::runtime::{BrowserRuntime, LaunchOptions};
        use std::time::Duration;

        let oopif_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let oopif_port = oopif_listener.local_addr().unwrap().port();
        let parent_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let parent_port = parent_listener.local_addr().unwrap().port();
        let child = "<!doctype html><button id='frame-button'>Frame action</button>".to_owned();
        let parent = format!(
            r#"<!doctype html><title>Snapshot Fixture</title>
<span id=prefix>Billing</span><span id=suffix> notifications</span>
<div id=whitespace>  Hello
   world  </div>
<div id=complex style="width:20px;height:20px" role=switch aria-labelledby="prefix suffix" aria-checked=true tabindex=0></div>
<button id=implicit>Implicit button</button><input id=focused aria-label="Focused field">
<button id=huge>{}</button>
<div id=shadow></div><script>
const shadow = document.querySelector('#shadow').attachShadow({{mode:'open'}});
shadow.innerHTML = '<button id=shadow-button>Shadow action</button>';
document.querySelector('#focused').focus();
</script>
<iframe src=/same></iframe><iframe src="http://localhost:{oopif_port}/"></iframe>"#,
            "X".repeat(8_192)
        );
        let parent_server = tokio::spawn(serve_fixture(parent_listener, parent, child.clone()));
        let oopif_server = tokio::spawn(serve_fixture(oopif_listener, child, String::new()));
        let runtime = BrowserRuntime::launch(
            LaunchOptions::default()
                .headless(true)
                .arg("--site-per-process"),
        )
        .await
        .unwrap();
        let page = runtime
            .default_session()
            .await
            .unwrap()
            .new_page(format!("http://127.0.0.1:{parent_port}/"))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let frames = page.frames().await.unwrap();
                let routes = futures::future::join_all(frames.iter().map(Frame::cdp_session)).await;
                if frames.len() >= 3
                    && routes
                        .iter()
                        .filter_map(|route| route.as_ref().ok())
                        .map(cdpkit::Session::id)
                        .collect::<std::collections::BTreeSet<_>>()
                        .len()
                        >= 2
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("frame routes ready");

        let stale_main = page.main_frame().await.unwrap();
        let snapshot = page
            .snapshot(SnapshotOptions::default().with_max_elements(20))
            .await
            .unwrap();
        assert_eq!(snapshot.title, "Snapshot Fixture");
        assert!(snapshot.visible_text.contains("Hello world"));
        assert!(!snapshot.visible_text.contains('\n'));
        assert_eq!(snapshot.frames.len(), 2);
        let complex = snapshot
            .elements
            .iter()
            .find(|element| element.id.as_deref() == Some("complex"))
            .unwrap_or_else(|| {
                panic!(
                    "complex missing from {:?}",
                    snapshot
                        .elements
                        .iter()
                        .map(|element| (
                            &element.tag_name,
                            &element.id,
                            &element.accessibility.availability
                        ))
                        .collect::<Vec<_>>()
                )
            });
        assert_eq!(
            complex.accessibility.availability,
            AccessibilityAvailability::Available
        );
        assert_eq!(complex.accessibility.role.as_deref(), Some("switch"));
        assert_eq!(
            complex.accessibility.name.as_deref(),
            Some("Billing notifications")
        );
        assert_eq!(complex.accessibility.state.checked, Some(true));
        let implicit = snapshot
            .elements
            .iter()
            .find(|element| element.id.as_deref() == Some("implicit"))
            .unwrap();
        assert_eq!(implicit.accessibility.role.as_deref(), Some("button"));
        assert!(snapshot
            .elements
            .iter()
            .any(|element| element.id.as_deref() == Some("shadow-button")));
        assert_eq!(
            snapshot
                .focus
                .and_then(|index| snapshot.elements.get(index))
                .and_then(|element| element.id.as_deref()),
            Some("focused")
        );
        assert!(snapshot.frames.iter().all(|frame| frame
            .elements
            .iter()
            .any(|element| element.id.as_deref() == Some("frame-button"))));

        let region = page
            .locator("#complex")
            .snapshot(SnapshotOptions::default().with_max_elements(2))
            .await
            .unwrap();
        assert_eq!(
            region.accessibility.name.as_deref(),
            Some("Billing notifications")
        );
        let shadow_region = page
            .locator("#shadow")
            .snapshot(SnapshotOptions::default().with_max_elements(4))
            .await
            .unwrap();
        assert!(shadow_region
            .descendants
            .iter()
            .any(|element| element.id.as_deref() == Some("shadow-button")));
        let byte_bounded = page
            .snapshot(
                SnapshotOptions::default()
                    .with_max_bytes(1_024)
                    .with_max_elements(20),
            )
            .await
            .unwrap();
        assert!(serde_json::to_vec(&byte_bounded).unwrap().len() <= 1_024);
        assert_eq!(
            byte_bounded.truncation.emitted_bytes,
            serde_json::to_vec(&byte_bounded).unwrap().len()
        );
        Evaluate::new("document.title = 'T'.repeat(8192)")
            .send(page.cdp_session())
            .await
            .unwrap();
        let title_error = page
            .snapshot(SnapshotOptions::default().with_max_bytes(512))
            .await
            .unwrap_err();
        assert_eq!(title_error.phase(), OperationPhase::Preparation);
        assert!(title_error.to_string().contains("URL/title"));
        Evaluate::new("document.title = 'Snapshot Fixture'")
            .send(page.cdp_session())
            .await
            .unwrap();
        let truncated = page
            .snapshot(SnapshotOptions::default().with_max_elements(1))
            .await
            .unwrap();
        assert!(truncated.truncation.omitted_elements > 0);
        for child_frame in page
            .frames()
            .await
            .unwrap()
            .into_iter()
            .filter(|frame| frame.id() != stale_main.id())
        {
            let child_snapshot = child_frame
                .snapshot(SnapshotOptions::default())
                .await
                .unwrap();
            assert_eq!(
                child_snapshot.parent_id.as_deref(),
                Some(stale_main.id().as_str())
            );
        }
        cdpkit::page::methods::Navigate::new(format!(
            "http://127.0.0.1:{parent_port}/?replacement"
        ))
        .send(page.cdp_session())
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if stale_main
                    .snapshot(SnapshotOptions::default())
                    .await
                    .is_err()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("stale frame snapshot remained usable after document replacement");
        assert!(runtime.close().await.is_complete());
        parent_server.abort();
        oopif_server.abort();
    }
}
