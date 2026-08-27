use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use super::{ActionCompletion, BrowserError, Frame, OperationPhase, Page, StackFrame};
use cdpkit::runtime::methods::{CallFunctionOn, Evaluate as CdpEvaluate, ReleaseObjectGroup};
use cdpkit::runtime::types::CallArgument;
use cdpkit::runtime::types::{ExceptionDetails, RemoteObject};

static NEXT_OBJECT_GROUP: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq)]
/// Structured details for an exception thrown while evaluating JavaScript.
pub struct JavaScriptException {
    text: String,
    line: i64,
    column: i64,
    url: Option<String>,
    preview: Option<String>,
    stack: Vec<StackFrame>,
}

impl JavaScriptException {
    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn line(&self) -> i64 {
        self.line
    }
    pub fn column(&self) -> i64 {
        self.column
    }
    pub fn url(&self) -> Option<&str> {
        self.url.as_deref()
    }
    pub fn preview(&self) -> Option<&str> {
        self.preview.as_deref()
    }
    pub fn stack(&self) -> &[StackFrame] {
        &self.stack
    }
}

#[derive(Debug, Clone, PartialEq)]
/// A JavaScript value returned without collapsing non-JSON primitives.
///
/// Use this type when `undefined`, non-finite numbers, negative zero, or
/// `BigInt` must remain distinguishable. JSON-compatible objects and arrays
/// are represented by [`RemoteValue::Json`].
pub enum RemoteValue {
    Undefined,
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Json(Value),
    NaN,
    Infinity,
    NegativeInfinity,
    NegativeZero,
    BigInt(String),
}

#[derive(Debug, Clone, PartialEq)]
/// A JSON-serializable argument passed to an evaluated JavaScript function.
///
/// Arguments are sent as CDP call arguments and are never interpolated into
/// the function declaration.
pub struct EvaluationArgument(Value);

impl EvaluationArgument {
    /// Serializes a Rust value for use as one evaluation argument.
    pub fn json(value: impl Serialize) -> Result<Self, BrowserError> {
        serde_json::to_value(value).map(Self).map_err(|error| {
            BrowserError::operation("serialize evaluation argument", OperationPhase::Preparation)
                .with_message(error.to_string())
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
enum EvaluationSource {
    Expression(String),
    Function(String),
}

#[derive(Debug, Clone, PartialEq)]
/// A JavaScript expression or function invocation and its execution options.
///
/// Evaluations run in the selected frame's current default main-world
/// execution context and await returned promises.
pub struct Evaluation {
    source: EvaluationSource,
    arguments: Vec<EvaluationArgument>,
    deadline: Option<Duration>,
}

impl Evaluation {
    /// Evaluates an expression without arguments.
    pub fn new(expression: impl Into<String>) -> Self {
        Self {
            source: EvaluationSource::Expression(expression.into()),
            arguments: Vec::new(),
            deadline: None,
        }
    }

    /// Calls a function declaration with arguments added by [`Self::argument`].
    pub fn function(declaration: impl Into<String>) -> Self {
        Self {
            source: EvaluationSource::Function(declaration.into()),
            arguments: Vec::new(),
            deadline: None,
        }
    }

    /// Appends one function argument.
    pub fn argument(mut self, argument: EvaluationArgument) -> Self {
        self.arguments.push(argument);
        self
    }

    /// Limits execution-context acquisition and the JavaScript evaluation.
    pub fn deadline(mut self, deadline: Duration) -> Self {
        self.deadline = Some(deadline);
        self
    }
}

impl From<&str> for Evaluation {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for Evaluation {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

struct RemoteValueHandleInner {
    page: Page,
    route: super::frame::LocatorFrameRoute,
    context: super::frame::MainWorldContext,
    object_group: String,
    object_id: Option<String>,
    type_name: String,
    subtype: Option<String>,
    description: Option<String>,
    value: Option<RemoteValue>,
    cleanup: parking_lot::Mutex<Option<super::PendingOwnershipGuard>>,
}

/// An explicitly managed reference to a value in a frame's main world.
///
/// A handle is bound to the page generation, frame document, routed CDP
/// session, and execution context in which it was created. Navigation, frame
/// detachment, route or context replacement, page closure, and browser
/// disconnection invalidate it. Call [`Self::release`] when it is no longer
/// needed; dropping an unreleased handle schedules best-effort managed cleanup.
pub struct RemoteValueHandle {
    inner: Arc<RemoteValueHandleInner>,
}

impl std::fmt::Debug for RemoteValueHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteValueHandle")
            .field("type_name", &self.inner.type_name)
            .field("object_group", &self.inner.object_group)
            .finish_non_exhaustive()
    }
}

impl RemoteValueHandle {
    /// Returns the CDP runtime type name, such as `object` or `function`.
    pub fn type_name(&self) -> &str {
        &self.inner.type_name
    }

    /// Returns the more specific CDP subtype when one is available.
    pub fn subtype(&self) -> Option<&str> {
        self.inner.subtype.as_deref()
    }

    /// Returns Chrome's diagnostic description of the remote value.
    pub fn description(&self) -> Option<&str> {
        self.inner.description.as_deref()
    }

    /// Copies the referenced value into a JSON value.
    pub async fn json_value(&self) -> Result<Value, BrowserError> {
        if let Some(value) = &self.inner.value {
            return remote_value_to_json(value.clone(), "serialize remote value");
        }
        let object_id = self.object_id("serialize remote value")?;
        let response = self
            .call_cdp(
                CallFunctionOn::new("function() { return this; }")
                    .with_object_id(object_id)
                    .with_return_by_value(true)
                    .with_await_promise(true),
                "serialize remote value",
            )
            .await?;
        let value = remote_value(response)?;
        remote_value_to_json(value, "serialize remote value")
    }

    /// Returns an independently releasable handle for a named property.
    pub async fn property(&self, name: impl AsRef<str>) -> Result<Self, BrowserError> {
        let object_id = self.object_id("read remote property")?;
        self.validate("read remote property").await?;
        let group = object_group();
        let cleanup = register_group_cleanup(&self.inner.page, &self.inner.route.session, &group);
        let response = self
            .call_cdp(
                CallFunctionOn::new("function(name) { return this[name]; }")
                    .with_object_id(object_id)
                    .with_arguments(vec![CallArgument {
                        value: Some(Value::String(name.as_ref().to_owned())),
                        unserializable_value: None,
                        object_id: None,
                    }])
                    .with_object_group(group.clone())
                    .with_await_promise(true),
                "read remote property",
            )
            .await?;
        handle_from_remote(
            self.inner.page.clone(),
            self.inner.route.clone(),
            self.inner.context.clone(),
            group,
            cleanup,
            response,
        )
    }

    /// Calls a function with this value as `this` and returns a new handle.
    pub async fn call(
        &self,
        function: impl Into<String>,
        arguments: impl IntoIterator<Item = EvaluationArgument>,
    ) -> Result<Self, BrowserError> {
        let object_id = self.object_id("call remote value")?;
        self.validate("call remote value").await?;
        let group = object_group();
        let cleanup = register_group_cleanup(&self.inner.page, &self.inner.route.session, &group);
        let arguments = arguments.into_iter().map(call_argument).collect();
        let response = self
            .call_cdp(
                CallFunctionOn::new(function)
                    .with_object_id(object_id)
                    .with_arguments(arguments)
                    .with_object_group(group.clone())
                    .with_await_promise(true),
                "call remote value",
            )
            .await?;
        handle_from_remote(
            self.inner.page.clone(),
            self.inner.route.clone(),
            self.inner.context.clone(),
            group,
            cleanup,
            response,
        )
    }

    /// Releases this handle's object group. Repeated calls are harmless.
    pub async fn release(&self) -> Result<(), BrowserError> {
        let cleanup = self.inner.cleanup.lock().take();
        match cleanup {
            Some(cleanup) => cleanup.cleanup().await.map_err(|error| {
                BrowserError::operation("release remote value", OperationPhase::Cleanup)
                    .with_message(error.to_string())
            }),
            None => Ok(()),
        }
    }

    fn object_id(&self, operation: &'static str) -> Result<String, BrowserError> {
        self.inner.object_id.clone().ok_or_else(|| {
            BrowserError::operation(operation, OperationPhase::Preparation)
                .with_message("remote JavaScript value is a primitive and has no object identity")
        })
    }

    async fn call_cdp(
        &self,
        command: CallFunctionOn,
        operation: &'static str,
    ) -> Result<RemoteObject, BrowserError> {
        let _operation = self.inner.page.admit_operation(operation)?;
        let store = self.inner.page.locator_frame_store(&_operation).await?;
        store.validate_main_world_context(&self.inner.route, &self.inner.context)?;
        let response = command
            .send(&self.inner.route.session)
            .await
            .map_err(|error| route_or_cdp_error(store, &self.inner.route, operation, error))?;
        store.validate_main_world_context(&self.inner.route, &self.inner.context)?;
        if let Some(exception) = response.exception_details {
            return Err(javascript_exception(operation, exception));
        }
        Ok(response.result)
    }

    async fn validate(&self, operation: &'static str) -> Result<(), BrowserError> {
        let admitted = self.inner.page.admit_operation(operation)?;
        let store = self.inner.page.locator_frame_store(&admitted).await?;
        store.validate_main_world_context(&self.inner.route, &self.inner.context)
    }
}

impl Page {
    /// Evaluates JavaScript in the main frame's current default main world.
    pub async fn evaluate<T: DeserializeOwned>(
        &self,
        evaluation: impl Into<Evaluation>,
    ) -> Result<T, BrowserError> {
        let value = self.evaluate_value(evaluation).await?;
        let value = remote_value_to_json(value, "evaluate JavaScript").map_err(mark_completed)?;
        serde_json::from_value(value).map_err(|error| {
            BrowserError::operation("deserialize JavaScript result", OperationPhase::Observation)
                .with_message(error.to_string())
                .with_action_completion(ActionCompletion::Completed)
        })
    }

    /// Evaluates JavaScript while preserving non-JSON primitive values.
    pub async fn evaluate_value(
        &self,
        evaluation: impl Into<Evaluation>,
    ) -> Result<RemoteValue, BrowserError> {
        self.main_frame().await?.evaluate_value(evaluation).await
    }

    /// Evaluates JavaScript and retains the result as a remote handle.
    pub async fn evaluate_handle(
        &self,
        evaluation: impl Into<Evaluation>,
    ) -> Result<RemoteValueHandle, BrowserError> {
        self.main_frame().await?.evaluate_handle(evaluation).await
    }
}

impl Frame {
    /// Evaluates JavaScript in this frame's current default main world.
    pub async fn evaluate<T: DeserializeOwned>(
        &self,
        evaluation: impl Into<Evaluation>,
    ) -> Result<T, BrowserError> {
        let value = self.evaluate_value(evaluation).await?;
        let value = remote_value_to_json(value, "evaluate JavaScript").map_err(mark_completed)?;
        serde_json::from_value(value).map_err(|error| {
            BrowserError::operation("deserialize JavaScript result", OperationPhase::Observation)
                .with_message(error.to_string())
                .with_action_completion(ActionCompletion::Completed)
        })
    }

    /// Evaluates JavaScript while preserving non-JSON primitive values.
    pub async fn evaluate_value(
        &self,
        evaluation: impl Into<Evaluation>,
    ) -> Result<RemoteValue, BrowserError> {
        let evaluation = evaluation.into();
        let (remote, cleanup) = execute(self, evaluation, true).await?;
        let value = remote_value(remote).map_err(mark_completed);
        let cleanup_result = cleanup.cleanup().await;
        match (value, cleanup_result) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(cleanup)) => Err(BrowserError::operation(
                "release evaluation object group",
                OperationPhase::Cleanup,
            )
            .with_message(cleanup.to_string())
            .with_action_completion(ActionCompletion::Completed)),
            (Err(error), Err(cleanup)) => Err(error.with_cleanup_failure(
                super::CleanupFailure::new("evaluation object group", cleanup.to_string()),
            )),
        }
    }

    /// Evaluates JavaScript and retains the result as a remote handle.
    pub async fn evaluate_handle(
        &self,
        evaluation: impl Into<Evaluation>,
    ) -> Result<RemoteValueHandle, BrowserError> {
        let evaluation = evaluation.into();
        let (remote, cleanup, page, route, context, group) =
            execute_handle(self, evaluation).await?;
        handle_from_remote(page, route, context, group, cleanup, remote)
    }
}

async fn execute(
    frame: &Frame,
    evaluation: Evaluation,
    return_by_value: bool,
) -> Result<(RemoteObject, super::PendingOwnershipGuard), BrowserError> {
    let deadline = evaluation.deadline;
    let (remote, cleanup, _, _, _, _) =
        within_deadline(deadline, execute_common(frame, evaluation, return_by_value)).await?;
    Ok((remote, cleanup))
}

async fn execute_handle(
    frame: &Frame,
    evaluation: Evaluation,
) -> Result<
    (
        RemoteObject,
        super::PendingOwnershipGuard,
        Page,
        super::frame::LocatorFrameRoute,
        super::frame::MainWorldContext,
        String,
    ),
    BrowserError,
> {
    let deadline = evaluation.deadline;
    within_deadline(deadline, execute_common(frame, evaluation, false)).await
}

async fn within_deadline<T>(
    deadline: Option<Duration>,
    future: impl std::future::Future<Output = Result<T, BrowserError>>,
) -> Result<T, BrowserError> {
    match deadline {
        Some(deadline) => tokio::time::timeout(deadline, future).await.map_err(|_| {
            BrowserError::operation("evaluate JavaScript", OperationPhase::Observation)
                .with_message(format!("JavaScript evaluation exceeded {deadline:?}"))
                .with_action_completion(ActionCompletion::Unknown)
        })?,
        None => future.await,
    }
}

async fn execute_common(
    frame: &Frame,
    evaluation: Evaluation,
    return_by_value: bool,
) -> Result<
    (
        RemoteObject,
        super::PendingOwnershipGuard,
        Page,
        super::frame::LocatorFrameRoute,
        super::frame::MainWorldContext,
        String,
    ),
    BrowserError,
> {
    let page = frame.page().clone();
    let operation = page.admit_operation("evaluate JavaScript")?;
    let store = page.locator_frame_store(&operation).await?;
    let route = store.locator_route(frame)?;
    let context = store.main_world_context(&route).await?;
    store.validate_main_world_context(&route, &context)?;
    let group = object_group();
    let cleanup = register_group_cleanup(&page, &route.session, &group);
    let command = async {
        match evaluation.source {
            EvaluationSource::Expression(expression) => {
                if !evaluation.arguments.is_empty() {
                    return Err(BrowserError::operation(
                        "evaluate JavaScript",
                        OperationPhase::Preparation,
                    )
                    .with_message("arguments require Evaluation::function"));
                }
                let response = CdpEvaluate::new(expression)
                    .with_unique_context_id(context.unique_id.clone())
                    .with_object_group(group.clone())
                    .with_return_by_value(return_by_value)
                    .with_await_promise(true)
                    .send(&route.session)
                    .await
                    .map_err(|error| {
                        route_or_cdp_error(store, &route, "evaluate JavaScript", error)
                    })?;
                if let Some(exception) = response.exception_details {
                    return Err(javascript_exception("evaluate JavaScript", exception));
                }
                Ok(response.result)
            }
            EvaluationSource::Function(function) => {
                let response = CallFunctionOn::new(function)
                    .with_unique_context_id(context.unique_id.clone())
                    .with_arguments(
                        evaluation
                            .arguments
                            .into_iter()
                            .map(call_argument)
                            .collect(),
                    )
                    .with_object_group(group.clone())
                    .with_return_by_value(return_by_value)
                    .with_await_promise(true)
                    .send(&route.session)
                    .await
                    .map_err(|error| {
                        route_or_cdp_error(store, &route, "evaluate JavaScript", error)
                    })?;
                if let Some(exception) = response.exception_details {
                    return Err(javascript_exception("evaluate JavaScript", exception));
                }
                Ok(response.result)
            }
        }
    };
    let remote = command.await?;
    store
        .validate_main_world_context(&route, &context)
        .map_err(mark_completed)?;
    Ok((remote, cleanup, page, route, context, group))
}

fn call_argument(argument: EvaluationArgument) -> CallArgument {
    CallArgument {
        value: Some(argument.0),
        unserializable_value: None,
        object_id: None,
    }
}

fn object_group() -> String {
    format!(
        "browserkit-evaluate-{}",
        NEXT_OBJECT_GROUP.fetch_add(1, Ordering::Relaxed)
    )
}

fn register_group_cleanup(
    page: &Page,
    session: &cdpkit::Session,
    group: &str,
) -> super::PendingOwnershipGuard {
    let session = session.clone();
    let group = group.to_owned();
    page.track_locator_cleanup(format!("remote-object-group:{group}"), move || async move {
        match ReleaseObjectGroup::new(group).send(&session).await {
            Ok(()) => Ok(()),
            Err(error) if resource_is_gone(&error.to_string()) => Ok(()),
            Err(error) => Err(super::OwnershipCleanupError::from(error)),
        }
    })
}

fn resource_is_gone(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "target closed",
        "session closed",
        "no session",
        "not found",
        "connection closed",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

fn route_or_cdp_error(
    store: &super::FrameStore,
    route: &super::frame::LocatorFrameRoute,
    operation: &'static str,
    error: cdpkit::CdpError,
) -> BrowserError {
    store
        .validate_locator_route(route)
        .err()
        .map(mark_unknown)
        .unwrap_or_else(|| {
            BrowserError::cdp_operation(operation, OperationPhase::Dispatch, error)
                .with_action_completion(ActionCompletion::Unknown)
        })
}

fn mark_completed(error: BrowserError) -> BrowserError {
    error.with_action_completion(ActionCompletion::Completed)
}

fn mark_unknown(error: BrowserError) -> BrowserError {
    error.with_action_completion(ActionCompletion::Unknown)
}

fn handle_from_remote(
    page: Page,
    route: super::frame::LocatorFrameRoute,
    context: super::frame::MainWorldContext,
    object_group: String,
    cleanup: super::PendingOwnershipGuard,
    remote: RemoteObject,
) -> Result<RemoteValueHandle, BrowserError> {
    let value = remote_value(remote.clone()).ok();
    Ok(RemoteValueHandle {
        inner: Arc::new(RemoteValueHandleInner {
            page,
            route,
            context,
            object_group,
            object_id: remote.object_id,
            type_name: remote.type_.as_ref().to_owned(),
            subtype: remote.subtype.map(|value| value.as_ref().to_owned()),
            description: remote.description,
            value,
            cleanup: parking_lot::Mutex::new(Some(cleanup)),
        }),
    })
}

fn remote_value_to_json(
    value: RemoteValue,
    operation: &'static str,
) -> Result<Value, BrowserError> {
    match value {
        RemoteValue::Null => Ok(Value::Null),
        RemoteValue::Bool(value) => Ok(Value::Bool(value)),
        RemoteValue::Number(value) => Ok(Value::Number(value)),
        RemoteValue::String(value) => Ok(Value::String(value)),
        RemoteValue::Json(value) => Ok(value),
        RemoteValue::Undefined
        | RemoteValue::NaN
        | RemoteValue::Infinity
        | RemoteValue::NegativeInfinity
        | RemoteValue::NegativeZero
        | RemoteValue::BigInt(_) => Err(BrowserError::operation(
            operation,
            OperationPhase::Observation,
        )
        .with_message("JavaScript result is not JSON-serializable; use evaluate_value")),
    }
}

fn remote_value(remote: RemoteObject) -> Result<RemoteValue, BrowserError> {
    let type_name = remote.type_.as_ref();
    if type_name == "undefined" {
        return Ok(RemoteValue::Undefined);
    }
    if remote
        .subtype
        .as_ref()
        .is_some_and(|value| value.as_ref() == "null")
    {
        return Ok(RemoteValue::Null);
    }
    if let Some(unserializable) = remote.unserializable_value.as_deref() {
        return match unserializable {
            "NaN" => Ok(RemoteValue::NaN),
            "Infinity" => Ok(RemoteValue::Infinity),
            "-Infinity" => Ok(RemoteValue::NegativeInfinity),
            "-0" => Ok(RemoteValue::NegativeZero),
            bigint if type_name == "bigint" && bigint.ends_with('n') => {
                Ok(RemoteValue::BigInt(bigint[..bigint.len() - 1].to_owned()))
            }
            other => Err(BrowserError::operation(
                "decode JavaScript value",
                OperationPhase::Observation,
            )
            .with_message(format!(
                "unsupported unserializable JavaScript value: {other}"
            ))),
        };
    }
    match (type_name, remote.value) {
        ("boolean", Some(Value::Bool(value))) => Ok(RemoteValue::Bool(value)),
        ("number", Some(Value::Number(value))) => Ok(RemoteValue::Number(value)),
        ("string", Some(Value::String(value))) => Ok(RemoteValue::String(value)),
        ("object", Some(value)) => Ok(RemoteValue::Json(value)),
        (_, Some(value)) => Ok(RemoteValue::Json(value)),
        _ if remote.object_id.is_some() => Err(BrowserError::operation(
            "decode JavaScript value",
            OperationPhase::Observation,
        )
        .with_message(
            "JavaScript value cannot be returned by value; use evaluate_handle for remote objects",
        )),
        _ => Err(invalid_remote_value(type_name)),
    }
}

fn invalid_remote_value(type_name: &str) -> BrowserError {
    BrowserError::operation("decode JavaScript value", OperationPhase::Observation)
        .with_message(format!("JavaScript {type_name} result had no value"))
}

fn javascript_exception(operation: &'static str, details: ExceptionDetails) -> BrowserError {
    let mut stack = Vec::new();
    if let Some(trace) = details.stack_trace.as_ref() {
        collect_stack_frames(trace, &mut stack);
    }
    let exception = JavaScriptException {
        text: details.text.clone(),
        line: details.line_number,
        column: details.column_number,
        url: details.url,
        preview: details.exception.and_then(|value| value.description),
        stack,
    };
    BrowserError::operation(operation, OperationPhase::Observation)
        .with_message(exception.text.clone())
        .with_javascript_exception(exception)
        .with_action_completion(ActionCompletion::Completed)
}

fn collect_stack_frames(trace: &cdpkit::runtime::types::StackTrace, stack: &mut Vec<StackFrame>) {
    stack.extend(trace.call_frames.iter().map(|frame| StackFrame {
        function_name: frame.function_name.clone(),
        url: frame.url.clone(),
        line_number: frame.line_number,
        column_number: frame.column_number,
    }));
    if let Some(parent) = trace.parent.as_deref() {
        collect_stack_frames(parent, stack);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{BrowserRuntime, BrowserSessionId, PageOwnership};
    use cdpkit::runtime::types::{ExceptionDetails, RemoteObject};
    use futures::{SinkExt, StreamExt};
    use serde_json::json;
    use std::sync::{Arc, Weak};
    use tokio_tungstenite::tungstenite::Message;

    fn remote(value: Value) -> RemoteObject {
        serde_json::from_value(value).unwrap()
    }

    #[derive(Default)]
    struct EvaluationFixture {
        stall_dispatched: Option<tokio::sync::oneshot::Sender<()>>,
        stall_release: Option<tokio::sync::oneshot::Receiver<()>>,
        release_error: bool,
        suppress_context: bool,
    }

    async fn page_for_evaluation() -> (Page, Arc<parking_lot::Mutex<Vec<Value>>>) {
        page_for_evaluation_with(EvaluationFixture::default()).await
    }

    async fn page_for_evaluation_with(
        fixture: EvaluationFixture,
    ) -> (Page, Arc<parking_lot::Mutex<Vec<Value>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let commands = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let server_commands = Arc::clone(&commands);
        tokio::spawn(async move {
            let mut fixture = fixture;
            let (stream, _) = listener.accept().await.unwrap();
            let websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut write, mut read) = websocket.split();
            while let Some(Ok(Message::Text(text))) = read.next().await {
                let command: Value = serde_json::from_str(&text).unwrap();
                server_commands.lock().push(command.clone());
                let id = command["id"].as_u64().unwrap();
                let method = command["method"].as_str().unwrap();
                let session_id = command.get("sessionId").cloned();
                if method == "Runtime.evaluate" && command["params"]["expression"] == "__stall__" {
                    if let Some(dispatched) = fixture.stall_dispatched.take() {
                        dispatched
                            .send(())
                            .expect("stall dispatch receiver dropped");
                    }
                    if let Some(release) = fixture.stall_release.take() {
                        tokio::time::timeout(Duration::from_secs(1), release)
                            .await
                            .expect("stall release was not acknowledged within one second")
                            .expect("stall release sender dropped");
                    }
                }
                if method == "Runtime.releaseObjectGroup" && fixture.release_error {
                    let mut response = json!({"id":id,"error":{"code":-32000,"message":"injected release failure"}});
                    if let Some(session_id) = session_id {
                        response["sessionId"] = session_id;
                    }
                    write
                        .send(Message::Text(response.to_string().into()))
                        .await
                        .unwrap();
                    continue;
                }
                if method == "Runtime.enable" && !fixture.suppress_context {
                    let mut event = json!({
                        "method":"Runtime.executionContextCreated",
                        "params":{"context":{
                            "id":101,"origin":"https://example.test","name":"","uniqueId":"main-world-101",
                            "auxData":{"isDefault":true,"type":"default","frameId":"main"}
                        }}
                    });
                    if let Some(session_id) = &session_id {
                        event["sessionId"] = session_id.clone();
                    }
                    write
                        .send(Message::Text(event.to_string().into()))
                        .await
                        .unwrap();
                }
                if method == "Runtime.evaluate" && command["params"]["expression"] == "__navigate__"
                {
                    let mut event = json!({
                        "method":"Page.frameNavigated",
                        "params":{"frame":{
                            "id":"main","loaderId":"loader-next","url":"https://example.test/next",
                            "domainAndRegistry":"example.test","securityOrigin":"https://example.test",
                            "mimeType":"text/html","secureContextType":"Secure","crossOriginIsolatedContextType":"NotIsolated",
                            "gatedAPIFeatures":[]
                        },"type":"Navigation"}
                    });
                    if let Some(session_id) = &session_id {
                        event["sessionId"] = session_id.clone();
                    }
                    write
                        .send(Message::Text(event.to_string().into()))
                        .await
                        .unwrap();
                }
                let result = match method {
                    "Browser.getVersion" => crate::runtime::test_browser_version_result(),
                    "Target.getBrowserContexts" => json!({"browserContextIds":[]}),
                    "Target.setDiscoverTargets"
                    | "Page.enable"
                    | "Target.setAutoAttach"
                    | "Target.detachFromTarget"
                    | "Runtime.enable"
                    | "Runtime.setAsyncCallStackDepth"
                    | "Runtime.releaseObjectGroup" => json!({}),
                    "Page.getFrameTree" => json!({"frameTree":{"frame":{
                        "id":"main","loaderId":"loader-main","url":"https://example.test/",
                        "domainAndRegistry":"example.test","securityOrigin":"https://example.test",
                        "mimeType":"text/html","secureContextType":"Secure","crossOriginIsolatedContextType":"NotIsolated",
                        "gatedAPIFeatures":[]
                    }}}),
                    "Runtime.evaluate" => match command["params"]["expression"].as_str().unwrap() {
                        "globalThis.appAnswer" => json!({"result":{"type":"number","value":42}}),
                        "void 0" => json!({"result":{"type":"undefined"}}),
                        "throw new Error('boom')" => json!({
                            "result":{"type":"object","subtype":"error","description":"Error: boom"},
                            "exceptionDetails":{"exceptionId":9,"text":"Uncaught Error: boom","lineNumber":0,"columnNumber":0,
                                "exception":{"type":"object","subtype":"error","description":"Error: boom"}}
                        }),
                        "__navigate__" => json!({"result":{"type":"undefined"}}),
                        "__stall__" => json!({"result":{"type":"number","value":1}}),
                        _ => {
                            json!({"result":{"type":"object","className":"Object","description":"Object","objectId":"root-1"}})
                        }
                    },
                    "Runtime.callFunctionOn" => {
                        let declaration =
                            command["params"]["functionDeclaration"].as_str().unwrap();
                        if declaration.contains("this[name]") {
                            json!({"result":{"type":"object","className":"Object","description":"Object","objectId":"property-1"}})
                        } else if command["params"].get("uniqueContextId").is_some() {
                            json!({"result":{"type":"number","value":42}})
                        } else if command["params"]["returnByValue"] == json!(true) {
                            json!({"result":{"type":"object","value":{"answer":42}}})
                        } else {
                            json!({"result":{"type":"number","value":50}})
                        }
                    }
                    other => panic!("unexpected evaluation test command: {other}"),
                };
                let mut response = json!({"id":id,"result":result});
                if let Some(session_id) = session_id {
                    response["sessionId"] = session_id;
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
            "target-eval".into(),
            PageOwnership::Attached,
            runtime.cdp().session("frame-session"),
        );
        (page, commands)
    }

    #[test]
    fn remote_value_preserves_non_json_primitives() {
        let cases = [
            (json!({"type":"undefined"}), RemoteValue::Undefined),
            (
                json!({"type":"object","subtype":"null","value":null}),
                RemoteValue::Null,
            ),
            (
                json!({"type":"number","unserializableValue":"NaN"}),
                RemoteValue::NaN,
            ),
            (
                json!({"type":"number","unserializableValue":"Infinity"}),
                RemoteValue::Infinity,
            ),
            (
                json!({"type":"number","unserializableValue":"-Infinity"}),
                RemoteValue::NegativeInfinity,
            ),
            (
                json!({"type":"number","unserializableValue":"-0"}),
                RemoteValue::NegativeZero,
            ),
            (
                json!({"type":"bigint","unserializableValue":"9007199254740993n"}),
                RemoteValue::BigInt("9007199254740993".into()),
            ),
        ];
        for (raw, expected) in cases {
            assert_eq!(remote_value(remote(raw)).unwrap(), expected);
        }
    }

    #[test]
    fn remote_value_distinguishes_json_objects_from_unserializable_handles() {
        assert_eq!(
            remote_value(remote(json!({"type":"object","value":{"answer":42}}))).unwrap(),
            RemoteValue::Json(json!({"answer":42}))
        );
        let error = remote_value(remote(json!({
            "type":"object", "className":"Object", "description":"Object", "objectId":"1.2.3"
        })))
        .unwrap_err();
        assert!(error.to_string().contains("evaluate_handle"));
    }

    #[test]
    fn exception_details_remain_structured() {
        let details: ExceptionDetails = serde_json::from_value(json!({
            "exceptionId": 7,
            "text": "Uncaught (in promise) Error: boom",
            "lineNumber": 4,
            "columnNumber": 9,
            "url": "https://example.test/app.js",
            "stackTrace": {
                "description": "async",
                "callFrames": [{
                    "functionName":"run", "scriptId":"11", "url":"https://example.test/app.js",
                    "lineNumber":4, "columnNumber":9
                }],
                "parent": {"description":"awaited","callFrames":[{
                    "functionName":"caller", "scriptId":"10", "url":"https://example.test/caller.js",
                    "lineNumber":2, "columnNumber":3
                }]}
            },
            "exception": {"type":"object","subtype":"error","className":"Error","description":"Error: boom"}
        })).unwrap();
        let error = javascript_exception("evaluate JavaScript", details);
        let exception = error.javascript_exception().unwrap();
        assert_eq!(exception.text(), "Uncaught (in promise) Error: boom");
        assert_eq!(exception.line(), 4);
        assert_eq!(exception.column(), 9);
        assert_eq!(exception.preview(), Some("Error: boom"));
        assert_eq!(exception.stack()[0].function_name, "run");
        assert_eq!(exception.stack()[1].function_name, "caller");
    }

    #[tokio::test]
    async fn page_and_frame_evaluate_in_their_default_main_world() {
        let (page, commands) = page_for_evaluation().await;
        let answer: i64 = page.evaluate("globalThis.appAnswer").await.unwrap();
        assert_eq!(answer, 42);
        let frame = page.main_frame().await.unwrap();
        let value = frame
            .evaluate_value(
                Evaluation::function("function(left, right) { return left + right; }")
                    .argument(EvaluationArgument::json(20).unwrap())
                    .argument(EvaluationArgument::json(22).unwrap()),
            )
            .await
            .unwrap();
        assert_eq!(value, RemoteValue::Number(serde_json::Number::from(42)));
        let commands = commands.lock();
        let evaluate = commands
            .iter()
            .find(|command| command["method"] == "Runtime.evaluate")
            .unwrap();
        assert_eq!(evaluate["params"]["uniqueContextId"], "main-world-101");
        assert_eq!(evaluate["params"]["awaitPromise"], true);
        assert_eq!(evaluate["params"]["returnByValue"], true);
        let call = commands
            .iter()
            .find(|command| command["method"] == "Runtime.callFunctionOn")
            .unwrap();
        assert_eq!(call["params"]["uniqueContextId"], "main-world-101");
        assert_eq!(call["params"]["arguments"][0]["value"], 20);
        assert_eq!(call["params"]["arguments"][1]["value"], 22);
        let async_stack = commands
            .iter()
            .find(|command| command["method"] == "Runtime.setAsyncCallStackDepth")
            .expect("async JavaScript stacks are enabled");
        assert!(async_stack["params"]["maxDepth"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn exception_and_non_json_values_have_explicit_results() {
        let (page, _) = page_for_evaluation().await;
        assert_eq!(
            page.evaluate_value("void 0").await.unwrap(),
            RemoteValue::Undefined
        );
        let error = page
            .evaluate_value("throw new Error('boom')")
            .await
            .unwrap_err();
        assert_eq!(
            error.javascript_exception().unwrap().preview(),
            Some("Error: boom")
        );
    }

    #[tokio::test]
    async fn remote_handle_supports_properties_json_calls_and_idempotent_release() {
        let (page, commands) = page_for_evaluation().await;
        let object = page.evaluate_handle("({answer:42})").await.unwrap();
        assert_eq!(object.type_name(), "object");
        assert_eq!(object.description(), Some("Object"));
        let property = object.property("nested").await.unwrap();
        assert_eq!(property.json_value().await.unwrap(), json!({"answer":42}));
        let called = object
            .call(
                "function(value) { return this.answer + value; }",
                [EvaluationArgument::json(8).unwrap()],
            )
            .await
            .unwrap();
        assert_eq!(called.type_name(), "number");
        assert_eq!(called.json_value().await.unwrap(), json!(50));
        property.release().await.unwrap();
        called.release().await.unwrap();
        object.release().await.unwrap();
        object.release().await.unwrap();
        let commands = commands.lock();
        let groups = commands
            .iter()
            .filter(|command| {
                matches!(
                    command["method"].as_str(),
                    Some("Runtime.evaluate" | "Runtime.callFunctionOn")
                )
            })
            .filter_map(|command| command["params"]["objectGroup"].as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(groups.len(), 3);
        let releases = commands
            .iter()
            .filter(|command| command["method"] == "Runtime.releaseObjectGroup")
            .count();
        assert_eq!(releases, 3);
    }

    #[tokio::test]
    async fn handle_fails_closed_after_document_replacement_but_still_releases() {
        let (page, commands) = page_for_evaluation().await;
        let object = page.evaluate_handle("({answer:42})").await.unwrap();
        CdpEvaluate::new("__navigate__")
            .send(page.cdp_session())
            .await
            .unwrap();
        loop {
            match page.main_frame().await {
                Ok(frame) if frame.document_epoch().get() > 0 => break,
                _ => tokio::task::yield_now().await,
            }
        }
        let error = object.property("answer").await.unwrap_err();
        assert!(error.to_string().contains("stale"));
        object.release().await.unwrap();
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
    async fn page_close_drains_unreleased_remote_object_groups() {
        let (page, commands) = page_for_evaluation().await;
        let _object = page.evaluate_handle("({answer:42})").await.unwrap();
        let report = page.close().await;
        assert!(report.is_complete());
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
    async fn deadline_after_dispatch_drops_the_call_and_releases_its_group_once() {
        let (dispatched_tx, dispatched_rx) = tokio::sync::oneshot::channel();
        let (stall_release_tx, stall_release_rx) = tokio::sync::oneshot::channel();
        let (page, commands) = page_for_evaluation_with(EvaluationFixture {
            stall_dispatched: Some(dispatched_tx),
            stall_release: Some(stall_release_rx),
            release_error: false,
            suppress_context: false,
        })
        .await;
        let frame = page.main_frame().await.unwrap();
        let mut executing = Box::pin(execute_common(&frame, Evaluation::new("__stall__"), true));

        tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                dispatched = dispatched_rx => {
                    dispatched.expect("stall dispatch sender dropped");
                }
                result = &mut executing => {
                    match result {
                        Ok(_) => panic!("evaluation completed before dispatch acknowledgement"),
                        Err(error) => panic!(
                            "evaluation failed before dispatch acknowledgement: {error}"
                        ),
                    }
                }
            }
        })
        .await
        .expect("stall dispatch was not acknowledged within one second");

        let error = match within_deadline(Some(Duration::from_millis(10)), &mut executing).await {
            Ok(_) => panic!("evaluation completed before its post-dispatch deadline"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("exceeded"));
        assert_eq!(error.operation_name(), Some("evaluate JavaScript"));
        assert_eq!(error.phase(), OperationPhase::Observation);
        assert_eq!(error.action_completed(), ActionCompletion::Unknown);

        drop(executing);
        stall_release_tx
            .send(())
            .expect("stall release receiver dropped");
        let close_report = tokio::time::timeout(Duration::from_secs(1), page.close())
            .await
            .expect("page cleanup did not drain within one second");
        assert!(close_report.is_complete());

        let commands = commands.lock();
        let object_group = commands
            .iter()
            .find(|command| command["method"] == "Runtime.evaluate")
            .and_then(|command| command["params"]["objectGroup"].as_str())
            .expect("stalled evaluation did not include an object group");
        assert_eq!(
            commands
                .iter()
                .filter(|command| command["method"] == "Runtime.evaluate")
                .count(),
            1
        );
        assert_eq!(
            commands
                .iter()
                .filter(|command| {
                    command["method"] == "Runtime.releaseObjectGroup"
                        && command["params"]["objectGroup"] == object_group
                })
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn deadline_before_dispatch_has_structured_timeout_without_group_cleanup() {
        let (page, commands) = page_for_evaluation_with(EvaluationFixture {
            suppress_context: true,
            ..Default::default()
        })
        .await;
        let error = page
            .evaluate_value(
                Evaluation::new("globalThis.appAnswer").deadline(Duration::from_millis(10)),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("exceeded"));
        assert_eq!(error.operation_name(), Some("evaluate JavaScript"));
        assert_eq!(error.phase(), OperationPhase::Observation);
        assert_eq!(error.action_completed(), ActionCompletion::Unknown);
        let commands = commands.lock();
        assert!(!commands
            .iter()
            .any(|command| command["method"] == "Runtime.evaluate"));
        assert!(!commands
            .iter()
            .any(|command| command["method"] == "Runtime.releaseObjectGroup"));
    }

    #[tokio::test]
    async fn aborting_an_evaluation_future_still_releases_its_object_group() {
        let (dispatched_tx, dispatched_rx) = tokio::sync::oneshot::channel();
        let (stall_release_tx, stall_release_rx) = tokio::sync::oneshot::channel();
        let (page, commands) = page_for_evaluation_with(EvaluationFixture {
            stall_dispatched: Some(dispatched_tx),
            stall_release: Some(stall_release_rx),
            release_error: false,
            suppress_context: false,
        })
        .await;
        let evaluation_page = page.clone();
        let evaluating =
            tokio::spawn(async move { evaluation_page.evaluate_value("__stall__").await });
        tokio::time::timeout(Duration::from_secs(1), dispatched_rx)
            .await
            .expect("stall dispatch was not acknowledged within one second")
            .expect("stall dispatch sender dropped");
        evaluating.abort();
        let join_error = tokio::time::timeout(Duration::from_secs(1), evaluating)
            .await
            .expect("aborted evaluation did not finish within one second")
            .expect_err("aborted evaluation task unexpectedly completed");
        assert!(join_error.is_cancelled());
        stall_release_tx
            .send(())
            .expect("stall release receiver dropped");
        let close_report = tokio::time::timeout(Duration::from_secs(1), page.close())
            .await
            .expect("page cleanup did not drain within one second");
        assert!(close_report.is_complete());

        let commands = commands.lock();
        let object_group = commands
            .iter()
            .find(|command| command["method"] == "Runtime.evaluate")
            .and_then(|command| command["params"]["objectGroup"].as_str())
            .expect("stalled evaluation did not include an object group");
        assert_eq!(
            commands
                .iter()
                .filter(|command| {
                    command["method"] == "Runtime.releaseObjectGroup"
                        && command["params"]["objectGroup"] == object_group
                })
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn cleanup_failure_is_structured_and_page_close_reports_unreleased_handle_failure() {
        let (page, _) = page_for_evaluation_with(EvaluationFixture {
            release_error: true,
            ..Default::default()
        })
        .await;
        let handle = page.evaluate_handle("({answer:42})").await.unwrap();
        let error = handle.release().await.unwrap_err();
        assert_eq!(error.phase(), OperationPhase::Cleanup);
        assert!(error.to_string().contains("injected release failure"));

        let unreleased = page.evaluate_handle("({answer:42})").await.unwrap();
        let report = page.close().await;
        assert!(!report.is_complete());
        assert!(report
            .failures()
            .iter()
            .any(|failure| failure.message().contains("injected release failure")));
        drop(unreleased);
    }

    #[tokio::test]
    #[ignore = "requires installed Chrome and loopback sockets"]
    async fn live_chrome_evaluates_main_same_process_and_oopif_worlds() {
        use crate::runtime::locator::resolver::tests::serve_live_locator_fixture;
        use crate::runtime::LaunchOptions;

        let oopif_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let oopif_port = oopif_listener.local_addr().unwrap().port();
        let parent_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let parent_port = parent_listener.local_addr().unwrap().port();
        let parent_body = format!(
            r#"<!doctype html><script>globalThis.appGlobal='main';</script>
            <iframe src='/same'></iframe><iframe src='http://child.test:{oopif_port}/'></iframe>"#
        );
        let same_body = "<!doctype html><script>globalThis.appGlobal='same';</script>".to_owned();
        let oopif_body = "<!doctype html><script>globalThis.appGlobal='oopif';</script>".to_owned();
        let parent_server = tokio::spawn(serve_live_locator_fixture(
            parent_listener,
            parent_body,
            same_body,
        ));
        let oopif_server = tokio::spawn(serve_live_locator_fixture(
            oopif_listener,
            oopif_body,
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
        let main = page.main_frame().await.unwrap();
        let main_session = main.cdp_session().await.unwrap().id().to_owned();
        let frames = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let frames = page.frames().await.unwrap();
                let mut has_oopif = false;
                for frame in &frames {
                    if frame
                        .cdp_session()
                        .await
                        .is_ok_and(|route| route.id() != main_session)
                    {
                        has_oopif = true;
                    }
                }
                if frames.len() >= 3 && has_oopif {
                    break frames;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
        let mut same = None;
        let mut oopif = None;
        for frame in frames {
            if frame.id() == main.id() {
                continue;
            }
            let route = frame.cdp_session().await.unwrap();
            if route.id() == main_session {
                same = Some(frame);
            } else {
                oopif = Some(frame);
            }
        }
        let same = same.unwrap();
        let oopif = oopif.unwrap();
        assert_eq!(
            page.evaluate::<String>("globalThis.appGlobal")
                .await
                .unwrap(),
            "main"
        );
        assert_eq!(
            same.evaluate::<String>("globalThis.appGlobal")
                .await
                .unwrap(),
            "same"
        );
        assert_eq!(
            oopif
                .evaluate::<String>("globalThis.appGlobal")
                .await
                .unwrap(),
            "oopif"
        );
        assert_eq!(
            page.evaluate::<i64>("Promise.resolve(42)").await.unwrap(),
            42
        );
        let exception = page
            .evaluate_value("Promise.reject(new Error('boom'))")
            .await
            .unwrap_err();
        assert!(exception.javascript_exception().is_some());
        let async_exception = page
            .evaluate_value(
                "(async function outer(){ async function inner(){ await new Promise(resolve => setTimeout(resolve, 0)); throw new Error('async boom'); } await inner(); })()",
            )
            .await
            .unwrap_err();
        assert!(async_exception
            .javascript_exception()
            .unwrap()
            .stack()
            .iter()
            .any(|frame| frame.function_name == "outer"));
        let handle = page
            .evaluate_handle("({nested:{answer:42}, add(value){return this.nested.answer+value}})")
            .await
            .unwrap();
        let nested = handle.property("nested").await.unwrap();
        assert_eq!(nested.json_value().await.unwrap(), json!({"answer":42}));
        let sum = handle
            .call(
                "function(value){return this.add(value)}",
                [EvaluationArgument::json(8).unwrap()],
            )
            .await
            .unwrap();
        assert_eq!(sum.json_value().await.unwrap(), json!(50));
        sum.release().await.unwrap();
        nested.release().await.unwrap();
        handle.release().await.unwrap();

        assert!(page.close().await.is_complete());
        assert!(session.close().await.is_complete());
        assert!(runtime.close().await.is_complete());
        parent_server.abort();
        oopif_server.abort();
    }
}
