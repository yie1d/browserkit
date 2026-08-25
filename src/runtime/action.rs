mod input;

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};

use cdpkit::dom::methods::{
    Focus, GetBoxModel, ResolveNode, ScrollIntoViewIfNeeded, SetFileInputFiles,
};
use cdpkit::dom::types::BackendNodeId;
use cdpkit::input::methods::DispatchMouseEvent;
use cdpkit::input::types::{DispatchMouseEventType, MouseButton};
use cdpkit::runtime::methods::{CallFunctionOn, ReleaseObjectGroup};
use cdpkit::runtime::types::CallArgument;
use serde_json::{json, Value};

use super::locator::resolver::ResolvedElement;
use super::{
    ActionCompletion, BrowserError, CleanupFailure, Frame, Locator, OperationPhase,
    OwnershipCleanupError, Page,
};

static NEXT_ACTION_GROUP: AtomicU64 = AtomicU64::new(1);

type SessionPoint = super::geometry::Point<super::geometry::SessionViewport>;
type FramePoint = super::geometry::Point<super::geometry::FrameViewport>;

#[derive(Clone, Copy)]
enum Gate {
    Actionable,
    Editable,
    Checkable,
    Uncheckable,
    Selectable,
    FileInput,
    Attached,
}

struct PreparedElement {
    locator: Locator,
    gate: Gate,
    page: Page,
    operation: super::page::PageOperation,
    session: cdpkit::Session,
    backend_node_id: BackendNodeId,
    route: super::frame::LocatorFrameRoute,
    checked: bool,
}

struct DispatchedElement {
    session: cdpkit::Session,
    backend_node_id: BackendNodeId,
    route: super::frame::LocatorFrameRoute,
    checked: bool,
}

struct ElementPoint {
    element: DispatchedElement,
    point: SessionPoint,
    fence: super::geometry::GeometryFence,
}

impl PreparedElement {
    async fn after_scroll(
        &self,
        operation: &'static str,
    ) -> Result<DispatchedElement, BrowserError> {
        ScrollIntoViewIfNeeded::new()
            .with_backend_node_id(self.backend_node_id)
            .send(&self.session)
            .await
            .map_err(|error| dispatched_error(operation, error))?;
        let resolved = self
            .locator
            .resolve_admitted(&self.operation)
            .await
            .map_err(mark_unknown)?;
        apply_gate(&resolved, self.gate).map_err(mark_unknown)?;
        Ok(DispatchedElement {
            session: resolved.session.clone(),
            backend_node_id: resolved.backend_node_id,
            route: resolved.route.clone(),
            checked: resolved.facts.checked,
        })
    }

    async fn point_after_dispatch(
        &self,
        operation: &'static str,
    ) -> Result<ElementPoint, BrowserError> {
        let element = self.after_scroll(operation).await?;
        let point = box_center(&element.session, element.backend_node_id, operation)
            .await
            .map_err(mark_unknown)?;
        let store = self.page.locator_frame_store(&self.operation).await?;
        let geometry = super::geometry::Geometry::for_route(&self.page, store, &element.route)?;
        Ok(ElementPoint {
            element,
            point,
            fence: geometry.route_fence(),
        })
    }

    async fn validate_dispatched(&self, sampled: &ElementPoint) -> Result<(), BrowserError> {
        self.locator.validate_document_for_action()?;
        sampled
            .fence
            .validate("confirm locator input geometry")
            .await
    }

    async fn validate_completed(
        &self,
        sampled: &ElementPoint,
        operation: &'static str,
    ) -> Result<(), BrowserError> {
        sampled.fence.validate(operation).await?;
        self.locator.validate_document_for_action()
    }

    async fn validate_route(&self) -> Result<(), BrowserError> {
        self.locator.validate_document_for_action()?;
        let store = self.page.locator_frame_store(&self.operation).await?;
        store
            .validate_locator_route_authoritative(&self.route)
            .await
            .map(|_| ())
    }
}

pub(super) async fn resolve_locator_after_scroll<'operation>(
    locator: &Locator,
    operation: &'operation super::page::PageOperation,
) -> Result<ResolvedElement<'operation>, BrowserError> {
    let (session, backend_node_id, route) = {
        let resolved = locator.resolve_admitted(operation).await?;
        apply_gate(&resolved, Gate::Attached)?;
        (
            resolved.session.clone(),
            resolved.backend_node_id,
            resolved.route.clone(),
        )
    };
    let store = locator.page().locator_frame_store(operation).await?;
    store.validate_locator_route_authoritative(&route).await?;
    ScrollIntoViewIfNeeded::new()
        .with_backend_node_id(backend_node_id)
        .send(&session)
        .await
        .map_err(|error| dispatched_error("scroll locator for screenshot", error))?;

    let resolved = locator.resolve_admitted(operation).await?;
    apply_gate(&resolved, Gate::Attached)?;
    store
        .validate_locator_route_authoritative(&resolved.route)
        .await?;
    Ok(resolved)
}

async fn prepare_locator(
    locator: &Locator,
    operation_name: &'static str,
    gate: Gate,
) -> Result<PreparedElement, BrowserError> {
    let page = locator.page_for_action().clone();
    let operation = page.admit_operation(operation_name)?;
    let (session, backend_node_id, route, checked) = {
        let resolved = locator.resolve_admitted(&operation).await?;
        apply_gate(&resolved, gate)?;
        (
            resolved.session.clone(),
            resolved.backend_node_id,
            resolved.route.clone(),
            resolved.facts.checked,
        )
    };
    let prepared = PreparedElement {
        locator: locator.clone(),
        gate,
        page: page.clone(),
        operation,
        session,
        backend_node_id,
        route,
        checked,
    };
    prepared.validate_route().await?;
    Ok(prepared)
}

fn apply_gate(resolved: &ResolvedElement<'_>, gate: Gate) -> Result<(), BrowserError> {
    match gate {
        Gate::Actionable => resolved.facts.ensure_actionable(),
        Gate::Editable => resolved.facts.ensure_editable(),
        Gate::Checkable => resolved.facts.ensure_checkable(),
        Gate::Uncheckable => resolved.facts.ensure_uncheckable(),
        Gate::Selectable => resolved.facts.ensure_selectable(),
        Gate::FileInput => resolved.facts.ensure_file_input(),
        Gate::Attached if !resolved.facts.attached => resolved.facts.ensure_actionable(),
        Gate::Attached => Ok(()),
    }
}

pub(crate) async fn locator_click(locator: &Locator, count: i64) -> Result<(), BrowserError> {
    let prepared = prepare_locator(locator, "click locator", Gate::Actionable).await?;
    spawn_dispatched(async move {
        let sampled = prepared.point_after_dispatch("prepare click").await?;
        prepared
            .validate_dispatched(&sampled)
            .await
            .map_err(mark_unknown)?;
        mouse_click(&sampled.element.session, sampled.point, count, true).await?;
        prepared
            .validate_completed(&sampled, "confirm locator click geometry")
            .await
            .map_err(mark_completed)
    })
    .await
}

pub(crate) async fn locator_hover(locator: &Locator) -> Result<(), BrowserError> {
    let prepared = prepare_locator(locator, "hover locator", Gate::Actionable).await?;
    spawn_dispatched(async move {
        let sampled = prepared.point_after_dispatch("prepare hover").await?;
        prepared
            .validate_dispatched(&sampled)
            .await
            .map_err(mark_unknown)?;
        mouse_move(&sampled.element.session, sampled.point, 0, true).await?;
        prepared
            .validate_completed(&sampled, "confirm locator hover geometry")
            .await
            .map_err(mark_completed)
    })
    .await
}

pub(crate) async fn locator_scroll(
    locator: &Locator,
    delta_x: f64,
    delta_y: f64,
) -> Result<(), BrowserError> {
    finite_pair(delta_x, delta_y, "scroll locator")?;
    let prepared = prepare_locator(locator, "scroll locator", Gate::Actionable).await?;
    spawn_dispatched(async move {
        let sampled = prepared.point_after_dispatch("prepare scroll").await?;
        prepared
            .validate_dispatched(&sampled)
            .await
            .map_err(mark_unknown)?;
        mouse_scroll(&sampled.element.session, sampled.point, delta_x, delta_y).await?;
        prepared
            .validate_completed(&sampled, "confirm locator scroll geometry")
            .await
            .map_err(mark_completed)
    })
    .await
}

pub(crate) async fn locator_scroll_into_view(locator: &Locator) -> Result<(), BrowserError> {
    let prepared = prepare_locator(locator, "scroll locator into view", Gate::Attached).await?;
    prepared.validate_route().await?;
    let session = prepared.session.clone();
    let node = prepared.backend_node_id;
    dispatched(prepared.operation, async move {
        ScrollIntoViewIfNeeded::new()
            .with_backend_node_id(node)
            .send(&session)
            .await
            .map_err(|error| dispatched_error("scroll locator into view", error))
    })
    .await
}

pub(crate) async fn locator_focus(locator: &Locator) -> Result<(), BrowserError> {
    let prepared = prepare_locator(locator, "focus locator", Gate::Attached).await?;
    spawn_dispatched(async move {
        prepared.validate_route().await?;
        Focus::new()
            .with_backend_node_id(prepared.backend_node_id)
            .send(&prepared.session)
            .await
            .map_err(|error| dispatched_error("focus locator", error))
    })
    .await
}

pub(crate) async fn locator_blur(locator: &Locator) -> Result<(), BrowserError> {
    call_element(
        locator,
        "blur locator",
        Gate::Attached,
        "function() { this.blur(); }",
        vec![],
    )
    .await
}

pub(crate) async fn locator_fill(locator: &Locator, value: &str) -> Result<(), BrowserError> {
    call_element(
        locator,
        "fill locator",
        Gate::Editable,
        FILL_FUNCTION,
        vec![argument(json!(value))],
    )
    .await
}

pub(crate) async fn locator_select<I, S>(locator: &Locator, values: I) -> Result<(), BrowserError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let values = values
        .into_iter()
        .map(|value| value.as_ref().to_owned())
        .collect::<Vec<_>>();
    call_element(
        locator,
        "select locator options",
        Gate::Selectable,
        SELECT_FUNCTION,
        vec![argument(json!(values))],
    )
    .await
}

pub(crate) async fn locator_type_text(locator: &Locator, value: &str) -> Result<(), BrowserError> {
    let prepared = prepare_locator(locator, "type text into locator", Gate::Editable).await?;
    let value = value.to_owned();
    spawn_dispatched(async move {
        Focus::new()
            .with_backend_node_id(prepared.backend_node_id)
            .send(&prepared.session)
            .await
            .map_err(|error| dispatched_error("focus before typing", error))?;
        prepared.validate_route().await.map_err(mark_unknown)?;
        let session = prepared.session.clone();
        input::type_text(&session, &value).await
    })
    .await
}

pub(crate) async fn locator_press(locator: &Locator, key: &str) -> Result<(), BrowserError> {
    input::parse_key(key)?;
    let prepared = prepare_locator(locator, "press key on locator", Gate::Actionable).await?;
    let key = key.to_owned();
    spawn_dispatched(async move {
        Focus::new()
            .with_backend_node_id(prepared.backend_node_id)
            .send(&prepared.session)
            .await
            .map_err(|error| dispatched_error("focus before key press", error))?;
        prepared.validate_route().await.map_err(mark_unknown)?;
        let session = prepared.session.clone();
        input::press(&session, &key).await
    })
    .await
}

pub(crate) async fn locator_set_checked(
    locator: &Locator,
    checked: bool,
) -> Result<(), BrowserError> {
    let gate = if checked {
        Gate::Checkable
    } else {
        Gate::Uncheckable
    };
    let prepared = prepare_locator(locator, "set locator checked state", gate).await?;
    if prepared.checked == checked {
        return Ok(());
    }
    spawn_dispatched(async move {
        let sampled = prepared
            .point_after_dispatch("prepare checked-state click")
            .await?;
        if sampled.element.checked == checked {
            return Ok(());
        }
        prepared
            .validate_dispatched(&sampled)
            .await
            .map_err(mark_unknown)?;
        mouse_click(&sampled.element.session, sampled.point, 1, true).await?;
        prepared
            .validate_completed(&sampled, "confirm checked-state click geometry")
            .await
            .map_err(mark_completed)
    })
    .await
}

pub(crate) async fn locator_set_input_files<I, S>(
    locator: &Locator,
    files: I,
) -> Result<(), BrowserError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let files = files
        .into_iter()
        .map(|file| file.as_ref().to_owned())
        .collect::<Vec<_>>();
    if files.iter().any(|file| file.is_empty()) {
        return Err(
            BrowserError::operation("set file input", OperationPhase::Preparation)
                .with_message("file input paths must not be empty"),
        );
    }
    let prepared = prepare_locator(locator, "set file input", Gate::FileInput).await?;
    prepared.validate_route().await?;
    let session = prepared.session.clone();
    let node = prepared.backend_node_id;
    dispatched(prepared.operation, async move {
        SetFileInputFiles::new(files)
            .with_backend_node_id(node)
            .send(&session)
            .await
            .map_err(|error| dispatched_error("set file input", error))
    })
    .await
}

pub(crate) async fn locator_drag_to(
    source: &Locator,
    target: &Locator,
) -> Result<(), BrowserError> {
    if source.page_for_action().target_id() != target.page_for_action().target_id() {
        return Err(
            BrowserError::operation("drag locator", OperationPhase::Preparation)
                .with_message("drag source and target must belong to the same page"),
        );
    }
    let page = source.page_for_action().clone();
    let operation = page.admit_operation("drag locator")?;
    let (session, source_id, target_id) = {
        let source_resolved = source.resolve_admitted(&operation).await?;
        source_resolved.facts.ensure_actionable()?;
        let target_resolved = target.resolve_admitted(&operation).await?;
        target_resolved.facts.ensure_actionable()?;
        if source_resolved.session.id() != target_resolved.session.id() {
            return Err(
                BrowserError::operation("drag locator", OperationPhase::Preparation).with_message(
                    "dragging across independently routed frame sessions is unsupported",
                ),
            );
        }
        (
            source_resolved.session.clone(),
            source_resolved.backend_node_id,
            target_resolved.backend_node_id,
        )
    };
    let source = source.clone();
    let target = target.clone();
    spawn_dispatched(async move {
        ScrollIntoViewIfNeeded::new()
            .with_backend_node_id(source_id)
            .send(&session)
            .await
            .map_err(|error| dispatched_error("prepare drag source", error))?;
        ScrollIntoViewIfNeeded::new()
            .with_backend_node_id(target_id)
            .send(&session)
            .await
            .map_err(|error| dispatched_error("prepare drag target", error))?;

        let source_resolved = source
            .resolve_admitted(&operation)
            .await
            .map_err(mark_unknown)?;
        source_resolved
            .facts
            .ensure_actionable()
            .map_err(mark_unknown)?;
        let target_resolved = target
            .resolve_admitted(&operation)
            .await
            .map_err(mark_unknown)?;
        target_resolved
            .facts
            .ensure_actionable()
            .map_err(mark_unknown)?;
        if source_resolved.session.id() != target_resolved.session.id() {
            return Err(mark_unknown(
                BrowserError::operation("drag locator", OperationPhase::Confirmation)
                    .with_message("drag routes changed to different frame sessions"),
            ));
        }
        let source_point = box_center(
            &source_resolved.session,
            source_resolved.backend_node_id,
            "prepare drag source",
        )
        .await
        .map_err(mark_unknown)?;
        let target_point = box_center(
            &target_resolved.session,
            target_resolved.backend_node_id,
            "prepare drag target",
        )
        .await
        .map_err(mark_unknown)?;
        source
            .validate_document_for_action()
            .map_err(mark_unknown)?;
        target
            .validate_document_for_action()
            .map_err(mark_unknown)?;
        let store = page
            .locator_frame_store(&operation)
            .await
            .map_err(mark_unknown)?;
        let source_fence =
            super::geometry::Geometry::for_route(&page, store, &source_resolved.route)
                .map_err(mark_unknown)?
                .route_fence();
        let target_fence =
            super::geometry::Geometry::for_route(&page, store, &target_resolved.route)
                .map_err(mark_unknown)?
                .route_fence();
        source_fence
            .validate("prepare drag source geometry")
            .await
            .map_err(mark_unknown)?;
        target_fence
            .validate("prepare drag target geometry")
            .await
            .map_err(mark_unknown)?;
        mouse_drag(&source_resolved.session, source_point, target_point).await?;
        source_fence
            .validate("confirm drag source geometry")
            .await
            .map_err(mark_completed)?;
        target_fence
            .validate("confirm drag target geometry")
            .await
            .map_err(mark_completed)
    })
    .await
}

async fn call_element(
    locator: &Locator,
    operation_name: &'static str,
    gate: Gate,
    function: &'static str,
    arguments: Vec<CallArgument>,
) -> Result<(), BrowserError> {
    let prepared = prepare_locator(locator, operation_name, gate).await?;
    let sequence = NEXT_ACTION_GROUP.fetch_add(1, Ordering::Relaxed);
    let object_group = format!("browserkit-action-{}-{sequence}", prepared.page.target_id());
    let release_session = prepared.session.clone();
    let release_group = object_group.clone();
    let cleanup = prepared
        .page
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
    let object = ResolveNode::new()
        .with_backend_node_id(prepared.backend_node_id)
        .with_object_group(object_group.clone())
        .send(&prepared.session)
        .await
        .map_err(|error| {
            BrowserError::cdp_operation(operation_name, OperationPhase::Observation, error)
        })?
        .object;
    let object_id = object.object_id.ok_or_else(|| {
        BrowserError::operation(operation_name, OperationPhase::Observation)
            .with_message("resolved action element did not expose an object id")
    })?;
    prepared.validate_route().await?;
    let session = prepared.session.clone();
    dispatched(prepared.operation, async move {
        let primary = CallFunctionOn::new(function)
            .with_object_id(object_id)
            .with_arguments(arguments)
            .with_user_gesture(true)
            .with_await_promise(true)
            .send(&session)
            .await
            .map_err(|error| dispatched_error(operation_name, error))
            .and_then(|response| {
                response.exception_details.map_or(Ok(()), |exception| {
                    Err(
                        BrowserError::operation(operation_name, OperationPhase::Dispatch)
                            .with_action_completion(ActionCompletion::Unknown)
                            .with_message(format!(
                                "element action raised JavaScript exception: {}",
                                exception.text
                            )),
                    )
                })
            });
        let released = cleanup.cleanup().await;
        match (primary, released) {
            (Ok(()), Ok(())) => Ok(()),
            (Ok(()), Err(error)) => Err(BrowserError::operation(
                operation_name,
                OperationPhase::Cleanup,
            )
            .with_action_completion(ActionCompletion::Completed)
            .with_message(format!(
                "action completed but object cleanup failed: {error}"
            ))
            .with_cleanup_failure(CleanupFailure::new(object_group, error.to_string()))),
            (Err(error), Ok(())) => Err(error),
            (Err(error), Err(cleanup_error)) => Err(error.with_cleanup_failure(
                CleanupFailure::new(object_group, cleanup_error.to_string()),
            )),
        }
    })
    .await
}

pub(crate) async fn page_press(page: &Page, key: &str) -> Result<(), BrowserError> {
    input::parse_key(key)?;
    let operation = page.admit_operation("press page key")?;
    let session = page.cdp_session().clone();
    let key = key.to_owned();
    dispatched(operation, async move { input::press(&session, &key).await }).await
}
pub(crate) async fn page_type_text(page: &Page, text: &str) -> Result<(), BrowserError> {
    let operation = page.admit_operation("type page text")?;
    let session = page.cdp_session().clone();
    let text = text.to_owned();
    dispatched(
        operation,
        async move { input::type_text(&session, &text).await },
    )
    .await
}
pub(crate) async fn page_move_pointer(page: &Page, x: f64, y: f64) -> Result<(), BrowserError> {
    finite_pair(x, y, "move page pointer")?;
    let point = SessionPoint::new(x, y, "move page pointer")?;
    let operation = page.admit_operation("move page pointer")?;
    let session = page.cdp_session().clone();
    dispatched(operation, async move {
        mouse_move(&session, point, 0, true).await
    })
    .await
}
pub(crate) async fn page_click_at(page: &Page, x: f64, y: f64) -> Result<(), BrowserError> {
    finite_pair(x, y, "click page point")?;
    let point = SessionPoint::new(x, y, "click page point")?;
    let operation = page.admit_operation("click page point")?;
    let session = page.cdp_session().clone();
    dispatched(operation, async move {
        mouse_click(&session, point, 1, true).await
    })
    .await
}
pub(crate) async fn page_scroll(page: &Page, dx: f64, dy: f64) -> Result<(), BrowserError> {
    finite_pair(dx, dy, "scroll page")?;
    let origin = SessionPoint::new(0.0, 0.0, "scroll page")?;
    let operation = page.admit_operation("scroll page")?;
    let session = page.cdp_session().clone();
    dispatched(operation, async move {
        mouse_scroll(&session, origin, dx, dy).await
    })
    .await
}

pub(crate) async fn frame_press(frame: &Frame, key: &str) -> Result<(), BrowserError> {
    input::parse_key(key)?;
    let operation = frame.page().admit_operation("press frame key")?;
    let geometry = super::geometry::Geometry::for_frame(frame, &operation).await?;
    let session = geometry.session();
    let fence = geometry.route_fence();
    fence.validate("prepare frame key route").await?;
    let key = key.to_owned();
    dispatched(operation, async move {
        input::press(&session, &key).await?;
        fence
            .validate("confirm frame key route")
            .await
            .map_err(mark_completed)
    })
    .await
}
pub(crate) async fn frame_type_text(frame: &Frame, text: &str) -> Result<(), BrowserError> {
    let operation = frame.page().admit_operation("type frame text")?;
    let geometry = super::geometry::Geometry::for_frame(frame, &operation).await?;
    let session = geometry.session();
    let fence = geometry.route_fence();
    fence.validate("prepare frame text route").await?;
    let text = text.to_owned();
    dispatched(operation, async move {
        input::type_text(&session, &text).await?;
        fence
            .validate("confirm frame text route")
            .await
            .map_err(mark_completed)
    })
    .await
}
pub(crate) async fn frame_move_pointer(frame: &Frame, x: f64, y: f64) -> Result<(), BrowserError> {
    finite_pair(x, y, "move frame pointer")?;
    let operation = frame.page().admit_operation("move frame pointer")?;
    let geometry = super::geometry::Geometry::for_frame(frame, &operation).await?;
    let mapped = geometry
        .map_frame_point_to_session(
            FramePoint::new(x, y, "move frame pointer")?,
            "map frame pointer",
        )
        .await?;
    mapped.fence.validate("prepare frame pointer").await?;
    let session = geometry.session();
    dispatched(operation, async move {
        mouse_move(&session, mapped.point, 0, true).await?;
        mapped
            .fence
            .validate("confirm frame pointer")
            .await
            .map_err(mark_completed)
    })
    .await
}
pub(crate) async fn frame_click_at(frame: &Frame, x: f64, y: f64) -> Result<(), BrowserError> {
    finite_pair(x, y, "click frame point")?;
    let operation = frame.page().admit_operation("click frame point")?;
    let geometry = super::geometry::Geometry::for_frame(frame, &operation).await?;
    let mapped = geometry
        .map_frame_point_to_session(
            FramePoint::new(x, y, "click frame point")?,
            "map frame click point",
        )
        .await?;
    mapped.fence.validate("prepare frame click point").await?;
    let session = geometry.session();
    dispatched(operation, async move {
        mouse_click(&session, mapped.point, 1, true).await?;
        mapped
            .fence
            .validate("confirm frame click point")
            .await
            .map_err(mark_completed)
    })
    .await
}
pub(crate) async fn frame_scroll(frame: &Frame, dx: f64, dy: f64) -> Result<(), BrowserError> {
    finite_pair(dx, dy, "scroll frame")?;
    let operation = frame.page().admit_operation("scroll frame")?;
    let geometry = super::geometry::Geometry::for_frame(frame, &operation).await?;
    let mapped = geometry
        .map_frame_point_to_session(
            FramePoint::new(1.0, 1.0, "scroll frame")?,
            "map frame scroll origin",
        )
        .await?;
    mapped.fence.validate("prepare frame scroll origin").await?;
    let session = geometry.session();
    dispatched(operation, async move {
        mouse_scroll(&session, mapped.point, dx, dy).await?;
        mapped
            .fence
            .validate("confirm frame scroll origin")
            .await
            .map_err(mark_completed)
    })
    .await
}

async fn dispatched<F>(operation: super::page::PageOperation, future: F) -> Result<(), BrowserError>
where
    F: Future<Output = Result<(), BrowserError>> + Send + 'static,
{
    spawn_dispatched(async move {
        let _operation = operation;
        future.await
    })
    .await
}

async fn spawn_dispatched<F>(future: F) -> Result<(), BrowserError>
where
    F: Future<Output = Result<(), BrowserError>> + Send + 'static,
{
    tokio::spawn(future).await.map_err(|error| {
        BrowserError::operation("join dispatched browser action", OperationPhase::Dispatch)
            .with_action_completion(ActionCompletion::Unknown)
            .with_message(format!("dispatched browser action task failed: {error}"))
    })?
}

async fn mouse_click(
    session: &cdpkit::Session,
    point: SessionPoint,
    count: i64,
    action_started: bool,
) -> Result<(), BrowserError> {
    mouse_move(session, point, 0, action_started).await?;
    for click_count in 1..=count {
        let pressed =
            DispatchMouseEvent::new(DispatchMouseEventType::MousePressed, point.x(), point.y())
                .with_button(MouseButton::Left)
                .with_buttons(1)
                .with_click_count(click_count)
                .send(session)
                .await
                .map_err(|error| dispatched_error("dispatch mouse press", error));
        if let Err(primary) = pressed {
            return release_after_primary_failure(session, point, primary).await;
        }
        release_mouse(session, point, "dispatch mouse release").await?;
    }
    Ok(())
}
async fn mouse_move(
    session: &cdpkit::Session,
    point: SessionPoint,
    buttons: i64,
    action_started: bool,
) -> Result<(), BrowserError> {
    let button = if buttons & 1 == 1 {
        MouseButton::Left
    } else {
        MouseButton::None
    };
    DispatchMouseEvent::new(DispatchMouseEventType::MouseMoved, point.x(), point.y())
        .with_button(button)
        .with_buttons(buttons)
        .send(session)
        .await
        .map_err(|error| {
            let completion = if action_started {
                ActionCompletion::Unknown
            } else {
                ActionCompletion::NotStarted
            };
            BrowserError::cdp_operation("dispatch mouse move", OperationPhase::Dispatch, error)
                .with_action_completion(completion)
        })
}
async fn mouse_scroll(
    session: &cdpkit::Session,
    point: SessionPoint,
    dx: f64,
    dy: f64,
) -> Result<(), BrowserError> {
    DispatchMouseEvent::new(DispatchMouseEventType::MouseWheel, point.x(), point.y())
        .with_delta_x(dx)
        .with_delta_y(dy)
        .send(session)
        .await
        .map_err(|error| dispatched_error("dispatch mouse wheel", error))
}
async fn mouse_drag(
    session: &cdpkit::Session,
    source: SessionPoint,
    target: SessionPoint,
) -> Result<(), BrowserError> {
    mouse_move(session, source, 0, true).await?;
    let pressed =
        DispatchMouseEvent::new(DispatchMouseEventType::MousePressed, source.x(), source.y())
            .with_button(MouseButton::Left)
            .with_buttons(1)
            .with_click_count(1)
            .send(session)
            .await
            .map_err(|error| dispatched_error("dispatch drag press", error));
    if let Err(primary) = pressed {
        return release_after_primary_failure(session, source, primary).await;
    }
    let movement = async {
        for step in 1..=10 {
            let progress = f64::from(step) / 10.0;
            mouse_move(
                session,
                SessionPoint::new(
                    source.x() + (target.x() - source.x()) * progress,
                    source.y() + (target.y() - source.y()) * progress,
                    "dispatch drag movement",
                )?,
                1,
                true,
            )
            .await?;
        }
        Ok::<(), BrowserError>(())
    }
    .await;
    if let Err(primary) = movement {
        return release_after_primary_failure(session, target, primary).await;
    }
    release_mouse(session, target, "dispatch drag release").await
}

async fn release_mouse(
    session: &cdpkit::Session,
    point: SessionPoint,
    operation: &'static str,
) -> Result<(), BrowserError> {
    DispatchMouseEvent::new(DispatchMouseEventType::MouseReleased, point.x(), point.y())
        .with_button(MouseButton::Left)
        .with_buttons(0)
        .with_click_count(1)
        .send(session)
        .await
        .map_err(|error| dispatched_error(operation, error))
}

async fn release_after_primary_failure(
    session: &cdpkit::Session,
    point: SessionPoint,
    primary: BrowserError,
) -> Result<(), BrowserError> {
    match release_mouse(session, point, "release mouse after failed press or drag").await {
        Ok(()) => Err(primary),
        Err(cleanup) => {
            Err(primary
                .with_cleanup_failure(CleanupFailure::new("mouse button", cleanup.to_string())))
        }
    }
}
async fn box_center(
    session: &cdpkit::Session,
    node: BackendNodeId,
    operation: &'static str,
) -> Result<SessionPoint, BrowserError> {
    let border = GetBoxModel::new()
        .with_backend_node_id(node)
        .send(session)
        .await
        .map_err(|error| {
            BrowserError::cdp_operation(operation, OperationPhase::Observation, error)
        })?
        .model
        .border;
    super::geometry::Quad::<super::geometry::SessionViewport>::try_from_slice(&border, operation)?
        .center(operation)
}

#[cfg(test)]
fn center_of_quad(quad: &[f64]) -> Option<(f64, f64)> {
    super::geometry::Quad::<super::geometry::SessionViewport>::try_from_slice(
        quad,
        "resolve quad center",
    )
    .and_then(|quad| quad.center("resolve quad center"))
    .ok()
    .map(|point| (point.x(), point.y()))
}
fn finite_pair(x: f64, y: f64, operation: &'static str) -> Result<(), BrowserError> {
    if x.is_finite() && y.is_finite() {
        Ok(())
    } else {
        Err(
            BrowserError::operation(operation, OperationPhase::Preparation)
                .with_message("coordinates and deltas must be finite"),
        )
    }
}
fn dispatched_error(operation: &'static str, error: cdpkit::CdpError) -> BrowserError {
    BrowserError::cdp_operation(operation, OperationPhase::Dispatch, error)
        .with_action_completion(ActionCompletion::Unknown)
}
fn mark_unknown(error: BrowserError) -> BrowserError {
    error.with_action_completion(ActionCompletion::Unknown)
}
fn mark_completed(error: BrowserError) -> BrowserError {
    error.with_action_completion(ActionCompletion::Completed)
}
fn argument(value: Value) -> CallArgument {
    CallArgument {
        value: Some(value),
        unserializable_value: None,
        object_id: None,
    }
}

const FILL_FUNCTION: &str = r#"function(value) {
  this.focus();
  if (this.isContentEditable) { this.textContent = value; } else {
    const owner = this.localName === 'textarea' ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    Object.getOwnPropertyDescriptor(owner, 'value').set.call(this, value);
  }
  this.dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'insertText', data: value }));
  this.dispatchEvent(new Event('change', { bubbles: true }));
}"#;
const SELECT_FUNCTION: &str = r#"function(values) {
  const available = Array.from(this.options);
  const missing = values.filter(value => !available.some(option => option.value === value || option.label === value));
  if (missing.length) throw new Error(`select options not found: ${missing.join(', ')}`);
  const wanted = new Set(values);
  for (const option of this.options) option.selected = wanted.has(option.value) || wanted.has(option.label);
  this.dispatchEvent(new Event('input', { bubbles: true }));
  this.dispatchEvent(new Event('change', { bubbles: true }));
}"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::locator::resolver::tests::{
        page_for, serve_live_locator_fixture, success_fixture,
    };
    #[test]
    fn quad_center_uses_css_viewport_coordinates_without_dpr_scaling() {
        assert_eq!(
            center_of_quad(&[10.0, 20.0, 30.0, 20.0, 30.0, 40.0, 10.0, 40.0]),
            Some((20.0, 30.0))
        );
        assert_eq!(center_of_quad(&[0.0, 1.0]), None);
        assert_eq!(
            center_of_quad(&[0.0, 0.0, f64::NAN, 0.0, 1.0, 1.0, 0.0, 1.0]),
            None
        );
    }

    #[tokio::test]
    async fn click_uses_the_resolved_border_center_and_dispatches_once_in_order() {
        let (page, commands, _) = page_for(success_fixture()).await;
        page.locator("#save").click().await.unwrap();

        let commands = commands.lock();
        let mouse = commands
            .iter()
            .filter(|command| command["method"] == "Input.dispatchMouseEvent")
            .collect::<Vec<_>>();
        assert_eq!(mouse.len(), 3);
        assert_eq!(mouse[0]["params"]["type"], "mouseMoved");
        assert_eq!(mouse[1]["params"]["type"], "mousePressed");
        assert_eq!(mouse[2]["params"]["type"], "mouseReleased");
        assert_eq!(mouse[1]["params"]["x"], 20.0);
        assert_eq!(mouse[1]["params"]["y"], 30.0);
    }

    #[tokio::test]
    async fn failure_after_scroll_but_before_mouse_press_is_unknown_and_never_retried() {
        let mut fixture = success_fixture();
        fixture.command_error = Some(("Input.dispatchMouseEvent", -32000, "target closed"));
        let (page, commands, _) = page_for(fixture).await;
        let error = page.locator("#save").click().await.unwrap_err();
        assert_eq!(error.action_completed(), ActionCompletion::Unknown);
        assert_eq!(
            commands
                .lock()
                .iter()
                .filter(|command| command["method"] == "Input.dispatchMouseEvent")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn first_state_changing_dispatch_failure_is_unknown_and_never_retried() {
        let mut fixture = success_fixture();
        fixture.command_error = Some(("Input.dispatchMouseEvent", -32000, "target closed"));
        fixture.command_error_occurrence = 2;
        let (page, commands, _) = page_for(fixture).await;
        let error = page.locator("#save").click().await.unwrap_err();
        assert_eq!(error.action_completed(), ActionCompletion::Unknown);
        let commands = commands.lock();
        let mouse = commands
            .iter()
            .filter(|command| command["method"] == "Input.dispatchMouseEvent")
            .collect::<Vec<_>>();
        assert_eq!(mouse.len(), 3);
        assert_eq!(mouse[2]["params"]["type"], "mouseReleased");
        assert_eq!(mouse[2]["params"]["buttons"], 0);
    }

    #[tokio::test]
    async fn fill_uses_native_value_semantics_and_cleanup_failure_keeps_completed() {
        let mut fixture = success_fixture();
        fixture.facts["editable"] = json!(true);
        fixture.command_error = Some(("Runtime.releaseObjectGroup", -32000, "release failed"));
        fixture.command_error_occurrence = 2;
        let (page, commands, _) = page_for(fixture).await;

        let error = page.locator("#field").fill("new value").await.unwrap_err();
        assert_eq!(error.action_completed(), ActionCompletion::Completed);
        assert_eq!(error.cleanup_failures().len(), 1);
        let commands = commands.lock();
        let calls = commands
            .iter()
            .filter(|command| command["method"] == "Runtime.callFunctionOn")
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 2);
        assert!(calls[1]["params"]["functionDeclaration"]
            .as_str()
            .unwrap()
            .contains("InputEvent('input'"));
        assert_eq!(calls[1]["params"]["arguments"][0]["value"], "new value");
    }

    #[tokio::test]
    async fn type_text_focuses_then_dispatches_keydown_and_keyup_per_character() {
        let mut fixture = success_fixture();
        fixture.facts["editable"] = json!(true);
        let (page, commands, _) = page_for(fixture).await;
        page.locator("#field").type_text("ab").await.unwrap();

        let commands = commands.lock();
        let focus = commands
            .iter()
            .position(|command| command["method"] == "DOM.focus")
            .unwrap();
        let keys = commands
            .iter()
            .enumerate()
            .filter(|(_, command)| command["method"] == "Input.dispatchKeyEvent")
            .collect::<Vec<_>>();
        assert_eq!(keys.len(), 4);
        assert!(focus < keys[0].0);
        assert_eq!(
            keys.iter()
                .map(|(_, command)| command["params"]["type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["keyDown", "keyUp", "keyDown", "keyUp"]
        );
    }

    #[tokio::test]
    async fn select_validates_every_requested_option_before_mutating_the_control() {
        let mut fixture = success_fixture();
        fixture.facts["selectable"] = json!(true);
        let (page, commands, _) = page_for(fixture).await;
        page.locator("#choice")
            .select(["known", "missing"])
            .await
            .unwrap();

        let commands = commands.lock();
        let call = commands
            .iter()
            .filter(|command| command["method"] == "Runtime.callFunctionOn")
            .nth(1)
            .expect("select action call");
        let function = call["params"]["functionDeclaration"].as_str().unwrap();
        assert!(function.contains("missing"));
        assert!(function.contains("throw"));
        assert_eq!(
            call["params"]["arguments"][0]["value"],
            json!(["known", "missing"])
        );
    }

    #[tokio::test]
    async fn uncheck_rejects_a_radio_before_dispatch() {
        let mut fixture = success_fixture();
        fixture.facts["checkable"] = json!(true);
        fixture.facts["checked"] = json!(true);
        fixture.facts["radio"] = json!(true);
        let (page, commands, _) = page_for(fixture).await;

        let error = page.locator("#radio").uncheck().await.unwrap_err();
        assert_eq!(
            error.locator_failure(),
            Some(&crate::runtime::LocatorFailure::NotUncheckable)
        );
        assert_eq!(error.action_completed(), ActionCompletion::NotStarted);
        assert!(!commands
            .lock()
            .iter()
            .any(|command| command["method"] == "Input.dispatchMouseEvent"));
    }

    #[tokio::test]
    async fn file_input_is_strict_and_dispatches_only_for_confirmed_file_controls() {
        let mut fixture = success_fixture();
        fixture.facts["file_input"] = json!(true);
        let (page, commands, _) = page_for(fixture).await;
        page.locator("#file")
            .set_input_files(["C:/fixtures/a.txt"])
            .await
            .unwrap();
        {
            let commands = commands.lock();
            let set_files = commands
                .iter()
                .find(|command| command["method"] == "DOM.setFileInputFiles")
                .expect("file input command");
            assert_eq!(set_files["params"]["files"], json!(["C:/fixtures/a.txt"]));
        }

        let (page, commands, _) = page_for(success_fixture()).await;
        let error = page
            .locator("#not-file")
            .set_input_files(["C:/fixtures/a.txt"])
            .await
            .unwrap_err();
        assert_eq!(
            error.locator_failure(),
            Some(&crate::runtime::LocatorFailure::NotFileInput)
        );
        assert!(!commands
            .lock()
            .iter()
            .any(|command| command["method"] == "DOM.setFileInputFiles"));
    }

    #[tokio::test]
    async fn drag_dispatches_pressed_moves_and_marks_post_press_failure_unknown() {
        let mut fixture = success_fixture();
        fixture.command_error = Some(("Input.dispatchMouseEvent", -32000, "target closed"));
        fixture.command_error_occurrence = 3;
        let (page, commands, _) = page_for(fixture).await;

        let error = page
            .locator("#source")
            .drag_to(&page.locator("#target"))
            .await
            .unwrap_err();
        assert_eq!(error.action_completed(), ActionCompletion::Unknown);
        let commands = commands.lock();
        let mouse = commands
            .iter()
            .filter(|command| command["method"] == "Input.dispatchMouseEvent")
            .collect::<Vec<_>>();
        assert_eq!(mouse.len(), 4);
        assert_eq!(mouse[0]["params"]["type"], "mouseMoved");
        assert_eq!(mouse[1]["params"]["type"], "mousePressed");
        assert_eq!(mouse[2]["params"]["type"], "mouseMoved");
        assert_eq!(mouse[2]["params"]["button"], "left");
        assert_eq!(mouse[2]["params"]["buttons"], 1);
        assert_eq!(mouse[3]["params"]["type"], "mouseReleased");
        assert_eq!(mouse[3]["params"]["buttons"], 0);
    }

    #[tokio::test]
    async fn drag_preserves_primary_failure_when_release_cleanup_also_fails() {
        let mut fixture = success_fixture();
        fixture.command_error = Some(("Input.dispatchMouseEvent", -32000, "target closed"));
        fixture.command_error_occurrence = 3;
        fixture.command_error_additional_occurrence = Some(4);
        let (page, _, _) = page_for(fixture).await;

        let error = page
            .locator("#source")
            .drag_to(&page.locator("#target"))
            .await
            .unwrap_err();
        assert_eq!(error.action_completed(), ActionCompletion::Unknown);
        assert_eq!(error.cleanup_failures().len(), 1);
        assert_eq!(error.cleanup_failures()[0].resource(), "mouse button");
        assert!(std::error::Error::source(&error).is_some());
    }

    #[tokio::test]
    async fn drag_press_failure_also_attempts_one_release() {
        let mut fixture = success_fixture();
        fixture.command_error = Some(("Input.dispatchMouseEvent", -32000, "target closed"));
        fixture.command_error_occurrence = 2;
        let (page, commands, _) = page_for(fixture).await;

        let error = page
            .locator("#source")
            .drag_to(&page.locator("#target"))
            .await
            .unwrap_err();
        assert_eq!(error.action_completed(), ActionCompletion::Unknown);
        let commands = commands.lock();
        let mouse = commands
            .iter()
            .filter(|command| command["method"] == "Input.dispatchMouseEvent")
            .collect::<Vec<_>>();
        assert_eq!(mouse.len(), 3);
        assert_eq!(mouse[1]["params"]["type"], "mousePressed");
        assert_eq!(mouse[2]["params"]["type"], "mouseReleased");
    }

    #[tokio::test]
    async fn direct_click_first_pointer_dispatch_failure_is_unknown() {
        let mut fixture = success_fixture();
        fixture.command_error = Some(("Input.dispatchMouseEvent", -32000, "target closed"));
        let (page, commands, _) = page_for(fixture).await;

        let error = page.click_at(1.0, 2.0).await.unwrap_err();
        assert_eq!(error.action_completed(), ActionCompletion::Unknown);
        assert_eq!(
            commands
                .lock()
                .iter()
                .filter(|command| command["method"] == "Input.dispatchMouseEvent")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn cancellation_before_dispatch_never_starts_input() {
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let mut fixture = success_fixture();
        fixture.stall_method = Some("Runtime.evaluate");
        fixture.stall_release = Some(std::sync::Arc::clone(&release));
        fixture.stall_started = Some(std::sync::Arc::clone(&started));
        let (page, commands, _) = page_for(fixture).await;

        let acting_page = page.clone();
        let action = tokio::spawn(async move { acting_page.locator("#save").click().await });
        started.notified().await;
        action.abort();
        release.notify_one();
        let _ = action.await;
        tokio::task::yield_now().await;

        assert!(!commands
            .lock()
            .iter()
            .any(|command| command["method"] == "Input.dispatchMouseEvent"));
    }

    #[tokio::test]
    async fn document_replacement_during_preparation_fails_closed_without_input() {
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let mut fixture = success_fixture();
        fixture.stall_method = Some("DOM.getBoxModel");
        fixture.stall_release = Some(std::sync::Arc::clone(&release));
        fixture.stall_started = Some(std::sync::Arc::clone(&started));
        let (page, commands, _) = page_for(fixture).await;

        let acting_page = page.clone();
        let action = tokio::spawn(async move { acting_page.locator("#save").click().await });
        started.notified().await;
        page.lifecycle().commit_new_document();
        release.notify_one();
        let error = action.await.unwrap().unwrap_err();

        assert_eq!(error.action_completed(), ActionCompletion::Unknown);
        assert!(error.to_string().contains("stale"));
        assert!(!commands
            .lock()
            .iter()
            .any(|command| command["method"] == "Input.dispatchMouseEvent"));
    }

    #[tokio::test]
    async fn document_replacement_after_click_dispatch_is_completed_and_never_retried() {
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let mut fixture = success_fixture();
        fixture.stall_method = Some("Input.dispatchMouseEvent");
        fixture.stall_occurrence = 3;
        fixture.stall_release = Some(std::sync::Arc::clone(&release));
        fixture.stall_started = Some(std::sync::Arc::clone(&started));
        let (page, commands, _) = page_for(fixture).await;

        let acting_page = page.clone();
        let action = tokio::spawn(async move { acting_page.locator("#save").click().await });
        started.notified().await;
        page.lifecycle().commit_new_document();
        release.notify_one();
        let error = action.await.unwrap().unwrap_err();

        assert_eq!(error.action_completed(), ActionCompletion::Completed);
        assert_eq!(
            commands
                .lock()
                .iter()
                .filter(|command| command["method"] == "Input.dispatchMouseEvent")
                .count(),
            3,
            "post-dispatch geometry changes must not retry input",
        );
    }

    #[tokio::test]
    async fn direct_page_primitives_use_the_same_single_dispatch_path() {
        let (page, commands, _) = page_for(success_fixture()).await;
        page.move_pointer(1.0, 2.0).await.unwrap();
        page.click_at(3.0, 4.0).await.unwrap();
        page.scroll(5.0, 6.0).await.unwrap();
        page.press("Control+A").await.unwrap();
        page.type_text("x").await.unwrap();

        let commands = commands.lock();
        assert_eq!(
            commands
                .iter()
                .filter(|command| command["method"] == "Input.dispatchMouseEvent")
                .count(),
            5
        );
        assert_eq!(
            commands
                .iter()
                .filter(|command| command["method"] == "Input.dispatchKeyEvent")
                .count(),
            4
        );
        let mouse = commands
            .iter()
            .filter(|command| command["method"] == "Input.dispatchMouseEvent")
            .collect::<Vec<_>>();
        assert_eq!(mouse[0]["params"]["type"], "mouseMoved");
        assert_eq!(mouse[0]["params"]["x"], 1.0);
        assert_eq!(mouse[0]["params"]["y"], 2.0);
        assert_eq!(mouse[2]["params"]["x"], 3.0);
        assert_eq!(mouse[2]["params"]["y"], 4.0);
        assert_eq!(mouse[4]["params"]["type"], "mouseWheel");
        assert_eq!(mouse[4]["params"]["deltaX"], 5.0);
        assert_eq!(mouse[4]["params"]["deltaY"], 6.0);
        let keys = commands
            .iter()
            .filter(|command| command["method"] == "Input.dispatchKeyEvent")
            .collect::<Vec<_>>();
        assert_eq!(keys[0]["params"]["modifiers"], 2);
        assert!(keys[0]["params"].get("text").is_none());
        assert_eq!(keys[2]["params"]["text"], "x");
    }

    #[tokio::test]
    async fn direct_frame_primitives_dispatch_on_the_frame_route() {
        let (page, commands, _) = page_for(success_fixture()).await;
        let frame = page.main_frame().await.unwrap();
        frame.move_pointer(11.0, 12.0).await.unwrap();
        frame.click_at(13.0, 14.0).await.unwrap();
        frame.scroll(15.0, 16.0).await.unwrap();
        frame.press("Meta+ArrowLeft").await.unwrap();
        frame.type_text("z").await.unwrap();

        let commands = commands.lock();
        let input = commands
            .iter()
            .filter(|command| {
                command["method"] == "Input.dispatchMouseEvent"
                    || command["method"] == "Input.dispatchKeyEvent"
            })
            .collect::<Vec<_>>();
        assert!(!input.is_empty());
        assert!(input
            .iter()
            .all(|command| command["sessionId"] == "frame-session"));
        let wheel = input
            .iter()
            .find(|command| command["params"]["type"] == "mouseWheel")
            .unwrap();
        assert_eq!(wheel["params"]["x"], 1.0);
        assert_eq!(wheel["params"]["y"], 1.0);
        assert_eq!(wheel["params"]["deltaX"], 15.0);
        assert_eq!(wheel["params"]["deltaY"], 16.0);
        let meta = input
            .iter()
            .find(|command| command["params"]["key"] == "ArrowLeft")
            .unwrap();
        assert_eq!(meta["params"]["modifiers"], 4);
        assert!(meta["params"].get("text").is_none());
    }

    #[tokio::test]
    async fn locator_auxiliary_actions_use_expected_protocol_primitives() {
        let (page, commands, _) = page_for(success_fixture()).await;
        let locator = page.locator("#target");
        locator.hover().await.unwrap();
        locator.focus().await.unwrap();
        locator.blur().await.unwrap();
        locator.scroll(7.0, 8.0).await.unwrap();
        locator.scroll_into_view().await.unwrap();
        locator.press("Alt+Shift+ArrowDown").await.unwrap();

        let commands = commands.lock();
        assert!(commands.iter().any(|command| {
            command["method"] == "Input.dispatchMouseEvent"
                && command["params"]["type"] == "mouseMoved"
                && command["params"]["x"] == 20.0
                && command["params"]["y"] == 30.0
        }));
        assert!(commands.iter().any(|command| {
            command["method"] == "Input.dispatchMouseEvent"
                && command["params"]["type"] == "mouseWheel"
                && command["params"]["deltaX"] == 7.0
                && command["params"]["deltaY"] == 8.0
        }));
        assert!(commands
            .iter()
            .any(|command| command["method"] == "DOM.focus"));
        assert!(commands.iter().any(|command| {
            command["method"] == "Runtime.callFunctionOn"
                && command["params"]["functionDeclaration"]
                    .as_str()
                    .is_some_and(|function| function.contains("this.blur()"))
        }));
        assert!(
            commands
                .iter()
                .filter(|command| command["method"] == "DOM.scrollIntoViewIfNeeded")
                .count()
                >= 3
        );
        let keys = commands
            .iter()
            .filter(|command| command["method"] == "Input.dispatchKeyEvent")
            .collect::<Vec<_>>();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0]["params"]["modifiers"], 1 | 8);
        assert_eq!(keys[0]["params"]["key"], "ArrowDown");
        assert!(keys[0]["params"].get("text").is_none());
    }

    #[tokio::test]
    async fn auxiliary_dispatch_failures_are_unknown_and_never_retried() {
        for action in ["hover", "focus", "scroll", "scroll_into_view"] {
            let mut fixture = success_fixture();
            fixture.command_error = Some(match action {
                "focus" => ("DOM.focus", -32000, "target closed"),
                "scroll_into_view" => ("DOM.scrollIntoViewIfNeeded", -32000, "target closed"),
                _ => ("Input.dispatchMouseEvent", -32000, "target closed"),
            });
            let (page, commands, _) = page_for(fixture.clone()).await;
            let locator = page.locator("#target");
            let error = match action {
                "hover" => locator.hover().await.unwrap_err(),
                "focus" => locator.focus().await.unwrap_err(),
                "scroll" => locator.scroll(1.0, 2.0).await.unwrap_err(),
                "scroll_into_view" => locator.scroll_into_view().await.unwrap_err(),
                _ => unreachable!(),
            };
            assert_eq!(
                error.action_completed(),
                ActionCompletion::Unknown,
                "{action}"
            );
            let method = fixture.command_error.unwrap().0;
            assert_eq!(
                commands
                    .lock()
                    .iter()
                    .filter(|command| command["method"] == method)
                    .count(),
                1,
                "{action}"
            );
        }
    }

    #[tokio::test]
    async fn cancellation_during_javascript_action_continues_cleanup_and_close_waits() {
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let mut fixture = success_fixture();
        fixture.facts["editable"] = json!(true);
        fixture.stall_method = Some("Runtime.callFunctionOn");
        fixture.stall_occurrence = 2;
        fixture.stall_release = Some(std::sync::Arc::clone(&release));
        fixture.stall_started = Some(std::sync::Arc::clone(&started));
        let (page, commands, _) = page_for(fixture).await;

        let acting_page = page.clone();
        let action = tokio::spawn(async move { acting_page.locator("#field").fill("x").await });
        started.notified().await;
        action.abort();
        let closing_page = page.clone();
        let close = tokio::spawn(async move { closing_page.close().await });
        tokio::task::yield_now().await;
        assert!(!close.is_finished());
        release.notify_one();
        assert!(close.await.unwrap().is_complete());
        assert_eq!(
            commands
                .lock()
                .iter()
                .filter(|command| command["method"] == "Runtime.releaseObjectGroup")
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn cancellation_after_dispatch_does_not_abort_or_repeat_and_close_waits() {
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let dispatch_started = std::sync::Arc::new(tokio::sync::Notify::new());
        let mut fixture = success_fixture();
        fixture.stall_method = Some("Input.dispatchMouseEvent");
        fixture.stall_release = Some(std::sync::Arc::clone(&release));
        fixture.stall_started = Some(std::sync::Arc::clone(&dispatch_started));
        let (page, commands, _) = page_for(fixture).await;

        let acting_page = page.clone();
        let action = tokio::spawn(async move { acting_page.locator("#save").click().await });
        dispatch_started.notified().await;
        action.abort();

        let closing_page = page.clone();
        let close = tokio::spawn(async move { closing_page.close().await });
        tokio::task::yield_now().await;
        assert!(
            !close.is_finished(),
            "close must wait for the dispatched action permit"
        );
        release.notify_one();
        assert!(close.await.unwrap().is_complete());
        assert_eq!(
            commands
                .lock()
                .iter()
                .filter(|command| command["method"] == "Input.dispatchMouseEvent")
                .count(),
            3
        );
    }

    #[tokio::test]
    #[ignore = "requires installed Chrome and loopback sockets"]
    async fn live_chrome_actions_cover_main_frame_iframes_shadow_dom_and_file_input() {
        use crate::runtime::{BrowserRuntime, LaunchOptions};
        use cdpkit::runtime::methods::Evaluate;
        use std::time::Duration;

        let child_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let child_port = child_listener.local_addr().unwrap().port();
        let parent_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let parent_port = parent_listener.local_addr().unwrap().port();
        let frame_body = r#"<!doctype html><button id='frame-button' onclick='document.body.dataset.clicked=String(Number(document.body.dataset.clicked||0)+1)'>frame</button>"#.to_owned();
        let parent_body = format!(
            r#"<!doctype html>
<style>body {{ min-height: 1200px }} button,input,select,#drag,#drop {{ margin:8px; width:120px; height:32px }} #drag,#drop {{ display:inline-block; background:#ddd }}</style>
<button id='button' onclick='window.clicks=(window.clicks||0)+1' ondblclick='window.doubleClicks=(window.doubleClicks||0)+1'>click</button>
<button id='offscreen' style='position:absolute;top:1000px' onclick='window.offscreenClicked=true'>offscreen</button>
<input id='field'><select id='select'><option value='one'>One</option><option value='two'>Two</option></select>
<input id='check' type='checkbox'><input id='radio' type='radio' checked><input id='file' type='file'>
<div id='drag' draggable='true'>drag</div><div id='drop'>drop</div><div id='shadow'></div>
<script>
window.events=[]; const field=document.querySelector('#field');
for (const name of ['input','change','keydown','keyup']) field.addEventListener(name,e=>events.push(name));
document.querySelector('#button').addEventListener('mouseenter',()=>window.hovered=true);
field.addEventListener('focus',()=>window.focused=true); field.addEventListener('blur',()=>window.blurred=true);
const drag=document.querySelector('#drag'), drop=document.querySelector('#drop');
drag.addEventListener('dragstart', e=>e.dataTransfer.setData('text/plain','ok'));
drop.addEventListener('dragover', e=>e.preventDefault()); drop.addEventListener('drop', e=>{{e.preventDefault(); window.dropped=e.dataTransfer.getData('text/plain')}});
const root=document.querySelector('#shadow').attachShadow({{mode:'open'}}); root.innerHTML=`<button id=shadow-button>shadow</button>`;
root.querySelector('button').onclick=()=>window.shadowClicked=true;
</script>
<iframe src='/same'></iframe><iframe src='http://child.test:{child_port}/'></iframe>"#
        );
        let parent_server = tokio::spawn(serve_live_locator_fixture(
            parent_listener,
            parent_body,
            frame_body.clone(),
        ));
        let child_server = tokio::spawn(serve_live_locator_fixture(
            child_listener,
            frame_body,
            String::new(),
        ));

        let runtime = BrowserRuntime::launch(
            LaunchOptions::default()
                .headless(true)
                .arg("--site-per-process")
                .arg("--host-resolver-rules=MAP *.test 127.0.0.1"),
        )
        .await
        .unwrap();
        let session = runtime.default_session().await.unwrap();
        let page = session
            .new_page(format!("http://parent.test:{parent_port}/"))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let ready = Evaluate::new("Boolean(document.querySelector('#button'))")
                    .with_return_by_value(true)
                    .send(page.cdp_session())
                    .await
                    .unwrap()
                    .result
                    .value;
                if ready == Some(json!(true)) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();

        page.locator("#button").click().await.unwrap();
        page.locator("#button").double_click().await.unwrap();
        page.locator("#button").hover().await.unwrap();
        page.locator("#field").fill("A").await.unwrap();
        page.locator("#field").type_text("b").await.unwrap();
        page.locator("#field").focus().await.unwrap();
        page.locator("#field").blur().await.unwrap();
        page.locator("#offscreen").click().await.unwrap();
        page.locator("#select").select(["two"]).await.unwrap();
        let missing_option = page
            .locator("#select")
            .select(["missing"])
            .await
            .unwrap_err();
        assert_eq!(missing_option.action_completed(), ActionCompletion::Unknown);
        page.locator("#check").check().await.unwrap();
        page.locator("#check").uncheck().await.unwrap();
        let radio_error = page.locator("#radio").uncheck().await.unwrap_err();
        assert_eq!(
            radio_error.locator_failure(),
            Some(&crate::runtime::LocatorFailure::NotUncheckable)
        );
        page.locator("#drag")
            .drag_to(&page.locator("#drop"))
            .await
            .unwrap();
        page.locator("#shadow-button").click().await.unwrap();
        let upload = tempfile::NamedTempFile::new().unwrap();
        let upload_path = upload.path().to_string_lossy().into_owned();
        page.locator("#file")
            .set_input_files([upload_path])
            .await
            .unwrap();

        let frames = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let frames = page.frames().await.unwrap();
                if frames.len() >= 3 {
                    break frames;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
        let main_frame_id = page.main_frame().await.unwrap().id().clone();
        for frame in frames
            .into_iter()
            .filter(|frame| frame.id() != &main_frame_id)
        {
            frame.locator("#frame-button").click().await.unwrap();
            frame.click_at(15.0, 15.0).await.unwrap();
            let routed = frame.cdp_session().await.unwrap();
            let world =
                cdpkit::page::methods::CreateIsolatedWorld::new(frame.id().as_str().to_owned())
                    .send(&routed)
                    .await
                    .unwrap();
            let value = Evaluate::new("document.body.dataset.clicked")
                .with_context_id(world.execution_context_id)
                .with_return_by_value(true)
                .send(&routed)
                .await
                .unwrap()
                .result
                .value;
            assert_eq!(value, Some(json!("2")));
        }
        let facts = Evaluate::new("({clicks:window.clicks||0, doubleClicks:window.doubleClicks||0, hovered:window.hovered||false, focused:window.focused||false, blurred:window.blurred||false, offscreenClicked:window.offscreenClicked||false, value:document.querySelector('#field').value, selected:document.querySelector('#select').value, checked:document.querySelector('#check').checked, dropped:window.dropped||null, shadowClicked:window.shadowClicked||false, file:document.querySelector('#file').files[0]?.name||null, events:window.events})")
            .with_return_by_value(true).send(page.cdp_session()).await.unwrap().result.value.unwrap();
        assert_eq!(facts["clicks"], 3);
        assert_eq!(facts["doubleClicks"], 1);
        assert_eq!(facts["hovered"], true);
        assert_eq!(facts["focused"], true);
        assert_eq!(facts["blurred"], true);
        assert_eq!(facts["offscreenClicked"], true);
        assert_eq!(facts["value"], "Ab");
        assert_eq!(facts["selected"], "two");
        assert_eq!(facts["checked"], false);
        assert_eq!(facts["dropped"], "ok");
        assert_eq!(facts["shadowClicked"], true);
        assert!(facts["events"].as_array().unwrap().starts_with(&[
            json!("input"),
            json!("change"),
            json!("keydown"),
            json!("input"),
            json!("keyup")
        ]));

        assert!(runtime.close().await.is_complete());
        parent_server.abort();
        child_server.abort();
    }
}
