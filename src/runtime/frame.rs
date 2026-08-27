use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use cdpkit::page::events::{
    FileChooserOpened, FrameAttached, FrameDetached, FrameNavigated, JavascriptDialogClosed,
    JavascriptDialogOpening, NavigatedWithinDocument,
};
use cdpkit::page::methods::{Enable, GetFrameTree, SetInterceptFileChooserDialog};
use cdpkit::page::types::{FrameDetachedReason, FrameTree};
use cdpkit::runtime::events::{
    ConsoleApiCalled, ExceptionThrown, ExecutionContextCreated, ExecutionContextDestroyed,
    ExecutionContextsCleared,
};
use cdpkit::runtime::methods::{
    Enable as RuntimeEnable, RunIfWaitingForDebugger, SetAsyncCallStackDepth,
};
use cdpkit::target::events::{AttachedToTarget, DetachedFromTarget};
use cdpkit::target::methods::{DetachFromTarget, SetAutoAttach};
use futures::{FutureExt, StreamExt};
use parking_lot::{Mutex, RwLock};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::runtime::{
    console_message, javascript_error, BrowserError, DocumentEpoch, FrameId, InvalidationReason,
    Page, PageEvent, PageGeneration, PageInner,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameSnapshot {
    pub page_generation: PageGeneration,
    pub document_epoch: DocumentEpoch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FrameScopeIdentity {
    frame_id: FrameId,
    snapshot: FrameSnapshot,
}

impl FrameScopeIdentity {
    pub(crate) fn frame_id(&self) -> &FrameId {
        &self.frame_id
    }
    pub(crate) fn snapshot(&self) -> FrameSnapshot {
        self.snapshot
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FrameStoreIdentity {
    page_generation: PageGeneration,
}

impl FrameStoreIdentity {
    fn new(page_generation: PageGeneration) -> Self {
        Self { page_generation }
    }

    fn snapshot(self, graph: &FrameGraph, frame_id: &str) -> Option<FrameSnapshot> {
        graph.snapshot(frame_id, self.page_generation)
    }
}

fn should_remove_detached_frame(reason: &FrameDetachedReason) -> bool {
    matches!(reason, FrameDetachedReason::Remove)
}

fn close_page_event_source(store: &Weak<FrameStore>) {
    if let Some(page) = store.upgrade().and_then(|store| store.page()) {
        page.close_event_source();
    }
}

#[derive(Debug, Clone)]
struct FrameRecord {
    parent: Option<String>,
    children: BTreeSet<String>,
    loader_id: Option<String>,
    document_epoch: DocumentEpoch,
    route_session_id: String,
    route_target_id: Option<String>,
    route_active: bool,
}

#[derive(Debug)]
pub(crate) struct FrameGraph {
    main_session_id: String,
    main_frame_id: Option<String>,
    frames: HashMap<String, FrameRecord>,
}

impl FrameGraph {
    fn new(main_session_id: impl Into<String>) -> Self {
        Self {
            main_session_id: main_session_id.into(),
            main_frame_id: None,
            frames: HashMap::new(),
        }
    }

    fn attach(&mut self, frame_id: &str, parent: Option<&str>) {
        let is_new = !self.frames.contains_key(frame_id);
        let waiting_children = self
            .frames
            .iter()
            .filter(|(_, child)| child.parent.as_deref() == Some(frame_id))
            .map(|(child_id, _)| child_id.clone())
            .collect::<Vec<_>>();
        let record = self
            .frames
            .entry(frame_id.to_owned())
            .or_insert_with(|| FrameRecord {
                parent: parent.map(str::to_owned),
                children: BTreeSet::new(),
                loader_id: None,
                document_epoch: DocumentEpoch::initial(),
                route_session_id: self.main_session_id.clone(),
                route_target_id: None,
                route_active: true,
            });
        if record.parent.is_none() {
            record.parent = parent.map(str::to_owned);
        }
        record.children.extend(waiting_children);
        if let Some(parent) = parent {
            if let Some(parent_record) = self.frames.get_mut(parent) {
                parent_record.children.insert(frame_id.to_owned());
            }
        } else if is_new || self.main_frame_id.is_none() {
            self.main_frame_id = Some(frame_id.to_owned());
        }
    }

    fn navigate(&mut self, frame_id: &str, parent: Option<&str>, loader_id: &str) -> bool {
        self.attach(frame_id, parent);
        let record = self.frames.get_mut(frame_id).expect("attached above");
        let cross_document = record
            .loader_id
            .as_deref()
            .is_some_and(|previous| previous != loader_id);
        if cross_document {
            record.document_epoch = DocumentEpoch::new(record.document_epoch.get() + 1);
        }
        record.loader_id = Some(loader_id.to_owned());
        cross_document
    }

    fn detach(&mut self, frame_id: &str) {
        let Some(record) = self.frames.remove(frame_id) else {
            return;
        };
        for child in record.children {
            self.detach(&child);
        }
        if let Some(parent) = record.parent {
            if let Some(parent_record) = self.frames.get_mut(&parent) {
                parent_record.children.remove(frame_id);
            }
        }
        if self.main_frame_id.as_deref() == Some(frame_id) {
            self.main_frame_id = None;
        }
    }

    fn route_to_session(
        &mut self,
        frame_id: &str,
        session_id: &str,
        target_id: Option<&str>,
    ) -> Option<String> {
        self.attach(frame_id, None);
        if let Some(record) = self.frames.get_mut(frame_id) {
            let previous = record.route_session_id.clone();
            record.route_session_id = session_id.to_owned();
            record.route_target_id = target_id.map(str::to_owned);
            return Some(previous);
        }
        None
    }

    fn route_oopif(
        &mut self,
        frame_id: &str,
        parent_frame_id: Option<&str>,
        parent_session_id: Option<&str>,
        session_id: &str,
        target_id: &str,
    ) -> bool {
        if !self.can_route_oopif(frame_id, parent_frame_id, parent_session_id) {
            return false;
        }
        let parent_frame_id = parent_frame_id.expect("validated above");
        self.attach(frame_id, Some(parent_frame_id));
        let _ = self.route_to_session(frame_id, session_id, Some(target_id));
        true
    }

    fn can_route_oopif(
        &self,
        frame_id: &str,
        parent_frame_id: Option<&str>,
        parent_session_id: Option<&str>,
    ) -> bool {
        let Some(parent_frame_id) = parent_frame_id else {
            return false;
        };
        let Some(parent) = self.frames.get(parent_frame_id) else {
            return false;
        };
        if parent_session_id != Some(parent.route_session_id.as_str()) {
            return false;
        }
        if self
            .frames
            .get(frame_id)
            .and_then(|frame| frame.parent.as_deref())
            .is_some_and(|existing_parent| existing_parent != parent_frame_id)
        {
            return false;
        }
        if self
            .frames
            .get(frame_id)
            .is_some_and(|frame| frame.route_session_id != parent.route_session_id)
        {
            return false;
        }
        true
    }

    fn snapshot(&self, frame_id: &str, page_generation: PageGeneration) -> Option<FrameSnapshot> {
        self.frames.get(frame_id).map(|record| FrameSnapshot {
            page_generation,
            document_epoch: record.document_epoch,
        })
    }

    pub(crate) fn main_frame_id(&self) -> Option<&str> {
        self.main_frame_id.as_deref()
    }

    #[cfg(test)]
    fn contains(&self, frame_id: &str) -> bool {
        self.frames.contains_key(frame_id)
    }

    fn children(&self, frame_id: &str) -> Vec<&str> {
        self.frames
            .get(frame_id)
            .map(|record| record.children.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }

    fn parent(&self, frame_id: &str) -> Option<&str> {
        self.frames.get(frame_id)?.parent.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn ids(&self) -> Vec<String> {
        let mut ids = self.frames.keys().cloned().collect::<Vec<_>>();
        ids.sort_unstable();
        ids
    }

    fn route_session(&self, frame_id: &str) -> Option<&str> {
        self.frames
            .get(frame_id)
            .map(|record| record.route_session_id.as_str())
    }

    fn set_route_active(&mut self, frame_id: &str, active: bool) {
        if let Some(record) = self.frames.get_mut(frame_id) {
            record.route_active = active;
        }
    }

    fn is_route_active(&self, frame_id: &str) -> bool {
        self.frames
            .get(frame_id)
            .is_some_and(|record| record.route_active)
    }

    fn active_ids(&self) -> Vec<String> {
        let mut ids = self
            .frames
            .iter()
            .filter(|(_, record)| record.route_active)
            .map(|(frame_id, _)| frame_id.clone())
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids
    }

    fn loader_id(&self, frame_id: &str) -> Option<&str> {
        self.frames.get(frame_id)?.loader_id.as_deref()
    }

    fn reroute_session(&mut self, session_id: &str, fallback_session_id: &str) {
        for record in self.frames.values_mut() {
            if record.route_session_id == session_id {
                record.route_session_id = fallback_session_id.to_owned();
                record.route_target_id = None;
            }
        }
    }
}

fn index_frame_tree<'tree>(
    tree: &'tree FrameTree,
    index: &mut HashMap<&'tree str, &'tree FrameTree>,
    ids: &mut BTreeSet<String>,
) -> usize {
    index.insert(tree.frame.id.as_str(), tree);
    ids.insert(tree.frame.id.clone());
    1 + tree
        .child_frames
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|child| index_frame_tree(child, index, ids))
        .sum::<usize>()
}

fn collect_session_subtree(
    parent_by_session: &HashMap<String, String>,
    root_session_id: &str,
) -> BTreeSet<String> {
    let mut subtree = BTreeSet::from([root_session_id.to_owned()]);
    loop {
        let before = subtree.len();
        for (session_id, parent_session_id) in parent_by_session {
            if subtree.contains(parent_session_id) {
                subtree.insert(session_id.clone());
            }
        }
        if subtree.len() == before {
            return subtree;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionAttachDisposition {
    New,
    Idempotent,
    Conflict,
}

fn classify_session_attach(
    existing: Option<(&str, &str)>,
    session_id: &str,
    root_frame_id: &str,
    parent_session_id: Option<&str>,
) -> SessionAttachDisposition {
    if parent_session_id == Some(session_id) {
        return SessionAttachDisposition::Conflict;
    }
    match (existing, parent_session_id) {
        (None, Some(_)) => SessionAttachDisposition::New,
        (None, None) => SessionAttachDisposition::Conflict,
        (Some((existing_root, existing_parent)), Some(parent))
            if existing_root == root_frame_id && existing_parent == parent =>
        {
            SessionAttachDisposition::Idempotent
        }
        (Some(_), _) => SessionAttachDisposition::Conflict,
    }
}

fn resolve_live_parent(
    parent_by_session: &HashMap<String, String>,
    live_sessions: &BTreeSet<String>,
    removing: &BTreeSet<String>,
    session_id: &str,
    main_session_id: &str,
) -> String {
    debug_assert!(live_sessions.contains(main_session_id));
    let mut visited = BTreeSet::from([session_id.to_owned()]);
    let mut candidate = parent_by_session.get(session_id).cloned();
    while let Some(session_id) = candidate {
        if !visited.insert(session_id.clone()) {
            break;
        }
        if !removing.contains(&session_id) && live_sessions.contains(&session_id) {
            return session_id;
        }
        candidate = parent_by_session.get(&session_id).cloned();
    }
    main_session_id.to_owned()
}

struct FrameEventStreams {
    attached: cdpkit::EventStream<FrameAttached>,
    detached: cdpkit::EventStream<FrameDetached>,
    navigated: cdpkit::EventStream<FrameNavigated>,
    same_document: cdpkit::EventStream<NavigatedWithinDocument>,
    console: cdpkit::EventStream<ConsoleApiCalled>,
    exception: cdpkit::EventStream<ExceptionThrown>,
    dialog_opened: cdpkit::EventStream<JavascriptDialogOpening>,
    dialog_closed: cdpkit::EventStream<JavascriptDialogClosed>,
    file_chooser_opened: cdpkit::EventStream<FileChooserOpened>,
    execution_contexts: cdpkit::RawEventStream,
}

#[derive(Debug, Clone)]
pub(super) struct MainWorldContext {
    pub(super) id: i64,
    pub(super) unique_id: String,
}

#[derive(Debug, Clone)]
struct ExecutionContextRecord {
    frame_id: String,
    session_id: String,
    context: MainWorldContext,
}

struct FrameState {
    graph: FrameGraph,
    sessions: HashMap<String, cdpkit::Session>,
    child_sessions: HashMap<String, ChildSessionOwnership>,
    next_attach_token: u64,
    execution_contexts: Vec<ExecutionContextRecord>,
}

struct FrameRouteChange {
    frame_id: String,
    previous_session_id: String,
    session_id: String,
}

#[derive(Default)]
struct DetachedChildSessions {
    route_changes: Vec<FrameRouteChange>,
    session_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AttachToken(u64);

impl AttachToken {
    fn new(value: u64) -> Self {
        Self(value)
    }

    #[cfg(test)]
    fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildSessionPhase {
    Initializing(AttachToken),
    Active(AttachToken),
    Detached(AttachToken),
}

struct ChildSessionOwnership {
    root_frame_id: String,
    parent_session_id: String,
    reducer_cancel: CancellationToken,
    phase: ChildSessionPhase,
}

struct InitialOopifAttach {
    parent_session_id: Option<String>,
    event: AttachedToTarget,
}

struct MalformedAttachedTarget {
    parent_session_id: Option<String>,
    session_id: Option<String>,
    target_id: Option<String>,
    target_type: Option<String>,
    target_url: Option<String>,
    waiting_for_debugger: Option<bool>,
    parse_error: String,
    field_errors: Vec<super::TargetFieldError>,
}

enum AttachedTargetResult {
    Typed(InitialOopifAttach),
    Malformed(MalformedAttachedTarget),
}

impl AttachedTargetResult {
    fn from_raw(parent_session_id: Option<String>, params: Value) -> Self {
        match serde_json::from_value::<AttachedToTarget>(params.clone()) {
            Ok(event) => Self::Typed(InitialOopifAttach {
                parent_session_id,
                event,
            }),
            Err(error) => {
                let mut field_errors = Vec::new();
                let session_id = extract_attached_string_field(
                    params.get("sessionId"),
                    "sessionId",
                    &mut field_errors,
                );
                let target_info = match params.get("targetInfo") {
                    Some(Value::Object(target_info)) => Some(target_info),
                    Some(value) => {
                        field_errors.push(super::TargetFieldError::new(
                            "targetInfo",
                            format!("expected object, found {}", json_value_kind(value)),
                        ));
                        None
                    }
                    None => {
                        field_errors.push(super::TargetFieldError::new(
                            "targetInfo",
                            "field is missing",
                        ));
                        None
                    }
                };
                let target_id = extract_attached_string_field(
                    target_info.and_then(|target| target.get("targetId")),
                    "targetInfo.targetId",
                    &mut field_errors,
                );
                let target_type = extract_attached_string_field(
                    target_info.and_then(|target| target.get("type")),
                    "targetInfo.type",
                    &mut field_errors,
                );
                let target_url = extract_attached_string_field(
                    target_info.and_then(|target| target.get("url")),
                    "targetInfo.url",
                    &mut field_errors,
                );
                let waiting_for_debugger = extract_attached_bool_field(
                    params.get("waitingForDebugger"),
                    "waitingForDebugger",
                    &mut field_errors,
                );
                Self::Malformed(MalformedAttachedTarget {
                    parent_session_id,
                    session_id,
                    target_id,
                    target_type,
                    target_url,
                    waiting_for_debugger,
                    parse_error: error.to_string(),
                    field_errors,
                })
            }
        }
    }

    fn target_id(&self) -> Option<&str> {
        match self {
            Self::Typed(attached) => Some(attached.event.target_info.target_id.as_str()),
            Self::Malformed(attached) => attached.target_id.as_deref(),
        }
    }

    fn is_iframe(&self) -> bool {
        matches!(self, Self::Typed(attached) if attached.event.target_info.type_ == "iframe")
    }
}

fn extract_attached_string_field(
    value: Option<&Value>,
    field: &str,
    errors: &mut Vec<super::TargetFieldError>,
) -> Option<String> {
    match value {
        Some(Value::String(value)) => Some(value.clone()),
        Some(value) => {
            errors.push(super::TargetFieldError::new(
                field,
                format!("expected string, found {}", json_value_kind(value)),
            ));
            None
        }
        None => {
            errors.push(super::TargetFieldError::new(field, "field is missing"));
            None
        }
    }
}

fn extract_attached_bool_field(
    value: Option<&Value>,
    field: &str,
    errors: &mut Vec<super::TargetFieldError>,
) -> Option<bool> {
    match value {
        Some(Value::Bool(value)) => Some(*value),
        Some(value) => {
            errors.push(super::TargetFieldError::new(
                field,
                format!("expected boolean, found {}", json_value_kind(value)),
            ));
            None
        }
        None => {
            errors.push(super::TargetFieldError::new(field, "field is missing"));
            None
        }
    }
}

fn json_value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

struct OopifAttachClaim {
    token: AttachToken,
    cancel: CancellationToken,
}

enum OopifAttachStart {
    Initialize(OopifAttachClaim),
    Idempotent { active: bool },
    Conflict,
    ForeignParent,
    RouteUnavailable,
}

impl FrameState {
    fn routed_session(&self, frame_id: &str) -> Option<cdpkit::Session> {
        let session_id = self.graph.route_session(frame_id)?;
        self.sessions.get(session_id).cloned()
    }

    fn set_default_context(
        &mut self,
        session_id: &str,
        frame_id: &str,
        id: i64,
        unique_id: String,
    ) {
        self.execution_contexts
            .retain(|record| record.session_id != session_id || record.frame_id != frame_id);
        self.execution_contexts.push(ExecutionContextRecord {
            frame_id: frame_id.to_owned(),
            session_id: session_id.to_owned(),
            context: MainWorldContext { id, unique_id },
        });
    }

    fn default_context(&self, session_id: &str, frame_id: &str) -> Option<MainWorldContext> {
        self.execution_contexts
            .iter()
            .find(|record| record.session_id == session_id && record.frame_id == frame_id)
            .map(|record| record.context.clone())
    }

    fn remove_context(&mut self, session_id: &str, id: i64, unique_id: &str) {
        self.execution_contexts.retain(|record| {
            record.session_id != session_id
                || if unique_id.is_empty() {
                    record.context.id != id
                } else {
                    record.context.unique_id != unique_id
                }
        });
    }

    fn clear_contexts(&mut self, session_id: &str) {
        self.execution_contexts
            .retain(|record| record.session_id != session_id);
    }

    fn begin_oopif_attach(
        &mut self,
        session_id: &str,
        root_frame_id: &str,
        parent_frame_id: Option<&str>,
        parent_session_id: Option<&str>,
        cancel: CancellationToken,
    ) -> OopifAttachStart {
        let existing = self.child_sessions.get(session_id).map(|ownership| {
            (
                ownership.root_frame_id.as_str(),
                ownership.parent_session_id.as_str(),
            )
        });
        match classify_session_attach(existing, session_id, root_frame_id, parent_session_id) {
            SessionAttachDisposition::Idempotent => {
                let active = self
                    .child_sessions
                    .get(session_id)
                    .is_some_and(|ownership| {
                        matches!(ownership.phase, ChildSessionPhase::Active(_))
                    });
                return OopifAttachStart::Idempotent { active };
            }
            SessionAttachDisposition::Conflict => return OopifAttachStart::Conflict,
            SessionAttachDisposition::New => {}
        }
        let parent_belongs_to_page = parent_frame_id
            .and_then(|parent_frame_id| self.graph.frames.get(parent_frame_id))
            .is_some_and(|parent| parent_session_id == Some(parent.route_session_id.as_str()));
        if !parent_belongs_to_page {
            return OopifAttachStart::ForeignParent;
        }
        if !self
            .graph
            .can_route_oopif(root_frame_id, parent_frame_id, parent_session_id)
        {
            return OopifAttachStart::RouteUnavailable;
        }

        self.next_attach_token = self.next_attach_token.wrapping_add(1);
        let token = AttachToken::new(self.next_attach_token);
        self.child_sessions.insert(
            session_id.to_owned(),
            ChildSessionOwnership {
                root_frame_id: root_frame_id.to_owned(),
                parent_session_id: parent_session_id
                    .expect("validated OOPIF parent session")
                    .to_owned(),
                reducer_cancel: cancel.clone(),
                phase: ChildSessionPhase::Initializing(token),
            },
        );
        OopifAttachStart::Initialize(OopifAttachClaim { token, cancel })
    }

    fn is_initializing_oopif_attach(&self, session_id: &str, token: AttachToken) -> bool {
        self.child_sessions
            .get(session_id)
            .is_some_and(|ownership| ownership.phase == ChildSessionPhase::Initializing(token))
    }

    fn activate_oopif_attach(&mut self, session_id: &str, token: AttachToken) -> bool {
        let Some(ownership) = self.child_sessions.get_mut(session_id) else {
            return false;
        };
        if ownership.phase != ChildSessionPhase::Initializing(token) {
            return false;
        }
        ownership.phase = ChildSessionPhase::Active(token);
        true
    }

    fn abandon_oopif_attach(&mut self, session_id: &str, token: AttachToken) -> bool {
        let Some(ownership) = self.child_sessions.get_mut(session_id) else {
            return false;
        };
        if ownership.phase != ChildSessionPhase::Initializing(token) {
            return false;
        }
        ownership.reducer_cancel.cancel();
        ownership.phase = ChildSessionPhase::Detached(token);
        true
    }

    fn acknowledge_detached(&mut self, session_id: &str, token: AttachToken) -> bool {
        if !self
            .child_sessions
            .get(session_id)
            .is_some_and(|ownership| ownership.phase == ChildSessionPhase::Detached(token))
        {
            return false;
        }
        self.child_sessions.remove(session_id);
        true
    }

    #[cfg(test)]
    fn child_session_phase(&self, session_id: &str) -> Option<ChildSessionPhase> {
        self.child_sessions
            .get(session_id)
            .map(|ownership| ownership.phase)
    }

    fn detach_child_session(&mut self, session_id: &str) -> DetachedChildSessions {
        if !self
            .child_sessions
            .get(session_id)
            .is_some_and(|ownership| !matches!(ownership.phase, ChildSessionPhase::Detached(_)))
        {
            return DetachedChildSessions::default();
        }
        let routes_before = self
            .graph
            .frames
            .iter()
            .map(|(frame_id, record)| (frame_id.clone(), record.route_session_id.clone()))
            .collect::<HashMap<_, _>>();
        let parent_by_session = self
            .child_sessions
            .iter()
            .filter(|(_, ownership)| !matches!(ownership.phase, ChildSessionPhase::Detached(_)))
            .map(|(child_session_id, ownership)| {
                (
                    child_session_id.clone(),
                    ownership.parent_session_id.clone(),
                )
            })
            .collect::<HashMap<_, _>>();
        let subtree = collect_session_subtree(&parent_by_session, session_id);
        let detached_session_ids = subtree.iter().cloned().collect::<Vec<_>>();
        let mut live_sessions = self.sessions.keys().cloned().collect::<BTreeSet<_>>();
        live_sessions.insert(self.graph.main_session_id.clone());
        for removed_session_id in &subtree {
            let fallback_session_id = resolve_live_parent(
                &parent_by_session,
                &live_sessions,
                &subtree,
                removed_session_id,
                &self.graph.main_session_id,
            );
            self.graph
                .reroute_session(removed_session_id, &fallback_session_id);
        }
        for removed_session_id in subtree {
            self.sessions.remove(&removed_session_id);
            self.clear_contexts(&removed_session_id);
            let phase = self
                .child_sessions
                .get(&removed_session_id)
                .map(|ownership| ownership.phase);
            match phase {
                Some(ChildSessionPhase::Initializing(token)) => {
                    let ownership = self
                        .child_sessions
                        .get_mut(&removed_session_id)
                        .expect("phase read above");
                    ownership.reducer_cancel.cancel();
                    ownership.phase = ChildSessionPhase::Detached(token);
                }
                Some(ChildSessionPhase::Active(_)) => {
                    if let Some(ownership) = self.child_sessions.remove(&removed_session_id) {
                        ownership.reducer_cancel.cancel();
                    }
                }
                Some(ChildSessionPhase::Detached(_)) | None => {}
            }
        }
        let route_changes = self
            .graph
            .frames
            .iter()
            .filter_map(|(frame_id, record)| {
                let previous_session_id = routes_before.get(frame_id)?;
                (previous_session_id != &record.route_session_id).then(|| FrameRouteChange {
                    frame_id: frame_id.clone(),
                    previous_session_id: previous_session_id.clone(),
                    session_id: record.route_session_id.clone(),
                })
            })
            .collect();
        DetachedChildSessions {
            route_changes,
            session_ids: detached_session_ids,
        }
    }
}

fn is_auxiliary_worker_target(target_type: &str) -> bool {
    matches!(
        target_type,
        "worker"
            | "shared_worker"
            | "service_worker"
            | "worklet"
            | "shared_storage_worklet"
            | "auction_worklet"
    )
}

enum AuxiliaryTargetOwnership {
    Pending(super::PendingOwnershipGuard),
    Retained(super::RetainedOwnership),
}

struct AuxiliaryTargetRecord {
    token: u64,
    target_id: String,
    target_type: String,
    direct_parent_session_id: Option<String>,
    network_route: bool,
    ownership: Option<AuxiliaryTargetOwnership>,
}

#[derive(Debug)]
struct AuxiliaryTargetClaim {
    session_id: String,
    token: u64,
}

struct DetachedAuxiliaryTarget {
    session_id: String,
    network_route: bool,
    ownership: AuxiliaryTargetOwnership,
}

impl DetachedAuxiliaryTarget {
    fn disarm(self) {
        match self.ownership {
            AuxiliaryTargetOwnership::Pending(ownership) => ownership.disarm(),
            AuxiliaryTargetOwnership::Retained(ownership) => ownership.disarm(),
        }
    }

    async fn cleanup(self) -> Result<(), super::OwnershipCleanupError> {
        match self.ownership {
            AuxiliaryTargetOwnership::Pending(ownership) => ownership.cleanup().await,
            AuxiliaryTargetOwnership::Retained(ownership) => ownership.cleanup().await,
        }
    }
}

/// Page-owned attachments that carry no FrameGraph or public target-route identity.
///
/// Direct parent session identity is retained so nested worker families remain
/// attributable without making workers visible as Page/Frame/OOPIF routes.
pub(crate) struct AuxiliaryTargetRegistry {
    next_token: AtomicU64,
    pending: super::PendingOwnershipRegistry,
    records: Mutex<HashMap<String, AuxiliaryTargetRecord>>,
}

impl AuxiliaryTargetRegistry {
    fn new() -> Self {
        Self {
            next_token: AtomicU64::new(1),
            pending: super::PendingOwnershipRegistry::new(),
            records: Mutex::new(HashMap::new()),
        }
    }

    fn begin(
        &self,
        cdp: &cdpkit::CDP,
        parent_session_id: Option<&str>,
        session_id: String,
        target_id: String,
        target_type: String,
    ) -> Option<AuxiliaryTargetClaim> {
        let mut records = self.records.lock();
        if records.contains_key(&session_id) {
            return None;
        }
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        let parent_session = parent_session_id.map(|id| cdp.session(id.to_owned()));
        let cdp = cdp.clone();
        let detach_session_id = session_id.clone();
        let ownership = self.pending.register(
            format!("auxiliary-target-detach:{session_id}"),
            move || async move {
                let command = DetachFromTarget::new().with_session_id(detach_session_id);
                match parent_session {
                    Some(parent_session) => command.send(&parent_session).await,
                    None => command.send(&cdp).await,
                }
                .map_err(super::OwnershipCleanupError::from)
            },
        );
        records.insert(
            session_id.clone(),
            AuxiliaryTargetRecord {
                token,
                target_id,
                target_type,
                direct_parent_session_id: parent_session_id.map(str::to_owned),
                network_route: false,
                ownership: Some(AuxiliaryTargetOwnership::Pending(ownership)),
            },
        );
        Some(AuxiliaryTargetClaim { session_id, token })
    }

    fn mark_network_route(&self, claim: &AuxiliaryTargetClaim) -> bool {
        let mut records = self.records.lock();
        let Some(record) = records.get_mut(&claim.session_id) else {
            return false;
        };
        if record.token != claim.token {
            return false;
        }
        record.network_route = true;
        true
    }

    fn retain(&self, claim: AuxiliaryTargetClaim) -> bool {
        let mut records = self.records.lock();
        let Some(record) = records.get_mut(&claim.session_id) else {
            return false;
        };
        if record.token != claim.token {
            return false;
        }
        let ownership = record
            .ownership
            .take()
            .expect("auxiliary target ownership is armed");
        record.ownership = Some(match ownership {
            AuxiliaryTargetOwnership::Pending(ownership) => {
                AuxiliaryTargetOwnership::Retained(ownership.retain())
            }
            AuxiliaryTargetOwnership::Retained(ownership) => {
                AuxiliaryTargetOwnership::Retained(ownership)
            }
        });
        tracing::debug!(
            session_id = %claim.session_id,
            target_id = %record.target_id,
            target_type = %record.target_type,
            direct_parent_session_id = ?record.direct_parent_session_id,
            network_route = record.network_route,
            "retained auxiliary target attachment"
        );
        true
    }

    fn take(&self, session_id: &str) -> Option<DetachedAuxiliaryTarget> {
        self.records
            .lock()
            .remove(session_id)
            .map(|record| DetachedAuxiliaryTarget {
                session_id: session_id.to_owned(),
                network_route: record.network_route,
                ownership: record
                    .ownership
                    .expect("auxiliary target ownership is armed"),
            })
    }

    fn has_network_route(&self, session_id: &str) -> bool {
        self.records
            .lock()
            .get(session_id)
            .is_some_and(|record| record.network_route)
    }

    #[cfg(test)]
    fn direct_parent_session_id(&self, session_id: &str) -> Option<String> {
        self.records
            .lock()
            .get(session_id)
            .and_then(|record| record.direct_parent_session_id.clone())
    }

    fn take_all(&self) -> Vec<DetachedAuxiliaryTarget> {
        std::mem::take(&mut *self.records.lock())
            .into_iter()
            .map(|(session_id, record)| DetachedAuxiliaryTarget {
                session_id,
                network_route: record.network_route,
                ownership: record
                    .ownership
                    .expect("auxiliary target ownership is armed"),
            })
            .collect()
    }

    async fn cleanup_after_parent_destroyed(
        &self,
    ) -> Vec<(String, Result<(), super::OwnershipCleanupError>)> {
        let records = std::mem::take(&mut *self.records.lock());
        for (session_id, record) in records {
            let attachment = DetachedAuxiliaryTarget {
                session_id,
                network_route: record.network_route,
                ownership: record
                    .ownership
                    .expect("auxiliary target ownership is armed"),
            };
            if record.direct_parent_session_id.is_some() {
                attachment.disarm();
            } else {
                let _ = attachment.cleanup().await;
            }
        }
        self.pending.cleanup_all().await
    }

    #[cfg(test)]
    fn schedule_all(&self) {
        for attachment in self.take_all() {
            match attachment.ownership {
                AuxiliaryTargetOwnership::Pending(ownership) => drop(ownership),
                AuxiliaryTargetOwnership::Retained(ownership) => ownership.schedule(),
            }
        }
    }

    async fn cleanup_all(&self) -> Vec<(String, Result<(), super::OwnershipCleanupError>)> {
        self.pending.cleanup_all().await
    }
}

fn apply_runtime_context_event(
    state: &mut FrameState,
    routed_session_id: &str,
    method: &str,
    params: Value,
) -> Result<bool, serde_json::Error> {
    match method {
        "Runtime.executionContextCreated" => {
            let event: ExecutionContextCreated = serde_json::from_value(params)?;
            let aux = event.context.aux_data.as_ref();
            let is_default = aux
                .and_then(|value| value.get("isDefault"))
                .and_then(Value::as_bool)
                == Some(true);
            let frame_id = aux
                .and_then(|value| value.get("frameId"))
                .and_then(Value::as_str);
            if let (true, Some(frame_id)) = (is_default, frame_id) {
                state.set_default_context(
                    routed_session_id,
                    frame_id,
                    event.context.id,
                    event.context.unique_id,
                );
                Ok(true)
            } else {
                Ok(false)
            }
        }
        "Runtime.executionContextDestroyed" => {
            #[allow(deprecated)]
            let event: ExecutionContextDestroyed = serde_json::from_value(params)?;
            #[allow(deprecated)]
            state.remove_context(
                routed_session_id,
                event.execution_context_id,
                &event.execution_context_unique_id,
            );
            Ok(true)
        }
        "Runtime.executionContextsCleared" => {
            let _: ExecutionContextsCleared = serde_json::from_value(params)?;
            state.clear_contexts(routed_session_id);
            Ok(true)
        }
        _ => Ok(false),
    }
}

#[cfg(test)]
type MainDocumentReducerGate = Option<(String, Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>;

#[cfg(test)]
static MAIN_DOCUMENT_REDUCER_GATE: std::sync::OnceLock<Mutex<MainDocumentReducerGate>> =
    std::sync::OnceLock::new();

#[cfg(test)]
pub(crate) fn gate_main_document_reducer(
    loader_id: &str,
) -> (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>) {
    let seen = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    *MAIN_DOCUMENT_REDUCER_GATE
        .get_or_init(|| Mutex::new(None))
        .lock() = Some((
        loader_id.to_owned(),
        Arc::clone(&seen),
        Arc::clone(&release),
    ));
    (seen, release)
}

#[cfg(test)]
async fn wait_for_main_document_reducer_gate(loader_id: &str) {
    let gate = MAIN_DOCUMENT_REDUCER_GATE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .as_ref()
        .filter(|(expected, _, _)| expected == loader_id)
        .map(|(_, seen, release)| (Arc::clone(seen), Arc::clone(release)));
    if let Some((seen, release)) = gate {
        seen.notify_one();
        release.notified().await;
        MAIN_DOCUMENT_REDUCER_GATE
            .get_or_init(|| Mutex::new(None))
            .lock()
            .take();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AppliedMainDocument {
    frame_id: String,
    loader_id: String,
}

pub(crate) struct FrameStore {
    page: Weak<PageInner>,
    runtime: super::BrowserRuntime,
    identity: FrameStoreIdentity,
    main_document_applied: tokio::sync::broadcast::Sender<AppliedMainDocument>,
    route_context_options: Option<super::ContextOptions>,
    pending_oopif_initializations: super::PendingOwnershipRegistry,
    auto_attached_target_cleanups: super::PendingOwnershipRegistry,
    auxiliary_targets: AuxiliaryTargetRegistry,
    state: RwLock<FrameState>,
    cancel: CancellationToken,
    runtime_events_lock: tokio::sync::Mutex<()>,
    runtime_events_enabled: tokio::sync::OnceCell<()>,
    runtime_events_requested: AtomicBool,
    context_changed: tokio::sync::Notify,
    network_lock: tokio::sync::Mutex<()>,
    network_requested: AtomicBool,
    file_chooser_state: tokio::sync::Mutex<super::file_chooser::FileChooserInterceptionState>,
    file_chooser_opened: tokio::sync::broadcast::Sender<super::file_chooser::FileChooserOpenedFact>,
}

#[derive(Clone)]
pub(crate) struct LocatorFrameRoute {
    pub(super) page_generation: PageGeneration,
    pub(super) document_epoch: DocumentEpoch,
    pub(super) frame_id: FrameId,
    pub(super) session_id: String,
    pub(super) session: cdpkit::Session,
    pub(super) loader_id: String,
}

pub(super) struct AuthoritativeFrameIdentity {
    pub(super) parent_id: Option<String>,
    pub(super) child_ids: Vec<String>,
}

pub(super) struct AuthoritativeFrameBatch {
    pub(super) frame_ids: BTreeSet<String>,
    pub(super) identities: HashMap<String, AuthoritativeFrameIdentity>,
    #[cfg(test)]
    pub(super) indexed_frame_nodes: usize,
}

impl FrameStore {
    fn track_unowned_oopif_initialization(
        &self,
        session_id: String,
    ) -> super::PendingOwnershipGuard {
        let cdp = self.runtime.cdp().clone();
        self.pending_oopif_initializations.register(
            format!("oopif-initialization:{session_id}"),
            move || async move {
                DetachFromTarget::new()
                    .with_session_id(session_id)
                    .send(&cdp)
                    .await
                    .map_err(super::OwnershipCleanupError::from)
            },
        )
    }

    fn track_auto_attached_target(
        &self,
        parent_session_id: Option<&str>,
        session_id: String,
    ) -> super::PendingOwnershipGuard {
        let cdp = self.runtime.cdp().clone();
        let parent_session = parent_session_id.map(|id| cdp.session(id.to_owned()));
        self.auto_attached_target_cleanups.register(
            format!("auto-attached-target-detach:{session_id}"),
            move || async move {
                let command = DetachFromTarget::new().with_session_id(session_id);
                match parent_session {
                    Some(parent_session) => command.send(&parent_session).await,
                    None => command.send(&cdp).await,
                }
                .map_err(super::OwnershipCleanupError::from)
            },
        )
    }

    fn track_auto_attached_target_by_target_id(
        &self,
        target_id: String,
    ) -> super::PendingOwnershipGuard {
        let cdp = self.runtime.cdp().clone();
        self.auto_attached_target_cleanups.register(
            format!("auto-attached-target-detach:{target_id}"),
            move || async move {
                DetachFromTarget::new()
                    .with_target_id(target_id)
                    .send(&cdp)
                    .await
                    .map_err(super::OwnershipCleanupError::from)
            },
        )
    }

    fn acknowledge_oopif_initialization(&self, session_id: &str, token: AttachToken) {
        let mut state = self.state.write();
        state.abandon_oopif_attach(session_id, token);
        state.acknowledge_detached(session_id, token);
        drop(state);
        self.context_changed.notify_waiters();
    }

    pub(crate) async fn initialize(page: Page) -> Result<Arc<Self>, BrowserError> {
        let runtime = page.runtime().clone();
        let identity = FrameStoreIdentity::new(page.generation());
        let main_session = page.cdp_session().clone();
        let route_context_options = page
            .owner_session()
            .ok()
            .map(|owner| owner.context_options().clone());
        let configure_every_route = route_context_options
            .as_ref()
            .is_some_and(super::route::has_every_route_configuration);
        let mut target_detached = runtime.cdp().observe(["Target.detachedFromTarget"]).await?;
        let (main_events, mut main_target_attached, tree) =
            Self::prepare_frame_session(&main_session).await?;

        let mut graph = FrameGraph::new(main_session.id());
        Self::merge_tree(
            &mut graph,
            &tree,
            main_session.id(),
            &BTreeSet::new(),
            false,
        );
        let mut sessions = HashMap::new();
        sessions.insert(main_session.id().to_owned(), main_session.clone());
        let (file_chooser_opened, _) = tokio::sync::broadcast::channel(16);
        let (main_document_applied, _) = tokio::sync::broadcast::channel(64);
        let store = Arc::new(Self {
            page: page.downgrade_inner(),
            runtime,
            identity,
            main_document_applied,
            route_context_options,
            pending_oopif_initializations: super::PendingOwnershipRegistry::new(),
            auto_attached_target_cleanups: super::PendingOwnershipRegistry::new(),
            auxiliary_targets: AuxiliaryTargetRegistry::new(),
            state: RwLock::new(FrameState {
                graph,
                sessions,
                child_sessions: HashMap::new(),
                next_attach_token: 0,
                execution_contexts: Vec::new(),
            }),
            cancel: CancellationToken::new(),
            runtime_events_lock: tokio::sync::Mutex::new(()),
            runtime_events_enabled: tokio::sync::OnceCell::new(),
            runtime_events_requested: AtomicBool::new(false),
            context_changed: tokio::sync::Notify::new(),
            network_lock: tokio::sync::Mutex::new(()),
            network_requested: AtomicBool::new(false),
            file_chooser_state: tokio::sync::Mutex::new(Default::default()),
            file_chooser_opened,
        });

        Self::spawn_frame_reducer(
            &store,
            main_events,
            main_session.id().to_owned(),
            store.cancel.child_token(),
        );
        let weak_store = Arc::downgrade(&store);
        let cancel = store.cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    event = target_detached.next() => match event {
                        Some(event) => {
                            let Some(store) = weak_store.upgrade() else { break; };
                            match serde_json::from_value::<DetachedFromTarget>((*event.params).clone()) {
                                Ok(event) => {
                                    if let Some(auxiliary) =
                                        store.auxiliary_targets.take(&event.session_id)
                                    {
                                        if auxiliary.network_route {
                                            if let Some(page) = store.page() {
                                                if let Some(manager) = page.network_manager() {
                                                    manager.remove_route(&auxiliary.session_id).await;
                                                }
                                            }
                                        }
                                        auxiliary.disarm();
                                        continue;
                                    }
                                    let detached = store
                                        .state
                                        .write()
                                        .detach_child_session(&event.session_id);
                                    store.context_changed.notify_waiters();
                                    if let Some(page) = store.page() {
                                        for session_id in &detached.session_ids {
                                            page.route_configurations().schedule(session_id);
                                        }
                                        if let Some(manager) = page.network_manager() {
                                            manager.remove_route(&event.session_id).await;
                                        }
                                        for change in detached.route_changes {
                                            page.publish_frame_event(
                                                PageEvent::FrameRouteChanged {
                                                    frame_id: FrameId::new(change.frame_id.clone()),
                                                    previous_session_id: change.previous_session_id,
                                                    session_id: change.session_id.clone(),
                                                    target_id: None,
                                                },
                                                FrameId::new(change.frame_id),
                                                Some(change.session_id),
                                            );
                                        }
                                    }
                                }
                                Err(error) => tracing::warn!(%error, "invalid detachedFromTarget payload"),
                            }
                        }
                        None => break,
                    },
                }
            }
        });
        SetAutoAttach::new(true, configure_every_route)
            .with_flatten(true)
            .send(&main_session)
            .await?;
        Self::drain_initial_attached_targets(&store, &mut main_target_attached).await?;
        Self::spawn_target_attach_reducer(&store, main_target_attached, store.cancel.child_token());
        Ok(store)
    }

    async fn prepare_frame_session(
        session: &cdpkit::Session,
    ) -> Result<(FrameEventStreams, cdpkit::RawEventStream, FrameTree), BrowserError> {
        let attached = FrameAttached::subscribe(session).await?;
        let detached = FrameDetached::subscribe(session).await?;
        let navigated = FrameNavigated::subscribe(session).await?;
        let same_document = NavigatedWithinDocument::subscribe(session).await?;
        let console = ConsoleApiCalled::subscribe(session).await?;
        let exception = ExceptionThrown::subscribe(session).await?;
        let dialog_opened = JavascriptDialogOpening::subscribe(session).await?;
        let dialog_closed = JavascriptDialogClosed::subscribe(session).await?;
        let file_chooser_opened = FileChooserOpened::subscribe(session).await?;
        let execution_contexts = session.observe(["Runtime.executionContext*"]).await?;
        let target_attached = session.observe(["Target.attachedToTarget"]).await?;
        Enable::new().send(session).await?;
        let tree = GetFrameTree::new().send(session).await?.frame_tree;
        Ok((
            FrameEventStreams {
                attached,
                detached,
                navigated,
                same_document,
                console,
                exception,
                dialog_opened,
                dialog_closed,
                file_chooser_opened,
                execution_contexts,
            },
            target_attached,
            tree,
        ))
    }

    pub(crate) async fn enable_runtime_events(&self) -> Result<(), BrowserError> {
        let _runtime_events_guard = self.runtime_events_lock.lock().await;
        self.runtime_events_requested.store(true, Ordering::Release);
        self.runtime_events_enabled
            .get_or_try_init(|| async {
                let sessions = {
                    let state = self.state.read();
                    state.sessions.values().cloned().collect::<Vec<_>>()
                };
                if sessions.is_empty() {
                    return Err(BrowserError::operation(
                        "enable page runtime events",
                        super::OperationPhase::Preparation,
                    )
                    .with_message("page CDP sessions are unavailable"));
                }
                for session in sessions {
                    RuntimeEnable::new()
                        .send(&session)
                        .await
                        .map_err(BrowserError::from)?;
                    SetAsyncCallStackDepth::new(32)
                        .send(&session)
                        .await
                        .map_err(BrowserError::from)?;
                }
                Ok(())
            })
            .await
            .map(|_| ())
    }

    pub(super) async fn main_world_context(
        &self,
        route: &LocatorFrameRoute,
    ) -> Result<MainWorldContext, BrowserError> {
        self.enable_runtime_events().await?;
        loop {
            let context_changed = self.context_changed.notified();
            tokio::pin!(context_changed);
            context_changed.as_mut().enable();
            self.validate_locator_route(route)?;
            if let Some(context) = self
                .state
                .read()
                .default_context(&route.session_id, route.frame_id.as_str())
            {
                return Ok(context);
            }
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    return Err(BrowserError::operation(
                        "resolve JavaScript execution context",
                        super::OperationPhase::Preparation,
                    ).with_message("page closed before its main-world execution context became available"));
                }
                _ = &mut context_changed => {}
            }
        }
    }

    pub(super) fn validate_main_world_context(
        &self,
        route: &LocatorFrameRoute,
        context: &MainWorldContext,
    ) -> Result<(), BrowserError> {
        self.validate_locator_route(route)?;
        let current = self
            .state
            .read()
            .default_context(&route.session_id, route.frame_id.as_str());
        if current
            .as_ref()
            .is_some_and(|current| current.unique_id == context.unique_id)
        {
            Ok(())
        } else {
            Err(BrowserError::operation(
                "validate JavaScript execution context",
                super::OperationPhase::Observation,
            )
            .with_message(format!(
                "frame {} main-world execution context was destroyed or replaced",
                route.frame_id
            )))
        }
    }

    pub(crate) async fn enable_network(
        &self,
        page: &Page,
    ) -> Result<Arc<super::network::NetworkManager>, BrowserError> {
        let _guard = self.network_lock.lock().await;
        let route_sessions = {
            let state = self.state.read();
            state
                .sessions
                .values()
                .map(|session| {
                    let direct_parent_session_id = state
                        .child_sessions
                        .get(session.id())
                        .map(|ownership| ownership.parent_session_id.clone());
                    (session.clone(), direct_parent_session_id)
                })
                .collect::<Vec<_>>()
        };
        let sessions: Vec<super::network::NetworkRouteRegistration> = route_sessions
            .into_iter()
            .map(|(session, direct_parent_session_id)| {
                let scopes = self.freeze_route_scopes(session.id());
                (session, scopes, direct_parent_session_id, None)
            })
            .collect();
        self.network_requested.store(true, Ordering::Release);
        for route in &sessions {
            SetAutoAttach::new(true, true)
                .with_flatten(true)
                .send(&route.0)
                .await
                .map_err(BrowserError::from)?;
        }
        page.initialize_network_manager(sessions).await
    }

    pub(crate) fn subscribe_file_choosers(
        &self,
    ) -> tokio::sync::broadcast::Receiver<super::file_chooser::FileChooserOpenedFact> {
        self.file_chooser_opened.subscribe()
    }

    pub(crate) async fn begin_file_chooser_interception(&self) -> Result<u64, BrowserError> {
        let mut state = self.file_chooser_state.lock().await;
        let generation = state.begin().ok_or_else(|| {
            BrowserError::operation("expect file chooser", super::OperationPhase::Preparation)
                .with_message("another file chooser expectation is active")
        })?;
        let sessions = self
            .state
            .read()
            .sessions
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut enabled: Vec<cdpkit::Session> = Vec::new();
        for session in sessions {
            if let Err(error) = SetInterceptFileChooserDialog::new(true)
                .send(&session)
                .await
            {
                let mut rollback_complete = true;
                for enabled_session in enabled {
                    let disabled = SetInterceptFileChooserDialog::new(false)
                        .send(&enabled_session)
                        .await
                        .is_ok();
                    if disabled {
                        state.remove_enabled(enabled_session.id());
                    }
                    rollback_complete &= disabled;
                }
                if rollback_complete {
                    state.finish(generation);
                }
                return Err(BrowserError::cdp_operation(
                    "enable file chooser interception",
                    super::OperationPhase::Preparation,
                    error,
                ));
            }
            state.track_enabled(session.clone());
            enabled.push(session);
        }
        Ok(generation)
    }

    pub(crate) async fn end_file_chooser_interception(
        &self,
        generation: u64,
    ) -> Result<(), BrowserError> {
        let mut state = self.file_chooser_state.lock().await;
        if state.active_generation() != Some(generation) {
            return Ok(());
        }
        let sessions = state.enabled_sessions();
        let mut first_error = None;
        for session in sessions {
            if let Err(error) = SetInterceptFileChooserDialog::new(false)
                .send(&session)
                .await
            {
                first_error.get_or_insert(error);
            } else {
                state.remove_enabled(session.id());
            }
        }
        match first_error {
            Some(error) => Err(BrowserError::cdp_operation(
                "disable file chooser interception",
                super::OperationPhase::Cleanup,
                error,
            )
            .with_action_completion(super::ActionCompletion::Completed)),
            None => {
                state.finish(generation);
                Ok(())
            }
        }
    }

    pub(crate) async fn close_file_chooser_interception(&self) -> Result<(), BrowserError> {
        let generation = self.file_chooser_state.lock().await.active_generation();
        match generation {
            Some(generation) => self.end_file_chooser_interception(generation).await,
            None => Ok(()),
        }
    }

    async fn enable_file_chooser_for_new_route(
        &self,
        session: &cdpkit::Session,
    ) -> Result<(), BrowserError> {
        let state = self.file_chooser_state.lock().await;
        if state.active_generation().is_none() {
            return Ok(());
        }
        SetInterceptFileChooserDialog::new(true)
            .send(session)
            .await
            .map_err(|error| {
                BrowserError::cdp_operation(
                    "enable file chooser interception for OOPIF",
                    super::OperationPhase::Preparation,
                    error,
                )
            })?;
        drop(state);
        let mut state = self.file_chooser_state.lock().await;
        if state.active_generation().is_some() {
            state.track_enabled(session.clone());
        } else {
            SetInterceptFileChooserDialog::new(false)
                .send(session)
                .await
                .map_err(|error| {
                    BrowserError::cdp_operation(
                        "disable stale file chooser interception for OOPIF",
                        super::OperationPhase::Cleanup,
                        error,
                    )
                })?;
        }
        Ok(())
    }

    fn take_initial_attached_targets(
        target_attached: &mut cdpkit::RawEventStream,
    ) -> Vec<AttachedTargetResult> {
        let mut initial = Vec::new();
        loop {
            let Some(event) = target_attached.next().now_or_never().flatten() else {
                return initial;
            };
            let parent_session_id = event.session_id.as_deref().map(str::to_owned);
            initial.push(AttachedTargetResult::from_raw(
                parent_session_id,
                (*event.params).clone(),
            ));
        }
    }

    async fn drain_initial_attached_targets(
        store: &Arc<Self>,
        target_attached: &mut cdpkit::RawEventStream,
    ) -> Result<(), BrowserError> {
        for attached in Self::take_initial_attached_targets(target_attached) {
            store.handle_attached_target_result(attached).await?;
        }
        Ok(())
    }

    fn spawn_target_attach_reducer(
        store: &Arc<Self>,
        mut target_attached: cdpkit::RawEventStream,
        cancel: CancellationToken,
    ) {
        let weak_store = Arc::downgrade(store);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    event = target_attached.next() => match event {
                        Some(event) => {
                            let Some(store) = weak_store.upgrade() else { break; };
                            let parent_session_id = event.session_id.as_deref().map(str::to_owned);
                            let attached = AttachedTargetResult::from_raw(
                                parent_session_id,
                                (*event.params).clone(),
                            );
                            let Some(page) = store.page() else {
                                let _ = store.salvage_unadmitted_attached_target(attached).await;
                                break;
                            };
                            if page.terminal_route_error().is_some() {
                                if let Err(error) = store.salvage_unadmitted_attached_target(attached).await {
                                    page.record_terminal_route_failure(error);
                                }
                                continue;
                            }
                            let Ok(_route_operation) = page.admit_route_initialization() else {
                                if let Err(error) = store.salvage_unadmitted_attached_target(attached).await {
                                    page.record_terminal_route_failure(error);
                                }
                                break;
                            };
                            if let Err(error) = store.handle_attached_target_result(attached).await {
                                page.record_terminal_route_failure(error);
                                store.context_changed.notify_waiters();
                            }
                        }
                        None => break,
                    },
                }
            }
        });
    }

    async fn handle_attached_target_result(
        self: &Arc<Self>,
        attached: AttachedTargetResult,
    ) -> Result<(), BrowserError> {
        match attached {
            AttachedTargetResult::Typed(attached)
                if attached.event.target_info.type_ == "iframe" =>
            {
                let frame_id = attached.event.target_info.target_id.clone();
                let session_id = attached.event.session_id.clone();
                Box::pin(
                    self.initialize_oopif(attached.parent_session_id.as_deref(), attached.event),
                )
                .await
                .map_err(|error| {
                    error.with_route_failure(super::RouteFailure::new(
                        frame_id.clone(),
                        frame_id,
                        session_id,
                    ))
                })
            }
            AttachedTargetResult::Typed(attached)
                if is_auxiliary_worker_target(&attached.event.target_info.type_) =>
            {
                self.initialize_auxiliary_target(attached).await
            }
            AttachedTargetResult::Typed(attached) => {
                let target_id = attached.event.target_info.target_id.clone();
                let session_id = attached.event.session_id.clone();
                let target_type = attached.event.target_info.type_.clone();
                let failures = self
                    .cleanup_auto_attached_target(
                        attached.parent_session_id.as_deref(),
                        &session_id,
                        attached.event.waiting_for_debugger,
                    )
                    .await;
                if failures.is_empty() {
                    Ok(())
                } else {
                    Err(Self::auto_attached_target_cleanup_error(
                        Some(target_id),
                        Some(session_id),
                        Some(target_type),
                        failures,
                    ))
                }
            }
            AttachedTargetResult::Malformed(attached) => {
                self.handle_malformed_attached_target(attached).await
            }
        }
    }

    async fn initialize_auxiliary_target(
        self: &Arc<Self>,
        attached: InitialOopifAttach,
    ) -> Result<(), BrowserError> {
        let event = attached.event;
        let session_id = event.session_id.clone();
        let target_id = event.target_info.target_id.clone();
        let target_type = event.target_info.type_.clone();
        let waiting_for_debugger = event.waiting_for_debugger;
        let Some(claim) = self.auxiliary_targets.begin(
            self.runtime.cdp(),
            attached.parent_session_id.as_deref(),
            session_id.clone(),
            target_id.clone(),
            target_type.clone(),
        ) else {
            return Self::resume_paused_target(
                &self.runtime.cdp().session(session_id),
                waiting_for_debugger,
            )
            .await;
        };
        let session = self.runtime.cdp().session(session_id.clone());
        let needs_configuration = self
            .route_context_options
            .as_ref()
            .is_some_and(super::route::has_auxiliary_network_configuration);
        let retain_network_route =
            needs_configuration || self.network_requested.load(Ordering::Acquire);

        if !retain_network_route {
            let mut error = Self::resume_paused_target(&session, waiting_for_debugger)
                .await
                .err()
                .map(|resume| {
                    BrowserError::operation(
                        "clean up auxiliary target",
                        super::OperationPhase::Cleanup,
                    )
                    .with_message("failed to resume an unneeded auxiliary target")
                    .with_cleanup_failure(super::CleanupFailure::new(
                        format!("auxiliary-target-resume:{session_id}"),
                        resume.to_string(),
                    ))
                });
            if let Some(attachment) = self.auxiliary_targets.take(&session_id) {
                if let Err(cleanup) = attachment.cleanup().await {
                    let failure = super::CleanupFailure::new(
                        format!("auxiliary-target-detach:{session_id}"),
                        cleanup.to_string(),
                    );
                    error = Some(match error {
                        Some(error) => error.with_cleanup_failure(failure),
                        None => BrowserError::operation(
                            "clean up auxiliary target",
                            super::OperationPhase::Cleanup,
                        )
                        .with_message("failed to detach an unneeded auxiliary target")
                        .with_cleanup_failure(failure),
                    });
                }
            }
            return match error {
                Some(error) => Err(error.with_target_failure(super::TargetFailure::new(
                    Some(target_id),
                    Some(session_id),
                    Some(target_type),
                ))),
                None => Ok(()),
            };
        }

        let manager = {
            let _network_guard = self.network_lock.lock().await;
            let Some(page) = self.page() else {
                let error = BrowserError::operation(
                    "configure auxiliary target",
                    super::OperationPhase::Preparation,
                )
                .with_message("page is unavailable");
                return Err(self
                    .fail_auxiliary_target(claim, &event, false, error)
                    .await);
            };
            let manager = match page.network_manager().cloned() {
                Some(manager) => manager,
                None => {
                    let route_sessions = {
                        let state = self.state.read();
                        state
                            .sessions
                            .values()
                            .map(|route| {
                                let direct_parent_session_id = state
                                    .child_sessions
                                    .get(route.id())
                                    .map(|ownership| ownership.parent_session_id.clone());
                                (route.clone(), direct_parent_session_id)
                            })
                            .collect::<Vec<_>>()
                    };
                    let sessions = route_sessions
                        .into_iter()
                        .map(|(route, direct_parent_session_id)| {
                            let scopes = self.freeze_route_scopes(route.id());
                            (route, scopes, direct_parent_session_id, None)
                        })
                        .collect();
                    match page.initialize_network_manager(sessions).await {
                        Ok(manager) => manager,
                        Err(error) => {
                            return Err(self
                                .fail_auxiliary_target(claim, &event, false, error)
                                .await);
                        }
                    }
                }
            };
            if let Err(error) = manager
                .add_route(
                    session.clone(),
                    Vec::new(),
                    attached.parent_session_id.clone(),
                    Some(event.target_info.url.clone()),
                )
                .await
            {
                return Err(self
                    .fail_auxiliary_target(claim, &event, false, error)
                    .await);
            }
            if !self.auxiliary_targets.mark_network_route(&claim) {
                manager.remove_route(&session_id).await;
                return Ok(());
            }
            manager
        };

        if needs_configuration {
            if let Err(error) = super::route::configure_auxiliary_network_route(
                self.route_context_options
                    .as_ref()
                    .expect("configuration exists"),
                &session,
            )
            .await
            {
                return Err(self.fail_auxiliary_target(claim, &event, true, error).await);
            }
        }

        let mut nested_attached = if target_type == "worker" {
            match session.observe(["Target.attachedToTarget"]).await {
                Ok(stream) => Some(stream),
                Err(error) => {
                    return Err(self
                        .fail_auxiliary_target(claim, &event, true, BrowserError::from(error))
                        .await);
                }
            }
        } else {
            None
        };
        if nested_attached.is_some() {
            if let Err(error) = SetAutoAttach::new(true, retain_network_route)
                .with_flatten(true)
                .send(&session)
                .await
            {
                return Err(self
                    .fail_auxiliary_target(claim, &event, true, BrowserError::from(error))
                    .await);
            }
        }
        if let Err(resume) = Self::resume_paused_target(&session, waiting_for_debugger).await {
            let error =
                BrowserError::operation("resume auxiliary target", super::OperationPhase::Cleanup)
                    .with_message("failed to resume a configured auxiliary target")
                    .with_cleanup_failure(super::CleanupFailure::new(
                        format!("auxiliary-target-resume:{session_id}"),
                        resume.to_string(),
                    ));
            return Err(self
                .fail_auxiliary_target(claim, &event, false, error)
                .await);
        }
        if !self.auxiliary_targets.retain(claim) {
            manager.remove_route(&session_id).await;
            return Ok(());
        }
        if let Some(stream) = nested_attached.take() {
            Self::spawn_target_attach_reducer(self, stream, self.cancel.child_token());
        }
        Ok(())
    }

    async fn fail_auxiliary_target(
        &self,
        claim: AuxiliaryTargetClaim,
        event: &AttachedToTarget,
        salvage_resume: bool,
        mut error: BrowserError,
    ) -> BrowserError {
        if self.auxiliary_targets.has_network_route(&claim.session_id) {
            if let Some(page) = self.page() {
                if let Some(manager) = page.network_manager() {
                    manager.remove_route(&claim.session_id).await;
                }
            }
        }
        if salvage_resume {
            if let Err(resume) = Self::resume_paused_target(
                &self.runtime.cdp().session(claim.session_id.clone()),
                event.waiting_for_debugger,
            )
            .await
            {
                error = error.with_cleanup_failure(super::CleanupFailure::new(
                    format!("auxiliary-target-resume:{}", claim.session_id),
                    resume.to_string(),
                ));
            }
        }
        if let Some(attachment) = self.auxiliary_targets.take(&claim.session_id) {
            if let Err(cleanup) = attachment.cleanup().await {
                error = error.with_cleanup_failure(super::CleanupFailure::new(
                    format!("auxiliary-target-detach:{}", claim.session_id),
                    cleanup.to_string(),
                ));
            }
        }
        error.with_target_failure(super::TargetFailure::new(
            Some(event.target_info.target_id.clone()),
            Some(event.session_id.clone()),
            Some(event.target_info.type_.clone()),
        ))
    }

    async fn salvage_unadmitted_attached_target(
        &self,
        attached: AttachedTargetResult,
    ) -> Result<(), BrowserError> {
        match attached {
            AttachedTargetResult::Typed(attached) => {
                let target_id = attached.event.target_info.target_id.clone();
                let session_id = attached.event.session_id.clone();
                let target_type = attached.event.target_info.type_.clone();
                let failures = self
                    .cleanup_auto_attached_target(
                        attached.parent_session_id.as_deref(),
                        &session_id,
                        attached.event.waiting_for_debugger,
                    )
                    .await;
                if failures.is_empty() {
                    Ok(())
                } else {
                    Err(Self::auto_attached_target_cleanup_error(
                        Some(target_id),
                        Some(session_id),
                        Some(target_type),
                        failures,
                    ))
                }
            }
            AttachedTargetResult::Malformed(attached) => {
                self.handle_malformed_attached_target(attached).await
            }
        }
    }

    async fn handle_malformed_attached_target(
        &self,
        attached: MalformedAttachedTarget,
    ) -> Result<(), BrowserError> {
        let failures = if let Some(session_id) = attached.session_id.as_deref() {
            self.cleanup_auto_attached_target(
                attached.parent_session_id.as_deref(),
                session_id,
                attached.waiting_for_debugger.unwrap_or(true),
            )
            .await
        } else if let Some(target_id) = attached.target_id.as_deref() {
            let cleanup = self.track_auto_attached_target_by_target_id(target_id.to_owned());
            match cleanup.cleanup().await {
                Ok(()) => Vec::new(),
                Err(error) => vec![super::CleanupFailure::new(
                    format!("auto-attached-target-detach:{target_id}"),
                    error.to_string(),
                )],
            }
        } else {
            Vec::new()
        };
        let mut error = BrowserError::operation(
            "parse auto-attached target",
            super::OperationPhase::Observation,
        )
        .with_message(format!(
            "invalid Target.attachedToTarget payload: {}",
            attached.parse_error
        ))
        .with_target_failure(
            super::TargetFailure::new(
                attached.target_id,
                attached.session_id,
                attached.target_type,
            )
            .with_target_url(attached.target_url)
            .with_event_error(attached.parse_error)
            .with_field_errors(attached.field_errors),
        );
        for failure in failures {
            error = error.with_cleanup_failure(failure);
        }
        Err(error)
    }

    async fn cleanup_auto_attached_target(
        &self,
        parent_session_id: Option<&str>,
        session_id: &str,
        waiting_for_debugger: bool,
    ) -> Vec<super::CleanupFailure> {
        let cleanup = self.track_auto_attached_target(parent_session_id, session_id.to_owned());
        let mut failures = Vec::new();
        if waiting_for_debugger {
            let session = self.runtime.cdp().session(session_id.to_owned());
            if let Err(error) = RunIfWaitingForDebugger::new().send(&session).await {
                failures.push(super::CleanupFailure::new(
                    format!("auto-attached-target-resume:{session_id}"),
                    error.to_string(),
                ));
            }
        }
        if let Err(error) = cleanup.cleanup().await {
            failures.push(super::CleanupFailure::new(
                format!("auto-attached-target-detach:{session_id}"),
                error.to_string(),
            ));
        }
        failures
    }

    fn auto_attached_target_cleanup_error(
        target_id: Option<String>,
        session_id: Option<String>,
        target_type: Option<String>,
        failures: Vec<super::CleanupFailure>,
    ) -> BrowserError {
        let mut error = BrowserError::operation(
            "clean up auto-attached target",
            super::OperationPhase::Cleanup,
        )
        .with_message("failed to resume or detach an auto-attached non-frame target")
        .with_target_failure(super::TargetFailure::new(
            target_id,
            session_id,
            target_type,
        ));
        for failure in failures {
            error = error.with_cleanup_failure(failure);
        }
        error
    }

    fn spawn_frame_reducer(
        store: &Arc<Self>,
        streams: FrameEventStreams,
        routed_session_id: String,
        cancel: CancellationToken,
    ) {
        let weak_store = Arc::downgrade(store);
        let FrameEventStreams {
            mut attached,
            mut detached,
            mut navigated,
            mut same_document,
            mut console,
            mut exception,
            mut dialog_opened,
            mut dialog_closed,
            mut file_chooser_opened,
            mut execution_contexts,
        } = streams;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    event = attached.next() => match event {
                        Some(Ok(event)) => {
                            let Some(store) = weak_store.upgrade() else { break; };
                            let route = {
                                let mut state = store.state.write();
                                state.graph.attach(&event.frame_id, Some(&event.parent_frame_id));
                                state.graph.route_session(&event.frame_id).map(str::to_owned)
                            };
                            store.context_changed.notify_waiters();
                            let Some(page) = store.page() else { break; };
                            page.publish_frame_event(
                                PageEvent::FrameAttached {
                                    frame_id: FrameId::new(event.frame_id.clone()),
                                    parent_frame_id: FrameId::new(event.parent_frame_id),
                                },
                                FrameId::new(event.frame_id),
                                route,
                            );
                        }
                        Some(Err(error)) => { tracing::warn!(%error, "frameAttached stream failed"); close_page_event_source(&weak_store); break; },
                        None => { close_page_event_source(&weak_store); break; },
                    },
                    event = detached.next() => match event {
                        Some(Ok(event)) => {
                            let Some(store) = weak_store.upgrade() else { break; };
                            let route = store.state.read().graph.route_session(&event.frame_id).map(str::to_owned);
                            if should_remove_detached_frame(&event.reason) {
                                store.state.write().graph.detach(&event.frame_id);
                            }
                            store.context_changed.notify_waiters();
                            let Some(page) = store.page() else { break; };
                            page.publish_frame_event(
                                PageEvent::FrameDetached { frame_id: FrameId::new(event.frame_id.clone()) },
                                FrameId::new(event.frame_id),
                                route,
                            );
                        }
                        Some(Err(error)) => { tracing::warn!(%error, "frameDetached stream failed"); close_page_event_source(&weak_store); break; },
                        None => { close_page_event_source(&weak_store); break; },
                    },
                    event = navigated.next() => match event {
                        Some(Ok(event)) => {
                            let Some(store) = weak_store.upgrade() else { break; };
                            let frame_id = event.frame.id.clone();
                            let url = event.frame.url.clone();
                            let loader_id = event.frame.loader_id.clone();
                            #[cfg(test)]
                            wait_for_main_document_reducer_gate(&loader_id).await;
                            let (is_page_main, changed) = {
                                let mut state = store.state.write();
                                let is_page_main = state.graph.main_frame_id() == Some(event.frame.id.as_str());
                                let changed = state.graph.navigate(
                                    &event.frame.id,
                                    event.frame.parent_id.as_deref(),
                                    &event.frame.loader_id,
                                );
                                (is_page_main, changed)
                            };
                            store.context_changed.notify_waiters();
                            if is_page_main && changed {
                                let Some(page) = store.page() else { break; };
                                page.lifecycle().commit_new_document();
                                let _ = store.main_document_applied.send(AppliedMainDocument {
                                    frame_id: frame_id.clone(),
                                    loader_id: loader_id.clone(),
                                });
                            }
                            let route = store.state.read().graph.route_session(&frame_id).map(str::to_owned);
                            let Some(page) = store.page() else { break; };
                            page.publish_frame_event(
                                PageEvent::FrameNavigated {
                                    frame_id: FrameId::new(frame_id.clone()), url,
                                    loader_id: Some(loader_id), same_document: false,
                                },
                                FrameId::new(frame_id), route,
                            );
                        }
                        Some(Err(error)) => { tracing::warn!(%error, "frameNavigated stream failed"); close_page_event_source(&weak_store); break; },
                        None => { close_page_event_source(&weak_store); break; },
                    },
                    event = same_document.next() => match event {
                        Some(Ok(event)) => {
                            let Some(store) = weak_store.upgrade() else { break; };
                            let route = store.state.read().graph.route_session(&event.frame_id).map(str::to_owned);
                            let Some(page) = store.page() else { break; };
                            page.publish_frame_event(
                                PageEvent::FrameNavigated { frame_id: FrameId::new(event.frame_id.clone()), url: event.url, loader_id: None, same_document: true },
                                FrameId::new(event.frame_id), route,
                            );
                        }
                        Some(Err(error)) => { tracing::warn!(%error, "navigatedWithinDocument stream failed"); close_page_event_source(&weak_store); break; },
                        None => { close_page_event_source(&weak_store); break; },
                    },
                    event = console.next() => match event {
                        Some(Ok(event)) => {
                            let Some(store) = weak_store.upgrade() else { break; };
                            let Some(page) = store.page() else { break; };
                            page.publish_routed_event(
                                PageEvent::Console(console_message(event)),
                                routed_session_id.clone(),
                            );
                        }
                        Some(Err(error)) => { tracing::warn!(%error, "consoleAPICalled stream failed"); close_page_event_source(&weak_store); break; },
                        None => { close_page_event_source(&weak_store); break; },
                    },
                    event = exception.next() => match event {
                        Some(Ok(event)) => {
                            let Some(store) = weak_store.upgrade() else { break; };
                            let Some(page) = store.page() else { break; };
                            page.publish_routed_event(
                                PageEvent::JavaScriptError(javascript_error(event)),
                                routed_session_id.clone(),
                            );
                        }
                        Some(Err(error)) => { tracing::warn!(%error, "exceptionThrown stream failed"); close_page_event_source(&weak_store); break; },
                        None => { close_page_event_source(&weak_store); break; },
                    },
                    event = dialog_opened.next() => match event {
                        Some(Ok(event)) => {
                            let Some(store) = weak_store.upgrade() else { break; };
                            let Some(page) = store.page() else { break; };
                            let Some(routed_session) = store.state.read().sessions.get(&routed_session_id).cloned() else { break; };
                            let frame_id = FrameId::new(event.frame_id.clone());
                            let dialog_type = super::DialogType::from_protocol(event.type_.as_ref());
                            page.dialogs().open(super::dialog::DialogOpenedFact {
                                epoch: 0,
                                routed_session,
                                frame_id: event.frame_id.clone(),
                                message: event.message.clone(),
                                dialog_type: dialog_type.clone(),
                                default_prompt: event.default_prompt.clone(),
                            });
                            page.publish_frame_event(PageEvent::DialogOpened { frame_id: frame_id.clone(), url: event.url, message: event.message, dialog_type, default_prompt: event.default_prompt, has_browser_handler: event.has_browser_handler }, frame_id, Some(routed_session_id.clone()));
                        }
                        Some(Err(error)) => { tracing::warn!(%error, "javascriptDialogOpening stream failed"); close_page_event_source(&weak_store); break; },
                        None => { close_page_event_source(&weak_store); break; },
                    },
                    event = dialog_closed.next() => match event {
                        Some(Ok(event)) => {
                            let Some(store) = weak_store.upgrade() else { break; };
                            let Some(page) = store.page() else { break; };
                            page.dialogs().close_route(&routed_session_id);
                            let frame_id = FrameId::new(event.frame_id.clone());
                            page.publish_frame_event(PageEvent::DialogClosed { frame_id: frame_id.clone(), accepted: event.result, user_input: event.user_input }, frame_id, Some(routed_session_id.clone()));
                        }
                        Some(Err(error)) => { tracing::warn!(%error, "javascriptDialogClosed stream failed"); close_page_event_source(&weak_store); break; },
                        None => { close_page_event_source(&weak_store); break; },
                    },
                    event = file_chooser_opened.next() => match event {
                        Some(Ok(event)) => {
                            let Some(store) = weak_store.upgrade() else { break; };
                            let Some(routed_session) = store.state.read().sessions.get(&routed_session_id).cloned() else { break; };
                            let _ = store.file_chooser_opened.send(super::file_chooser::FileChooserOpenedFact {
                                routed_session,
                                frame_id: event.frame_id,
                                backend_node_id: event.backend_node_id,
                                multiple: event.mode.as_ref() == "selectMultiple",
                            });
                        }
                        Some(Err(error)) => { tracing::warn!(%error, "fileChooserOpened stream failed"); close_page_event_source(&weak_store); break; },
                        None => { close_page_event_source(&weak_store); break; },
                    },
                    event = execution_contexts.next() => match event {
                        Some(event) => {
                            let Some(store) = weak_store.upgrade() else { break; };
                            let changed = {
                                let mut state = store.state.write();
                                apply_runtime_context_event(
                                    &mut state,
                                    &routed_session_id,
                                    event.method.as_ref(),
                                    (*event.params).clone(),
                                )
                            };
                            match changed {
                                Ok(true) => store.context_changed.notify_waiters(),
                                Ok(false) => {}
                                Err(error) => {
                                    tracing::warn!(
                                        %error,
                                        method = %event.method,
                                        "execution context event stream failed"
                                    );
                                    close_page_event_source(&weak_store);
                                    break;
                                }
                            }
                        }
                        None => { close_page_event_source(&weak_store); break; },
                    },
                }
            }
        });
    }

    async fn initialize_oopif(
        self: &Arc<Self>,
        parent_session_id: Option<&str>,
        event: AttachedToTarget,
    ) -> Result<(), BrowserError> {
        let frame_id = event.target_info.target_id.clone();
        let parent_frame_id = event.target_info.parent_frame_id.as_deref();
        let session = self.runtime.cdp().session(event.session_id.clone());
        let waiting_for_debugger = event.waiting_for_debugger;
        let start = self.state.write().begin_oopif_attach(
            &event.session_id,
            &frame_id,
            parent_frame_id,
            parent_session_id,
            self.cancel.child_token(),
        );
        let claim = match start {
            OopifAttachStart::Initialize(claim) => claim,
            OopifAttachStart::Idempotent { active } => {
                if active {
                    Self::resume_paused_target(&session, waiting_for_debugger).await?;
                }
                return Ok(());
            }
            OopifAttachStart::Conflict => {
                tracing::warn!(
                    session_id = %event.session_id,
                    %frame_id,
                    ?parent_session_id,
                    "ignored conflicting OOPIF session ownership event"
                );
                Self::resume_paused_target(&session, waiting_for_debugger).await?;
                return Ok(());
            }
            OopifAttachStart::ForeignParent => {
                Self::resume_paused_target(&session, waiting_for_debugger).await?;
                return Ok(());
            }
            OopifAttachStart::RouteUnavailable => {
                let route_error =
                    BrowserError::operation("attach OOPIF", super::OperationPhase::Preparation)
                        .with_message("OOPIF frame already has an active route");
                let error = match Self::resume_paused_target(&session, waiting_for_debugger).await {
                    Ok(()) => route_error,
                    Err(error) => error,
                };
                let error = self
                    .rollback_oopif_session(&event.session_id, error)
                    .await
                    .with_route_failure(super::RouteFailure::new(
                        frame_id.clone(),
                        event.target_info.target_id.clone(),
                        event.session_id.clone(),
                    ));
                return Err(error);
            }
        };

        let configure_every_route = self
            .route_context_options
            .as_ref()
            .is_some_and(super::route::has_every_route_configuration);
        let owner = self.page().and_then(|page| page.owner_session().ok());
        if configure_every_route && owner.is_none() {
            let error = BrowserError::operation(
                "configure OOPIF route",
                super::OperationPhase::Preparation,
            )
            .with_message("page owner session is unavailable");
            let _ = self
                .state
                .write()
                .abandon_oopif_attach(&event.session_id, claim.token);
            let error = self
                .rollback_oopif_session(&event.session_id, error)
                .await
                .with_route_failure(super::RouteFailure::new(
                    frame_id.clone(),
                    event.target_info.target_id.clone(),
                    event.session_id.clone(),
                ));
            self.acknowledge_oopif_initialization(&event.session_id, claim.token);
            return Err(error);
        }
        let context_options = self.route_context_options.clone().unwrap_or_default();
        let applied = super::route::applied_configuration();
        let lifecycle = match owner {
            Some(owner) => owner.track_oopif_initialization(event.session_id.clone()),
            None => self.track_unowned_oopif_initialization(event.session_id.clone()),
        };

        let prepared = tokio::select! {
            _ = claim.cancel.cancelled() => {
                return Err(self.fail_oopif_initialization(
                    &event.session_id,
                    claim.token,
                    lifecycle,
                    BrowserError::operation("prepare OOPIF route", super::OperationPhase::Preparation)
                        .with_message("page closed during OOPIF initialization"),
                ).await);
            },
            prepared = Self::prepare_frame_session(&session) => prepared,
        };
        let (streams, mut target_attached, _) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                return Err(self
                    .fail_oopif_initialization(&event.session_id, claim.token, lifecycle, error)
                    .await);
            }
        };

        if configure_every_route {
            let page = self.page().ok_or_else(|| {
                BrowserError::operation("configure OOPIF route", super::OperationPhase::Preparation)
                    .with_message("page is unavailable")
            })?;
            match super::route::configure_oopif_route(&page, &context_options, &session, &applied)
                .await
            {
                Ok(configuration) => configuration.retain(),
                Err(error) => {
                    return Err(self
                        .fail_oopif_initialization(&event.session_id, claim.token, lifecycle, error)
                        .await);
                }
            }
        }
        if claim.cancel.is_cancelled() {
            return Err(self
                .fail_oopif_initialization(
                    &event.session_id,
                    claim.token,
                    lifecycle,
                    BrowserError::operation(
                        "configure OOPIF route",
                        super::OperationPhase::Preparation,
                    )
                    .with_message("page closed during OOPIF configuration"),
                )
                .await);
        }

        // Serialize requested manager replay through provisional route commit.
        // Existing managers must see this paused route before it can execute.
        let _runtime_events_guard = self.runtime_events_lock.lock().await;
        if self.runtime_events_requested.load(Ordering::Acquire) {
            let runtime_result = async {
                RuntimeEnable::new().send(&session).await?;
                SetAsyncCallStackDepth::new(32).send(&session).await
            }
            .await;
            if let Err(error) = runtime_result {
                return Err(self
                    .fail_oopif_initialization(
                        &event.session_id,
                        claim.token,
                        lifecycle,
                        BrowserError::from(error),
                    )
                    .await);
            }
        }
        if let Err(error) = self.enable_file_chooser_for_new_route(&session).await {
            return Err(self
                .fail_oopif_initialization(&event.session_id, claim.token, lifecycle, error)
                .await);
        }
        {
            let _network_guard = self.network_lock.lock().await;
            if let Some(page) = self.page() {
                if let Some(manager) = page.network_manager() {
                    let scopes = self
                        .freeze_frame_lineage(&FrameId::new(frame_id.clone()))
                        .unwrap_or_default();
                    if let Err(error) = manager
                        .add_route(
                            session.clone(),
                            scopes,
                            parent_session_id.map(str::to_owned),
                            None,
                        )
                        .await
                    {
                        return Err(self
                            .fail_oopif_initialization(
                                &event.session_id,
                                claim.token,
                                lifecycle,
                                error,
                            )
                            .await);
                    }
                }
            }
        }

        let wait_for_debugger =
            configure_every_route || self.network_requested.load(Ordering::Acquire);
        if let Err(error) = SetAutoAttach::new(true, wait_for_debugger)
            .with_flatten(true)
            .send(&session)
            .await
        {
            return Err(self
                .fail_oopif_initialization(
                    &event.session_id,
                    claim.token,
                    lifecycle,
                    BrowserError::from(error),
                )
                .await);
        }
        if let Err(error) = Self::resume_paused_target(&session, waiting_for_debugger).await {
            if let Some(page) = self.page() {
                if let Some(manager) = page.network_manager() {
                    manager.remove_route(&event.session_id).await;
                }
            }
            return Err(self
                .fail_oopif_initialization(&event.session_id, claim.token, lifecycle, error)
                .await);
        }

        // The post-resume FrameTree command is the protocol fence: all route
        // configuration and replay commands are acknowledged before commit.
        // It is required even when no per-route configuration was requested,
        // because its response also orders nested auto-attach events before
        // publication of this route.
        let tree = match GetFrameTree::new().send(&session).await {
            Ok(response) => response.frame_tree,
            Err(error) => {
                return Err(self
                    .fail_oopif_initialization(
                        &event.session_id,
                        claim.token,
                        lifecycle,
                        BrowserError::cdp_operation(
                            "fence OOPIF route configuration",
                            super::OperationPhase::Confirmation,
                            error,
                        ),
                    )
                    .await);
            }
        };
        let initial_targets = Self::take_initial_attached_targets(&mut target_attached);
        let provisional_roots = initial_targets
            .iter()
            .filter(|attached| attached.is_iframe())
            .filter_map(AttachedTargetResult::target_id)
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        if claim.cancel.is_cancelled()
            || !self
                .state
                .read()
                .is_initializing_oopif_attach(&event.session_id, claim.token)
        {
            return Err(self
                .fail_oopif_initialization(
                    &event.session_id,
                    claim.token,
                    lifecycle,
                    BrowserError::operation(
                        "commit OOPIF route",
                        super::OperationPhase::Confirmation,
                    )
                    .with_message("OOPIF ownership changed before route commit"),
                )
                .await);
        }

        let reducer_cancel = claim.cancel.clone();
        let previous_route = {
            let mut state = self.state.write();
            let previous_route = state.graph.route_session(&frame_id).map(str::to_owned);
            if state.graph.route_oopif(
                &frame_id,
                parent_frame_id,
                parent_session_id,
                &event.session_id,
                &event.target_info.target_id,
            ) {
                Self::merge_tree(
                    &mut state.graph,
                    &tree,
                    &event.session_id,
                    &provisional_roots,
                    false,
                );
                let _ = state.graph.route_to_session(
                    &frame_id,
                    &event.session_id,
                    Some(&event.target_info.target_id),
                );
                state
                    .sessions
                    .insert(event.session_id.clone(), session.clone());
                debug_assert!(state.activate_oopif_attach(&event.session_id, claim.token));
                Some(previous_route)
            } else {
                state.abandon_oopif_attach(&event.session_id, claim.token);
                None
            }
        };
        let Some(previous_route) = previous_route else {
            return Err(self
                .fail_oopif_initialization(
                    &event.session_id,
                    claim.token,
                    lifecycle,
                    BrowserError::operation(
                        "commit OOPIF route",
                        super::OperationPhase::Confirmation,
                    )
                    .with_message("OOPIF parent route changed during initialization"),
                )
                .await);
        };
        self.context_changed.notify_waiters();
        drop(_runtime_events_guard);

        for initial in initial_targets {
            if let Err(error) = Box::pin(self.handle_attached_target_result(initial)).await {
                self.state.write().detach_child_session(&event.session_id);
                self.context_changed.notify_waiters();
                return Err(self
                    .fail_oopif_initialization(&event.session_id, claim.token, lifecycle, error)
                    .await);
            }
        }
        lifecycle.disarm();

        if let Some(page) = self.page() {
            page.publish_frame_event(
                PageEvent::FrameRouteChanged {
                    frame_id: FrameId::new(frame_id.clone()),
                    previous_session_id: previous_route
                        .or_else(|| parent_session_id.map(str::to_owned))
                        .unwrap_or_else(|| self.state.read().graph.main_session_id.clone()),
                    session_id: event.session_id.clone(),
                    target_id: Some(event.target_info.target_id.clone()),
                },
                FrameId::new(frame_id),
                Some(event.session_id.clone()),
            );
        }
        Self::spawn_frame_reducer(
            self,
            streams,
            event.session_id.clone(),
            reducer_cancel.clone(),
        );
        Self::spawn_target_attach_reducer(self, target_attached, reducer_cancel);
        Ok(())
    }

    async fn fail_oopif_initialization(
        &self,
        session_id: &str,
        token: AttachToken,
        lifecycle: super::PendingOwnershipGuard,
        mut error: BrowserError,
    ) -> BrowserError {
        let frame_id = self
            .state
            .read()
            .child_sessions
            .get(session_id)
            .map(|ownership| ownership.root_frame_id.clone())
            .unwrap_or_else(|| session_id.to_owned());
        if let Some(page) = self.page() {
            if let Some(manager) = page.network_manager() {
                manager.remove_route(session_id).await;
            }
        }
        self.state.write().abandon_oopif_attach(session_id, token);
        if let Some(page) = self.page() {
            if let Some(Err(cleanup)) = page.route_configurations().cleanup(session_id).await {
                error = error.with_cleanup_failure(super::CleanupFailure::new(
                    format!("route:{session_id}"),
                    cleanup.to_string(),
                ));
            }
        }
        if let Err(cleanup) = lifecycle.cleanup().await {
            error = error.with_cleanup_failure(super::CleanupFailure::new(
                format!("oopif-route:{session_id}"),
                cleanup.to_string(),
            ));
        }
        self.acknowledge_oopif_initialization(session_id, token);
        error.with_route_failure(super::RouteFailure::new(
            frame_id.clone(),
            frame_id,
            session_id.to_owned(),
        ))
    }

    async fn resume_paused_target(
        session: &cdpkit::Session,
        waiting_for_debugger: bool,
    ) -> Result<(), BrowserError> {
        if waiting_for_debugger {
            RunIfWaitingForDebugger::new()
                .send(session)
                .await
                .map_err(BrowserError::from)?;
        }
        Ok(())
    }

    async fn rollback_oopif_session(
        &self,
        session_id: &str,
        mut error: BrowserError,
    ) -> BrowserError {
        if let Err(cleanup_error) = DetachFromTarget::new()
            .with_session_id(session_id.to_owned())
            .send(self.runtime.cdp())
            .await
        {
            error = error.with_cleanup_failure(super::CleanupFailure::new(
                format!("oopif-route:{session_id}"),
                cleanup_error.to_string(),
            ));
        }
        error
    }

    fn merge_tree(
        graph: &mut FrameGraph,
        tree: &FrameTree,
        route_session_id: &str,
        provisional_roots: &BTreeSet<String>,
        provisional_ancestor: bool,
    ) {
        let provisional = provisional_ancestor || provisional_roots.contains(&tree.frame.id);
        graph.navigate(
            &tree.frame.id,
            tree.frame.parent_id.as_deref(),
            &tree.frame.loader_id,
        );
        let _ = graph.route_to_session(&tree.frame.id, route_session_id, None);
        graph.set_route_active(&tree.frame.id, !provisional);
        if let Some(children) = &tree.child_frames {
            for child in children {
                Self::merge_tree(
                    graph,
                    child,
                    route_session_id,
                    provisional_roots,
                    provisional,
                );
            }
        }
    }

    pub(crate) fn handle(&self, frame_id: &str) -> Option<Frame> {
        let page = self.page()?;
        let state = self.state.read();
        if !state.graph.is_route_active(frame_id) {
            return None;
        }
        let snapshot = self.identity.snapshot(&state.graph, frame_id)?;
        Some(Frame {
            page,
            id: FrameId::new(frame_id),
            snapshot,
        })
    }

    pub(crate) fn main_frame_id(&self) -> Option<String> {
        self.state.read().graph.main_frame_id().map(str::to_owned)
    }

    pub(crate) fn frame_ids(&self) -> Vec<String> {
        self.state.read().graph.active_ids()
    }

    pub(crate) fn freeze_frame_lineage(
        &self,
        frame_id: &FrameId,
    ) -> Option<Vec<FrameScopeIdentity>> {
        let state = self.state.read();
        let mut lineage = Vec::new();
        let mut current = Some(frame_id.as_str());
        while let Some(frame_id) = current {
            if !state.graph.is_route_active(frame_id) {
                return None;
            }
            lineage.push(FrameScopeIdentity {
                frame_id: FrameId::new(frame_id),
                snapshot: self.identity.snapshot(&state.graph, frame_id)?,
            });
            current = state.graph.parent(frame_id);
        }
        Some(lineage)
    }

    pub(super) fn freeze_locator_lineage(
        &self,
        resolved_route: &LocatorFrameRoute,
    ) -> Result<Vec<LocatorFrameRoute>, BrowserError> {
        let page = self.page().ok_or_else(|| {
            BrowserError::operation(
                "freeze locator frame lineage",
                super::OperationPhase::Observation,
            )
            .with_message("page was dropped")
        })?;
        page.lifecycle()
            .validate_page(resolved_route.page_generation)
            .map_err(|reason| {
                BrowserError::operation(
                    "freeze locator frame lineage",
                    super::OperationPhase::Observation,
                )
                .with_message(format!(
                    "frame {} is stale: {reason:?}",
                    resolved_route.frame_id
                ))
            })?;

        let state = self.state.read();
        let main_frame_id = state.graph.main_frame_id().ok_or_else(|| {
            BrowserError::operation(
                "freeze locator frame lineage",
                super::OperationPhase::Observation,
            )
            .with_message("page has no main frame")
        })?;
        let mut routes = Vec::new();
        let mut visited = BTreeSet::new();
        let mut current = resolved_route.frame_id.as_str();
        loop {
            if !visited.insert(current.to_owned()) {
                return Err(BrowserError::operation(
                    "freeze locator frame lineage",
                    super::OperationPhase::Observation,
                )
                .with_message("frame lineage contains a cycle"));
            }
            let record = state.graph.frames.get(current).ok_or_else(|| {
                BrowserError::operation(
                    "freeze locator frame lineage",
                    super::OperationPhase::Observation,
                )
                .with_message(format!("frame {current} is detached"))
            })?;
            if !record.route_active {
                return Err(BrowserError::operation(
                    "freeze locator frame lineage",
                    super::OperationPhase::Observation,
                )
                .with_message(format!("frame {current} route is not active")));
            }
            let loader_id = record.loader_id.clone().ok_or_else(|| {
                BrowserError::operation(
                    "freeze locator frame lineage",
                    super::OperationPhase::Observation,
                )
                .with_message(format!("frame {current} has no document identity"))
            })?;
            let session = state
                .sessions
                .get(&record.route_session_id)
                .cloned()
                .ok_or_else(|| {
                    BrowserError::operation(
                        "freeze locator frame lineage",
                        super::OperationPhase::Observation,
                    )
                    .with_message(format!("frame {current} route is unavailable"))
                })?;
            routes.push(LocatorFrameRoute {
                page_generation: resolved_route.page_generation,
                document_epoch: record.document_epoch,
                frame_id: FrameId::new(current),
                session_id: record.route_session_id.clone(),
                session,
                loader_id,
            });
            if current == main_frame_id {
                break;
            }
            current = record.parent.as_deref().ok_or_else(|| {
                BrowserError::operation(
                    "freeze locator frame lineage",
                    super::OperationPhase::Observation,
                )
                .with_message(format!(
                    "frame {current} lineage does not reach the main frame"
                ))
            })?;
        }

        let frozen_resolved = routes.first().expect("resolved frame route is present");
        if frozen_resolved.document_epoch != resolved_route.document_epoch
            || frozen_resolved.loader_id != resolved_route.loader_id
            || frozen_resolved.session_id != resolved_route.session_id
        {
            return Err(BrowserError::operation(
                "freeze locator frame lineage",
                super::OperationPhase::Observation,
            )
            .with_message(format!(
                "frame {} changed during locator resolution",
                resolved_route.frame_id
            )));
        }
        Ok(routes)
    }

    pub(crate) fn freeze_route_scopes(&self, route_session_id: &str) -> Vec<FrameScopeIdentity> {
        let state = self.state.read();
        let frame_ids = state
            .graph
            .frames
            .iter()
            .filter(|(_, record)| {
                record.route_active && record.route_session_id == route_session_id
            })
            .map(|(frame_id, _)| FrameId::new(frame_id))
            .collect::<Vec<_>>();
        drop(state);
        let mut scopes = Vec::new();
        for frame_id in frame_ids {
            if let Some(lineage) = self.freeze_frame_lineage(&frame_id) {
                for scope in lineage {
                    if !scopes.contains(&scope) {
                        scopes.push(scope);
                    }
                }
            }
        }
        scopes
    }

    fn validate(&self, frame: &Frame) -> Result<(), BrowserError> {
        let page = self.page().ok_or_else(|| {
            BrowserError::operation("use frame", super::OperationPhase::Preparation)
                .with_message("page was dropped")
        })?;
        if let Some(error) = page.terminal_route_error() {
            return Err(error);
        }
        page.lifecycle()
            .validate_page(frame.snapshot.page_generation)
            .map_err(|reason| self.invalidation_error(frame, reason))?;
        let state = self.state.read();
        let graph = &state.graph;
        let current = graph
            .snapshot(frame.id.as_str(), frame.snapshot.page_generation)
            .ok_or_else(|| {
                BrowserError::operation("use frame", super::OperationPhase::Preparation)
                    .with_message(format!("frame {} is detached", frame.id))
            })?;
        if !graph.is_route_active(frame.id.as_str()) {
            return Err(
                BrowserError::operation("use frame", super::OperationPhase::Preparation)
                    .with_message(format!("frame {} route is not active", frame.id)),
            );
        }
        if current.document_epoch != frame.snapshot.document_epoch {
            return Err(self.invalidation_error(frame, InvalidationReason::DocumentChanged));
        }
        Ok(())
    }

    fn invalidation_error(&self, frame: &Frame, reason: InvalidationReason) -> BrowserError {
        BrowserError::operation("use frame", super::OperationPhase::Preparation)
            .with_message(format!("frame {} is stale: {reason:?}", frame.id))
    }

    #[allow(dead_code)] // Used by Task 2 resolution before Task 4 wires actions to it.
    pub(super) fn locator_route(&self, frame: &Frame) -> Result<LocatorFrameRoute, BrowserError> {
        self.validate(frame)?;
        let state = self.state.read();
        let session = state.routed_session(frame.id.as_str()).ok_or_else(|| {
            BrowserError::operation(
                "resolve locator frame route",
                super::OperationPhase::Preparation,
            )
            .with_message(format!("frame {} is detached", frame.id))
        })?;
        let loader_id = state
            .graph
            .loader_id(frame.id.as_str())
            .ok_or_else(|| {
                BrowserError::operation(
                    "resolve locator frame route",
                    super::OperationPhase::Preparation,
                )
                .with_message(format!("frame {} has no document identity", frame.id))
            })?
            .to_owned();
        Ok(LocatorFrameRoute {
            page_generation: frame.snapshot.page_generation,
            document_epoch: frame.snapshot.document_epoch,
            frame_id: frame.id.clone(),
            session_id: session.id().to_owned(),
            session,
            loader_id,
        })
    }

    pub(super) async fn validate_locator_route_authoritative(
        &self,
        route: &LocatorFrameRoute,
    ) -> Result<AuthoritativeFrameIdentity, BrowserError> {
        let mut batch = self.validate_locator_routes_authoritative(&[route]).await?;
        batch
            .identities
            .remove(route.frame_id.as_str())
            .ok_or_else(|| {
                BrowserError::operation(
                    "confirm frame document identity",
                    super::OperationPhase::Confirmation,
                )
                .with_message(format!(
                    "frame {} is stale: document is absent",
                    route.frame_id
                ))
            })
    }

    pub(super) async fn validate_locator_routes_authoritative(
        &self,
        routes: &[&LocatorFrameRoute],
    ) -> Result<AuthoritativeFrameBatch, BrowserError> {
        if let Some(error) = self.page().and_then(|page| page.terminal_route_error()) {
            return Err(error);
        }
        let mut by_session: HashMap<&str, Vec<&LocatorFrameRoute>> = HashMap::new();
        for route in routes {
            by_session
                .entry(route.session_id.as_str())
                .or_default()
                .push(route);
        }
        let mut frame_ids = BTreeSet::new();
        let mut identities = HashMap::new();
        #[cfg(test)]
        let mut indexed_frame_nodes = 0;
        for session_routes in by_session.into_values() {
            let tree = GetFrameTree::new()
                .send(&session_routes[0].session)
                .await
                .map_err(|error| {
                    BrowserError::cdp_operation(
                        "confirm frame document identity",
                        super::OperationPhase::Confirmation,
                        error,
                    )
                })?
                .frame_tree;
            let mut frame_index = HashMap::new();
            let visits = index_frame_tree(&tree, &mut frame_index, &mut frame_ids);
            #[cfg(test)]
            {
                indexed_frame_nodes += visits;
            }
            #[cfg(not(test))]
            let _ = visits;
            for route in session_routes {
                let current_frame = frame_index
                    .get(route.frame_id.as_str())
                    .copied()
                    .ok_or_else(|| {
                        BrowserError::operation(
                            "confirm frame document identity",
                            super::OperationPhase::Confirmation,
                        )
                        .with_message(format!(
                            "frame {} is stale: document is absent",
                            route.frame_id
                        ))
                    })?;
                let identity = self.validate_route_in_tree(route, current_frame)?;
                identities.insert(route.frame_id.as_str().to_owned(), identity);
            }
        }
        Ok(AuthoritativeFrameBatch {
            frame_ids,
            identities,
            #[cfg(test)]
            indexed_frame_nodes,
        })
    }

    pub(super) async fn validate_locator_lineage_authoritative(
        &self,
        routes: &[&LocatorFrameRoute],
    ) -> Result<AuthoritativeFrameBatch, BrowserError> {
        let batch = self.validate_locator_routes_authoritative(routes).await?;
        for boundary in routes.windows(2) {
            let child = boundary[0];
            let parent = boundary[1];
            let child_identity = batch
                .identities
                .get(child.frame_id.as_str())
                .expect("every validated child route has an authoritative identity");
            let parent_identity = batch
                .identities
                .get(parent.frame_id.as_str())
                .expect("every validated parent route has an authoritative identity");
            let reciprocal_parent_link = child.session_id != parent.session_id
                || parent_identity
                    .child_ids
                    .iter()
                    .any(|frame_id| frame_id == child.frame_id.as_str());
            if child_identity.parent_id.as_deref() != Some(parent.frame_id.as_str())
                || !reciprocal_parent_link
            {
                return Err(BrowserError::operation(
                    "confirm frame document identity",
                    super::OperationPhase::Confirmation,
                )
                .with_message(format!(
                    "frame {} lineage changed relative to parent {}",
                    child.frame_id, parent.frame_id
                )));
            }
        }
        Ok(batch)
    }

    fn validate_route_in_tree(
        &self,
        route: &LocatorFrameRoute,
        current_frame: &FrameTree,
    ) -> Result<AuthoritativeFrameIdentity, BrowserError> {
        if current_frame.frame.loader_id != route.loader_id {
            return Err(BrowserError::operation(
                "confirm frame document identity",
                super::OperationPhase::Confirmation,
            )
            .with_message(format!(
                "frame {} is stale: {:?}",
                route.frame_id,
                InvalidationReason::DocumentChanged
            )));
        }
        let parent_id = current_frame.frame.parent_id.clone();
        let child_ids = current_frame
            .child_frames
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|child| child.frame.id.clone())
            .collect();
        self.validate_locator_route(route)?;
        Ok(AuthoritativeFrameIdentity {
            parent_id,
            child_ids,
        })
    }

    pub(super) fn validate_locator_route(
        &self,
        route: &LocatorFrameRoute,
    ) -> Result<(), BrowserError> {
        let page = self.page().ok_or_else(|| {
            BrowserError::operation(
                "validate locator frame route",
                super::OperationPhase::Confirmation,
            )
            .with_message("page was dropped")
        })?;
        if let Some(error) = page.terminal_route_error() {
            return Err(error);
        }
        page.lifecycle()
            .validate_page(route.page_generation)
            .map_err(|reason| {
                BrowserError::operation(
                    "validate locator frame route",
                    super::OperationPhase::Confirmation,
                )
                .with_message(format!("frame {} is stale: {reason:?}", route.frame_id))
            })?;
        let state = self.state.read();
        let current = state
            .graph
            .snapshot(route.frame_id.as_str(), route.page_generation)
            .ok_or_else(|| {
                BrowserError::operation(
                    "validate locator frame route",
                    super::OperationPhase::Confirmation,
                )
                .with_message(format!("frame {} is detached", route.frame_id))
            })?;
        if !state.graph.is_route_active(route.frame_id.as_str()) {
            return Err(BrowserError::operation(
                "validate locator frame route",
                super::OperationPhase::Confirmation,
            )
            .with_message(format!("frame {} route is not active", route.frame_id)));
        }
        if current.document_epoch != route.document_epoch {
            return Err(BrowserError::operation(
                "validate locator frame route",
                super::OperationPhase::Confirmation,
            )
            .with_message(format!(
                "frame {} is stale: {:?}",
                route.frame_id,
                InvalidationReason::DocumentChanged
            )));
        }
        let current_session = state
            .routed_session(route.frame_id.as_str())
            .ok_or_else(|| {
                BrowserError::operation(
                    "validate locator frame route",
                    super::OperationPhase::Confirmation,
                )
                .with_message(format!("frame {} is detached", route.frame_id))
            })?;
        if current_session.id() != route.session_id {
            return Err(BrowserError::operation(
                "validate locator frame route",
                super::OperationPhase::Confirmation,
            )
            .with_message(format!(
                "frame {} route changed during locator resolution",
                route.frame_id
            )));
        }
        Ok(())
    }

    pub(crate) async fn cleanup_auto_attached_targets(&self) -> super::CloseReport {
        let mut report = super::CloseReport::new("auto-attached-targets");
        let auxiliaries = self.auxiliary_targets.take_all();
        if let Some(page) = self.page() {
            if let Some(manager) = page.network_manager() {
                for auxiliary in &auxiliaries {
                    if auxiliary.network_route {
                        manager.remove_route(&auxiliary.session_id).await;
                    }
                }
            }
        }
        drop(auxiliaries);
        for (resource, result) in self.auxiliary_targets.cleanup_all().await {
            report = match result {
                Ok(()) => report.closed(resource),
                Err(error) => report.failed(resource, error.to_string()),
            };
        }
        for (resource, result) in self.auto_attached_target_cleanups.cleanup_all().await {
            report = match result {
                Ok(()) => report.closed(resource),
                Err(error) => report.failed(resource, error.to_string()),
            };
        }
        report
    }

    pub(crate) async fn finalize_after_target_destroyed(&self) -> super::CloseReport {
        self.cancel();
        self.file_chooser_state.lock().await.close_locally();
        self.pending_oopif_initializations.abandon_all();
        self.auto_attached_target_cleanups.abandon_all();
        let mut report = super::CloseReport::new("auto-attached-targets");
        for (resource, result) in self
            .auxiliary_targets
            .cleanup_after_parent_destroyed()
            .await
        {
            report = match result {
                Ok(()) => report.closed(resource),
                Err(error) if error.is_missing_session() => report.closed(resource),
                Err(error) => report.failed(resource, error.to_string()),
            };
        }
        report
    }

    #[cfg(test)]
    pub(crate) fn schedule_auto_attached_targets(&self) {
        self.auxiliary_targets.schedule_all();
    }

    pub(crate) fn subscribe_main_document_applied(
        &self,
    ) -> tokio::sync::broadcast::Receiver<AppliedMainDocument> {
        self.main_document_applied.subscribe()
    }

    pub(crate) async fn wait_main_document_applied(
        &self,
        receiver: &mut tokio::sync::broadcast::Receiver<AppliedMainDocument>,
        frame_id: &str,
        loader_id: &str,
    ) -> Result<(), BrowserError> {
        let already_applied = {
            let state = self.state.read();
            state.graph.main_frame_id() == Some(frame_id)
                && state.graph.loader_id(frame_id) == Some(loader_id)
        };
        if already_applied {
            return Ok(());
        }
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    return Err(BrowserError::operation(
                        "confirm local navigation document",
                        super::OperationPhase::Confirmation,
                    ).with_message("frame store closed before the main document was applied"));
                }
                event = receiver.recv() => match event {
                    Ok(event) if event.frame_id == frame_id && event.loader_id == loader_id => return Ok(()),
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        return Err(BrowserError::operation(
                            "confirm local navigation document",
                            super::OperationPhase::Confirmation,
                        ).with_message(format!("main-document reducer acknowledgement lagged by {skipped} events")));
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return Err(BrowserError::operation(
                            "confirm local navigation document",
                            super::OperationPhase::Confirmation,
                        ).with_message("main-document reducer acknowledgement closed"));
                    }
                }
            }
        }
    }

    pub(crate) fn cancel(&self) {
        self.cancel.cancel();
        self.context_changed.notify_waiters();
    }

    fn page(&self) -> Option<Page> {
        self.page.upgrade().map(Page::from_inner)
    }
}

impl Drop for FrameStore {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[derive(Clone)]
/// Stable logical frame handle whose protocol route may move to an OOPIF Session.
pub struct Frame {
    page: Page,
    id: FrameId,
    snapshot: FrameSnapshot,
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Frame")
            .field("id", &self.id)
            .field("snapshot", &self.snapshot)
            .finish_non_exhaustive()
    }
}

impl Frame {
    /// Captures this frame's current viewport region from the top-level page.
    ///
    /// For embedded frames, the viewport is the frame owner's content aperture,
    /// not the embedded document's content or `documentElement` box.
    pub async fn screenshot(
        &self,
        options: super::ScreenshotOptions,
    ) -> Result<super::ArtifactBytes, BrowserError> {
        super::artifact::screenshot_frame(self, options).await
    }

    pub async fn html(
        &self,
        options: super::HtmlOptions,
    ) -> Result<super::HtmlArtifact, BrowserError> {
        super::artifact::frame_html(self, options).await
    }

    /// Subscribes to future network facts associated with this frame subtree.
    pub async fn subscribe_network_events(
        &self,
    ) -> Result<super::NetworkEventStream, BrowserError> {
        super::network::subscribe_frame(self).await
    }

    pub(crate) fn scope_identity(&self) -> FrameScopeIdentity {
        FrameScopeIdentity {
            frame_id: self.id.clone(),
            snapshot: self.snapshot,
        }
    }

    pub async fn wait_for_dom_stability(
        &self,
        options: super::WaitOptions,
    ) -> Result<(), BrowserError> {
        super::wait::wait_frame_stability(self, options).await
    }

    pub async fn press(&self, key: &str) -> Result<(), BrowserError> {
        super::action::frame_press(self, key).await
    }
    pub async fn type_text(&self, text: &str) -> Result<(), BrowserError> {
        super::action::frame_type_text(self, text).await
    }
    pub async fn move_pointer(&self, x: f64, y: f64) -> Result<(), BrowserError> {
        super::action::frame_move_pointer(self, x, y).await
    }
    pub async fn click_at(&self, x: f64, y: f64) -> Result<(), BrowserError> {
        super::action::frame_click_at(self, x, y).await
    }
    pub async fn scroll(&self, delta_x: f64, delta_y: f64) -> Result<(), BrowserError> {
        super::action::frame_scroll(self, delta_x, delta_y).await
    }

    /// Captures bounded, structured facts for this frame's current document.
    pub async fn snapshot(
        &self,
        options: super::SnapshotOptions,
    ) -> Result<super::FrameSnapshotView, BrowserError> {
        super::snapshot::capture_frame(self, options).await
    }

    /// Creates a lazy locator scoped to this frame's current document.
    pub fn locator(&self, query: impl Into<super::LocatorQuery>) -> super::Locator {
        super::Locator::for_frame(self.clone(), query.into())
    }

    pub(crate) async fn validate_locator_scope(&self) -> Result<(), BrowserError> {
        let store = self.page.frame_store().await?;
        store.validate(self)
    }

    pub fn id(&self) -> &FrameId {
        &self.id
    }

    pub fn page(&self) -> &Page {
        &self.page
    }

    pub fn document_epoch(&self) -> DocumentEpoch {
        self.snapshot.document_epoch
    }

    /// Returns the parent frame after validating this handle's generation.
    pub async fn parent(&self) -> Result<Option<Frame>, BrowserError> {
        let store = self.page.frame_store().await?;
        store.validate(self)?;
        let parent = store
            .state
            .read()
            .graph
            .parent(self.id.as_str())
            .map(str::to_owned);
        Ok(parent.and_then(|id| store.handle(&id)))
    }

    /// Returns direct child frames after validating this handle's generation.
    pub async fn children(&self) -> Result<Vec<Frame>, BrowserError> {
        let store = self.page.frame_store().await?;
        store.validate(self)?;
        let ids = store
            .state
            .read()
            .graph
            .children(self.id.as_str())
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        Ok(ids.iter().filter_map(|id| store.handle(id)).collect())
    }

    /// Returns the cdpkit Session currently carrying commands for this frame.
    ///
    /// This is the page Session for in-process frames and the attached child
    /// Session for out-of-process frames.
    pub async fn cdp_session(&self) -> Result<cdpkit::Session, BrowserError> {
        let store = self.page.frame_store().await?;
        store.validate(self)?;
        store
            .state
            .read()
            .routed_session(self.id.as_str())
            .ok_or_else(|| {
                BrowserError::operation("resolve frame session", super::OperationPhase::Preparation)
                    .with_message(format!("frame {} is detached", self.id))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{DocumentEpoch, PageGeneration};
    use futures::{SinkExt, StreamExt};
    use serde_json::{json, Value};
    use std::time::Duration;
    use tokio_tungstenite::tungstenite::Message;

    fn graph() -> FrameGraph {
        FrameGraph::new("page-session")
    }

    #[test]
    fn authoritative_frame_index_visits_each_tree_node_once() {
        let tree: FrameTree = serde_json::from_value(json!({
            "frame": {
                "id": "main", "loaderId": "loader-main", "url": "about:blank",
                "domainAndRegistry": "", "securityOrigin": "null", "mimeType": "text/html",
                "secureContextType": "InsecureScheme",
                "crossOriginIsolatedContextType": "NotIsolated", "gatedAPIFeatures": []
            },
            "childFrames": [{"frame": {
                "id": "child", "parentId": "main", "loaderId": "loader-child",
                "url": "about:blank", "domainAndRegistry": "", "securityOrigin": "null",
                "mimeType": "text/html", "secureContextType": "InsecureScheme",
                "crossOriginIsolatedContextType": "NotIsolated", "gatedAPIFeatures": []
            }}]
        }))
        .unwrap();
        let mut index = HashMap::new();
        let mut ids = BTreeSet::new();

        let visits = index_frame_tree(&tree, &mut index, &mut ids);

        assert_eq!(visits, 2);
        assert_eq!(index.len(), 2);
        assert_eq!(ids, BTreeSet::from(["child".to_owned(), "main".to_owned()]));
    }

    #[test]
    fn recursive_children_keep_stable_logical_identity() {
        let mut graph = graph();
        graph.attach("main", None);
        graph.attach("child", Some("main"));
        graph.attach("grandchild", Some("child"));

        assert_eq!(graph.main_frame_id(), Some("main"));
        assert_eq!(graph.children("main"), vec!["child"]);
        assert_eq!(graph.children("child"), vec!["grandchild"]);
    }

    #[test]
    fn child_before_parent_is_reconciled_when_parent_arrives() {
        let mut graph = graph();
        graph.attach("child", Some("main"));
        graph.attach("main", None);

        assert_eq!(graph.main_frame_id(), Some("main"));
        assert_eq!(graph.parent("child"), Some("main"));
        assert_eq!(graph.children("main"), vec!["child"]);
    }

    #[test]
    fn oopif_swap_does_not_remove_the_logical_frame() {
        use cdpkit::page::types::FrameDetachedReason;

        assert!(!should_remove_detached_frame(&FrameDetachedReason::Swap));
        assert!(should_remove_detached_frame(&FrameDetachedReason::Remove));
        assert!(!should_remove_detached_frame(
            &FrameDetachedReason::UnknownValue("future-reason".to_owned())
        ));
    }

    #[test]
    fn detach_removes_the_entire_subtree() {
        let mut graph = graph();
        graph.attach("main", None);
        graph.attach("child", Some("main"));
        graph.attach("grandchild", Some("child"));

        graph.detach("child");

        assert!(graph.contains("main"));
        assert!(!graph.contains("child"));
        assert!(!graph.contains("grandchild"));
    }

    #[test]
    fn only_cross_document_navigation_increments_document_epoch() {
        let mut graph = graph();
        graph.navigate("main", None, "loader-1");
        let first = graph.snapshot("main", PageGeneration::initial()).unwrap();

        graph.navigate("main", None, "loader-1");
        assert_eq!(
            graph.snapshot("main", PageGeneration::initial()).unwrap(),
            first
        );

        graph.navigate("main", None, "loader-2");
        assert_eq!(
            graph
                .snapshot("main", PageGeneration::initial())
                .unwrap()
                .document_epoch,
            DocumentEpoch::new(first.document_epoch.get() + 1)
        );
    }

    #[test]
    fn old_frame_store_cannot_mint_handles_for_a_replaced_page_generation() {
        let lifecycle = crate::runtime::PageLifecycle::new(PageGeneration::initial());
        let store_identity = FrameStoreIdentity::new(lifecycle.snapshot().page_generation);
        let mut graph = graph();
        graph.navigate("main", None, "loader-main");

        lifecycle.replace_target();
        let handle_snapshot = store_identity.snapshot(&graph, "main").unwrap();

        assert_eq!(handle_snapshot.page_generation, PageGeneration::initial());
        assert_eq!(
            lifecycle.validate_page(handle_snapshot.page_generation),
            Err(InvalidationReason::PageReplaced)
        );
    }

    async fn start_frame_store_cdp_server(
        emit_events: bool,
        close_after_events: bool,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut write, mut read) = websocket.split();
            let mut events_sent = false;
            while let Some(Ok(Message::Text(text))) = read.next().await {
                let command: Value = serde_json::from_str(&text).unwrap();
                let id = command["id"].as_u64().unwrap();
                let method = command["method"].as_str().unwrap();
                let result = match method {
                    "Browser.getVersion" => crate::runtime::test_browser_version_result(),
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
                    "Page.enable"
                    | "Runtime.enable"
                    | "Runtime.setAsyncCallStackDepth"
                    | "Target.setAutoAttach" => json!({}),
                    other => panic!("unexpected fake CDP command: {other}"),
                };
                let mut response = json!({"id": id, "result": result});
                if let Some(session_id) = command.get("sessionId") {
                    response["sessionId"] = session_id.clone();
                }
                write
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .unwrap();
                if emit_events && method == "Target.setAutoAttach" {
                    let session_id = command["sessionId"].clone();
                    let events = [
                        json!({"method":"Page.frameAttached","sessionId":session_id,"params":{"frameId":"child","parentFrameId":"main"}}),
                        json!({"method":"Page.frameNavigated","sessionId":session_id,"params":{"frame":{"id":"child","parentId":"main","loaderId":"loader-child","url":"https://example.test/child","domainAndRegistry":"example.test","securityOrigin":"https://example.test","mimeType":"text/html","secureContextType":"Secure","crossOriginIsolatedContextType":"NotIsolated","gatedAPIFeatures":[]},"type":"Navigation"}}),
                        json!({"method":"Page.navigatedWithinDocument","sessionId":session_id,"params":{"frameId":"child","url":"https://example.test/child#ready","navigationType":"fragment"}}),
                        json!({"method":"Runtime.consoleAPICalled","sessionId":session_id,"params":{"type":"log","args":[{"type":"string","value":"ready"}],"executionContextId":1,"timestamp":1.0}}),
                        json!({"method":"Runtime.exceptionThrown","sessionId":session_id,"params":{"timestamp":2.0,"exceptionDetails":{"exceptionId":3,"text":"Uncaught boom","lineNumber":1,"columnNumber":2}}}),
                        json!({"method":"Page.javascriptDialogOpening","sessionId":session_id,"params":{"url":"https://example.test/child","frameId":"child","message":"confirm?","type":"confirm","hasBrowserHandler":true,"defaultPrompt":null}}),
                        json!({"method":"Page.javascriptDialogClosed","sessionId":session_id,"params":{"frameId":"child","result":false,"userInput":""}}),
                        json!({"method":"Page.frameDetached","sessionId":session_id,"params":{"frameId":"child","reason":"remove"}}),
                    ];
                    for event in events {
                        write
                            .send(Message::Text(event.to_string().into()))
                            .await
                            .unwrap();
                    }
                    events_sent = true;
                }
                if close_after_events && events_sent && method == "Runtime.setAsyncCallStackDepth" {
                    break;
                }
            }
        });
        (format!("ws://{address}"), server)
    }

    #[tokio::test]
    async fn initialized_frame_store_handle_stays_stale_after_target_destroyed() {
        use crate::runtime::{BrowserRuntime, BrowserSessionId, InvalidationReason, PageOwnership};

        let (url, server) = start_frame_store_cdp_server(false, false).await;
        let runtime = BrowserRuntime::connect(url).await.unwrap();
        let page = Page::new(
            runtime.clone(),
            BrowserSessionId::new("owner-session"),
            Weak::new(),
            "target".to_owned(),
            PageOwnership::Attached,
            runtime.cdp().session("page-session"),
        );
        let store = page.frame_store().await.unwrap().clone();

        page.invalidate_target();
        let frame = store
            .handle("main")
            .expect("initialized store should retain its logical frame graph");

        assert_eq!(frame.snapshot.page_generation, PageGeneration::initial());
        assert!(matches!(
            store.validate(&frame),
            Err(error) if error.to_string().contains(&format!("{:?}", InvalidationReason::PageReplaced))
        ));
        let report = runtime.close().await;
        assert!(report.is_complete(), "runtime cleanup failed: {report:?}");
        server
            .await
            .expect("fake CDP server should shut down cleanly");
    }

    #[tokio::test]
    async fn frame_reducer_broadcasts_navigation_console_error_and_detach_facts() {
        use crate::runtime::{BrowserRuntime, BrowserSessionId, PageEvent, PageOwnership};

        let (url, server) = start_frame_store_cdp_server(true, false).await;
        let runtime = BrowserRuntime::connect(url).await.unwrap();
        let page = Page::new(
            runtime.clone(),
            BrowserSessionId::new("owner-session"),
            Weak::new(),
            "target".to_owned(),
            PageOwnership::Attached,
            runtime.cdp().session("page-session"),
        );
        let mut events = page.subscribe_events().await.unwrap();
        page.frame_store().await.unwrap();

        let facts = tokio::time::timeout(Duration::from_secs(1), async {
            let mut facts = Vec::new();
            while facts.len() < 8 {
                facts.push(events.next().await.unwrap().unwrap());
            }
            facts
        })
        .await
        .expect("all typed page facts");
        assert!(facts
            .windows(2)
            .all(|pair| pair[0].metadata().sequence() < pair[1].metadata().sequence()));
        assert!(facts
            .iter()
            .any(|fact| matches!(fact.event(), PageEvent::FrameAttached { .. })));
        assert!(facts.iter().any(|fact| matches!(
            fact.event(),
            PageEvent::FrameNavigated {
                same_document: false,
                ..
            }
        )));
        assert!(facts.iter().any(|fact| matches!(
            fact.event(),
            PageEvent::FrameNavigated {
                same_document: true,
                ..
            }
        )));
        assert!(facts.iter().any(|fact| matches!(fact.event(), PageEvent::Console(message) if message.arguments[0].value == Some(json!("ready")))));
        assert!(facts.iter().any(|fact| matches!(fact.event(), PageEvent::JavaScriptError(error) if error.text == "Uncaught boom")));
        assert!(facts.iter().any(|fact| matches!(fact.event(), PageEvent::DialogOpened { message, dialog_type, .. } if message == "confirm?" && dialog_type == &crate::runtime::DialogType::Confirm)));
        assert!(facts.iter().any(|fact| matches!(
            fact.event(),
            PageEvent::DialogClosed {
                accepted: false,
                ..
            }
        )));
        assert!(facts
            .iter()
            .filter(|fact| matches!(
                fact.event(),
                PageEvent::Console(_) | PageEvent::JavaScriptError(_)
            ))
            .all(|fact| fact.metadata().identity().routed_session_id() == Some("page-session")));
        assert!(facts
            .iter()
            .any(|fact| matches!(fact.event(), PageEvent::FrameDetached { .. })));

        assert!(runtime.close().await.is_complete());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn underlying_cdp_stream_close_terminates_page_events_as_source_closed() {
        use crate::runtime::{
            BrowserRuntime, BrowserSessionId, EventStreamCloseReason, PageOwnership,
        };

        let (url, server) = start_frame_store_cdp_server(true, true).await;
        let runtime = BrowserRuntime::connect(url).await.unwrap();
        let page = Page::new(
            runtime.clone(),
            BrowserSessionId::new("owner-session"),
            Weak::new(),
            "target".to_owned(),
            PageOwnership::Attached,
            runtime.cdp().session("page-session"),
        );
        let mut events = page.subscribe_events().await.unwrap();
        page.frame_store().await.unwrap();
        let reason = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Err(error) = events.next().await.expect("terminal event") {
                    break error.reason();
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(reason, EventStreamCloseReason::SourceClosed);
        assert!(events.next().await.is_none());
        server.await.unwrap();
    }

    #[test]
    fn oopif_route_changes_without_changing_frame_identity() {
        let mut graph = graph();
        graph.navigate("main", None, "loader-main");
        graph.navigate("child", Some("main"), "loader-child");
        let before = graph.ids();

        let previous = graph.route_to_session("child", "oopif-session", Some("oopif-target"));
        assert_eq!(previous.as_deref(), Some("page-session"));
        assert_eq!(graph.route_session("child"), Some("oopif-session"));
        assert_eq!(graph.ids(), before);

        graph.reroute_session("oopif-session", "page-session");
        assert_eq!(graph.route_session("child"), Some("page-session"));
        assert_eq!(graph.ids(), before);
    }

    #[test]
    fn detached_nested_oopif_routes_through_its_parent_session() {
        let mut graph = graph();
        graph.navigate("main", None, "loader-main");
        graph.navigate("child", Some("main"), "loader-child");
        graph.navigate("grandchild", Some("child"), "loader-grandchild");
        graph.route_to_session("child", "child-session", Some("child-target"));
        graph.route_to_session(
            "grandchild",
            "grandchild-session",
            Some("grandchild-target"),
        );

        graph.reroute_session("grandchild-session", "child-session");
        assert_eq!(graph.route_session("grandchild"), Some("child-session"));

        graph.reroute_session("child-session", "page-session");
        assert_eq!(graph.route_session("child"), Some("page-session"));
    }

    #[test]
    fn oopif_route_ignores_targets_from_another_page() {
        let mut graph = graph();
        graph.navigate("main", None, "loader-main");

        assert!(!graph.route_oopif(
            "foreign-child",
            Some("foreign-main"),
            Some("page-session"),
            "foreign-session",
            "foreign-target",
        ));
        assert!(!graph.contains("foreign-child"));

        assert!(!graph.route_oopif(
            "child",
            Some("main"),
            Some("another-page-session"),
            "foreign-session",
            "foreign-target",
        ));
        assert!(graph.route_oopif(
            "child",
            Some("main"),
            Some("page-session"),
            "oopif-session",
            "oopif-target",
        ));
        assert_eq!(graph.parent("child"), Some("main"));
        assert_eq!(graph.route_session("child"), Some("oopif-session"));
        assert!(!graph.route_oopif(
            "child",
            Some("main"),
            Some("page-session"),
            "second-session",
            "oopif-target",
        ));
        assert_eq!(graph.route_session("child"), Some("oopif-session"));
    }

    #[test]
    fn detaching_a_session_reroutes_every_frame_it_carried() {
        let mut graph = graph();
        graph.navigate("main", None, "loader-main");
        graph.navigate("child", Some("main"), "loader-child");
        graph.navigate("leaf", Some("child"), "loader-leaf");
        graph.navigate("nested", Some("child"), "loader-nested");
        graph.route_to_session("child", "child-session", Some("child-target"));
        graph.route_to_session("leaf", "child-session", None);
        graph.route_to_session("nested", "nested-session", Some("nested-target"));

        graph.reroute_session("child-session", "page-session");

        assert_eq!(graph.route_session("child"), Some("page-session"));
        assert_eq!(graph.route_session("leaf"), Some("page-session"));
        assert_eq!(graph.route_session("nested"), Some("nested-session"));
    }

    #[test]
    fn nested_session_subtree_is_collected_for_cascade_cleanup() {
        let parents = HashMap::from([
            ("child".to_owned(), "page".to_owned()),
            ("grandchild".to_owned(), "child".to_owned()),
            ("sibling".to_owned(), "page".to_owned()),
        ]);

        assert_eq!(
            collect_session_subtree(&parents, "child"),
            BTreeSet::from(["child".to_owned(), "grandchild".to_owned()])
        );
    }

    #[test]
    fn existing_session_id_conflicts_are_ignored_without_detach() {
        assert_eq!(
            classify_session_attach(
                Some(("child-frame", "page-session")),
                "child-session",
                "child-frame",
                Some("page-session"),
            ),
            SessionAttachDisposition::Idempotent
        );
        assert_eq!(
            classify_session_attach(
                Some(("child-frame", "page-session")),
                "child-session",
                "different-frame",
                Some("page-session"),
            ),
            SessionAttachDisposition::Conflict
        );
        assert_eq!(
            classify_session_attach(None, "child-session", "child-frame", Some("child-session"),),
            SessionAttachDisposition::Conflict
        );
    }

    #[test]
    fn detach_fallback_rejects_missing_and_cyclic_parent_sessions() {
        let live = BTreeSet::from(["page-session".to_owned(), "child".to_owned()]);
        let missing_parent = HashMap::from([("child".to_owned(), "missing".to_owned())]);
        assert_eq!(
            resolve_live_parent(
                &missing_parent,
                &live,
                &BTreeSet::from(["child".to_owned()]),
                "child",
                "page-session",
            ),
            "page-session"
        );

        let cycle = HashMap::from([
            ("child".to_owned(), "grandchild".to_owned()),
            ("grandchild".to_owned(), "child".to_owned()),
        ]);
        assert_eq!(
            resolve_live_parent(
                &cycle,
                &live,
                &BTreeSet::from(["child".to_owned()]),
                "child",
                "page-session",
            ),
            "page-session"
        );
    }

    fn frame_state() -> FrameState {
        let mut graph = graph();
        graph.navigate("main", None, "loader-main");
        FrameState {
            graph,
            sessions: HashMap::new(),
            child_sessions: HashMap::new(),
            next_attach_token: 0,
            execution_contexts: Vec::new(),
        }
    }

    #[test]
    fn runtime_context_events_apply_in_wire_order_with_numeric_id_reuse() {
        let mut state = frame_state();
        let created = |unique_id: &str| {
            json!({"context": {
                "id": 7,
                "origin": "https://child.test",
                "name": "",
                "uniqueId": unique_id,
                "auxData": {"isDefault": true, "type": "default", "frameId": "child"}
            }})
        };

        apply_runtime_context_event(
            &mut state,
            "child-session",
            "Runtime.executionContextCreated",
            created("child-old"),
        )
        .unwrap();
        apply_runtime_context_event(
            &mut state,
            "child-session",
            "Runtime.executionContextsCleared",
            json!({}),
        )
        .unwrap();
        apply_runtime_context_event(
            &mut state,
            "child-session",
            "Runtime.executionContextCreated",
            created("child-current"),
        )
        .unwrap();
        apply_runtime_context_event(
            &mut state,
            "child-session",
            "Runtime.executionContextDestroyed",
            json!({"executionContextId": 7, "executionContextUniqueId": "child-old"}),
        )
        .unwrap();
        assert!(!apply_runtime_context_event(
            &mut state,
            "child-session",
            "Runtime.executionContextFutureEvent",
            json!({"unexpected": true}),
        )
        .unwrap());

        assert_eq!(
            state
                .default_context("child-session", "child")
                .map(|context| (context.id, context.unique_id)),
            Some((7, "child-current".to_owned()))
        );
    }

    #[test]
    fn detached_initializing_session_is_cancelled_and_cannot_commit_late() {
        let mut state = frame_state();
        let claim = state.begin_oopif_attach(
            "child-session",
            "child",
            Some("main"),
            Some("page-session"),
            CancellationToken::new(),
        );
        let OopifAttachStart::Initialize(claim) = claim else {
            panic!("valid child session should start initialization");
        };

        let _ = state.detach_child_session("child-session");

        assert!(claim.cancel.is_cancelled());
        assert_eq!(
            state.child_session_phase("child-session"),
            Some(ChildSessionPhase::Detached(claim.token))
        );
        assert!(!state.activate_oopif_attach("child-session", claim.token));
        assert_eq!(state.graph.route_session("child"), None);
    }

    #[test]
    fn attach_commit_requires_the_exact_initializing_token() {
        let mut state = frame_state();
        let OopifAttachStart::Initialize(claim) = state.begin_oopif_attach(
            "child-session",
            "child",
            Some("main"),
            Some("page-session"),
            CancellationToken::new(),
        ) else {
            panic!("valid child session should start initialization");
        };

        assert!(
            !state.activate_oopif_attach("child-session", AttachToken::new(claim.token.get() + 1),)
        );
        assert!(state.activate_oopif_attach("child-session", claim.token));
        assert_eq!(
            state.child_session_phase("child-session"),
            Some(ChildSessionPhase::Active(claim.token))
        );
    }

    #[test]
    fn initializing_ownership_is_idempotent_but_conflicting_reuse_is_rejected() {
        let mut state = frame_state();
        assert!(matches!(
            state.begin_oopif_attach(
                "child-session",
                "child",
                Some("main"),
                Some("page-session"),
                CancellationToken::new(),
            ),
            OopifAttachStart::Initialize(_)
        ));
        assert!(matches!(
            state.begin_oopif_attach(
                "child-session",
                "child",
                Some("main"),
                Some("page-session"),
                CancellationToken::new(),
            ),
            OopifAttachStart::Idempotent { active: false }
        ));
        assert!(matches!(
            state.begin_oopif_attach(
                "child-session",
                "other-child",
                Some("main"),
                Some("page-session"),
                CancellationToken::new(),
            ),
            OopifAttachStart::Conflict
        ));
    }

    #[test]
    fn foreign_parent_is_not_claimed_by_this_page() {
        let mut state = frame_state();

        assert!(matches!(
            state.begin_oopif_attach(
                "foreign-session",
                "foreign-child",
                Some("foreign-main"),
                Some("foreign-page-session"),
                CancellationToken::new(),
            ),
            OopifAttachStart::ForeignParent
        ));
        assert!(!state.child_sessions.contains_key("foreign-session"));
    }

    #[test]
    fn detaching_active_parent_cancels_initializing_descendant_subtree() {
        let mut state = frame_state();
        let OopifAttachStart::Initialize(parent_claim) = state.begin_oopif_attach(
            "child-session",
            "child",
            Some("main"),
            Some("page-session"),
            CancellationToken::new(),
        ) else {
            panic!("valid child session should start initialization");
        };
        assert!(state.graph.route_oopif(
            "child",
            Some("main"),
            Some("page-session"),
            "child-session",
            "child-target",
        ));
        assert!(state.activate_oopif_attach("child-session", parent_claim.token));

        let OopifAttachStart::Initialize(descendant_claim) = state.begin_oopif_attach(
            "grandchild-session",
            "grandchild",
            Some("child"),
            Some("child-session"),
            CancellationToken::new(),
        ) else {
            panic!("nested child session should start initialization");
        };

        let changes = state.detach_child_session("child-session");

        assert!(descendant_claim.cancel.is_cancelled());
        assert!(!state.child_sessions.contains_key("child-session"));
        assert_eq!(
            state.child_session_phase("grandchild-session"),
            Some(ChildSessionPhase::Detached(descendant_claim.token))
        );
        assert_eq!(state.graph.route_session("child"), Some("page-session"));
        assert!(changes
            .route_changes
            .iter()
            .any(|change| change.frame_id == "child"
                && change.previous_session_id == "child-session"
                && change.session_id == "page-session"));
        assert!(!state.activate_oopif_attach("grandchild-session", descendant_claim.token,));
    }

    #[test]
    fn detached_tombstone_is_acknowledged_by_exact_token_before_session_reuse() {
        let mut state = frame_state();
        let OopifAttachStart::Initialize(first_claim) = state.begin_oopif_attach(
            "child-session",
            "child",
            Some("main"),
            Some("page-session"),
            CancellationToken::new(),
        ) else {
            panic!("valid child session should start initialization");
        };
        let _ = state.detach_child_session("child-session");

        let wrong_token = AttachToken::new(first_claim.token.get() + 1);
        assert!(!state.acknowledge_detached("child-session", wrong_token));
        assert_eq!(
            state.child_session_phase("child-session"),
            Some(ChildSessionPhase::Detached(first_claim.token))
        );
        assert!(state.acknowledge_detached("child-session", first_claim.token));
        assert!(!state.child_sessions.contains_key("child-session"));

        let OopifAttachStart::Initialize(second_claim) = state.begin_oopif_attach(
            "child-session",
            "child",
            Some("main"),
            Some("page-session"),
            CancellationToken::new(),
        ) else {
            panic!("acknowledged session id should be reusable");
        };
        assert_ne!(second_claim.token, first_claim.token);
        assert!(!state.acknowledge_detached("child-session", first_claim.token));
        assert_eq!(
            state.child_session_phase("child-session"),
            Some(ChildSessionPhase::Initializing(second_claim.token))
        );
    }

    #[test]
    fn rollback_abandonment_is_acknowledged_after_commit_path_exits() {
        let mut state = frame_state();
        let OopifAttachStart::Initialize(claim) = state.begin_oopif_attach(
            "failed-session",
            "child",
            Some("main"),
            Some("page-session"),
            CancellationToken::new(),
        ) else {
            panic!("valid child session should start initialization");
        };

        assert!(state.abandon_oopif_attach("failed-session", claim.token));
        assert!(claim.cancel.is_cancelled());
        assert!(state.acknowledge_detached("failed-session", claim.token));
        assert!(!state.child_sessions.contains_key("failed-session"));
        assert!(!state.activate_oopif_attach("failed-session", claim.token));
    }

    fn oopif_frame_tree(root: &str, parent: Option<&str>, child: Option<(&str, &str)>) -> Value {
        let mut frame = json!({
            "id": root,
            "loaderId": format!("loader-{root}"),
            "url": format!("https://{root}.test/"),
            "domainAndRegistry": format!("{root}.test"),
            "securityOrigin": format!("https://{root}.test"),
            "mimeType": "text/html",
            "secureContextType": "Secure",
            "crossOriginIsolatedContextType": "NotIsolated",
            "gatedAPIFeatures": []
        });
        if let Some(parent) = parent {
            frame["parentId"] = json!(parent);
        }
        let mut tree = json!({"frame": frame});
        if let Some((child_id, child_parent)) = child {
            tree["childFrames"] = json!([oopif_frame_tree(child_id, Some(child_parent), None)]);
        }
        tree
    }

    fn main_frame_navigated_event(session_id: &Value) -> Value {
        let frame_tree = oopif_frame_tree("main", None, None);
        json!({
            "method": "Page.frameNavigated",
            "sessionId": session_id,
            "params": {
                "frame": frame_tree["frame"].clone(),
                "type": "Navigation"
            }
        })
    }

    fn attached_oopif_event(
        parent_session_id: &str,
        session_id: &str,
        frame_id: &str,
        parent_frame_id: &str,
    ) -> Value {
        json!({
            "method": "Target.attachedToTarget",
            "sessionId": parent_session_id,
            "params": {
                "sessionId": session_id,
                "targetInfo": {
                    "targetId": frame_id,
                    "type": "iframe",
                    "title": "",
                    "url": format!("https://{frame_id}.test/"),
                    "attached": true,
                    "canAccessOpener": false,
                    "parentFrameId": parent_frame_id
                },
                "waitingForDebugger": true
            }
        })
    }

    #[tokio::test]
    async fn nested_oopif_route_is_not_published_before_its_own_commit_fence() {
        use crate::runtime::{
            BrowserRuntime, ContextOptions, IsolatedSessionOptions, PageEvent, TargetRouteOptions,
            Viewport,
        };

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (attach_child, mut attach_child_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let nested_config_seen = Arc::new(tokio::sync::Notify::new());
        let release_nested_config = Arc::new(tokio::sync::Notify::new());
        let command_log = Arc::new(tokio::sync::Mutex::new(Vec::<(
            String,
            Option<String>,
            Option<String>,
        )>::new()));
        let server_seen = Arc::clone(&nested_config_seen);
        let server_release = Arc::clone(&release_nested_config);
        let server_log = Arc::clone(&command_log);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut write, mut read) = websocket.split();
            let mut child_frame_tree_requests = 0_u8;
            loop {
                tokio::select! {
                    trigger = attach_child_rx.recv() => {
                        if trigger.is_none() {
                            break;
                        }
                        write.send(Message::Text(
                            attached_oopif_event("page-session", "child-session", "child", "main")
                                .to_string().into(),
                        )).await.unwrap();
                    }
                    message = read.next() => {
                        let Some(message) = message else { break; };
                        match message.unwrap() {
                            Message::Text(text) => {
                                let command: Value = serde_json::from_str(&text).unwrap();
                                let id = command["id"].as_u64().unwrap();
                                let method = command["method"].as_str().unwrap().to_owned();
                                let session_id = command.get("sessionId").and_then(Value::as_str).map(str::to_owned);
                                let unique_context_id = command["params"]
                                    .get("uniqueContextId")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned);
                                server_log.lock().await.push((method.clone(), session_id.clone(), unique_context_id));

                                if method == "Emulation.setLocaleOverride"
                                    && session_id.as_deref() == Some("nested-session")
                                    && command["params"]["locale"] == "en-US"
                                {
                                    for event in [
                                        json!({
                                            "method": "Runtime.executionContextCreated",
                                            "sessionId": "child-session",
                                            "params": {"context": {
                                                "id": 7,
                                                "origin": "https://child.test",
                                                "name": "",
                                                "uniqueId": "child-old",
                                                "auxData": {"isDefault": true, "type": "default", "frameId": "child"}
                                            }}
                                        }),
                                        json!({
                                            "method": "Runtime.executionContextsCleared",
                                            "sessionId": "child-session",
                                            "params": {}
                                        }),
                                        json!({
                                            "method": "Runtime.executionContextCreated",
                                            "sessionId": "child-session",
                                            "params": {"context": {
                                                "id": 7,
                                                "origin": "https://child.test",
                                                "name": "",
                                                "uniqueId": "child-current",
                                                "auxData": {"isDefault": true, "type": "default", "frameId": "child"}
                                            }}
                                        }),
                                        json!({
                                            "method": "Runtime.executionContextDestroyed",
                                            "sessionId": "child-session",
                                            "params": {"executionContextId": 7, "executionContextUniqueId": "child-old"}
                                        }),
                                    ] {
                                        write.send(Message::Text(event.to_string().into())).await.unwrap();
                                    }
                                    server_seen.notify_one();
                                    server_release.notified().await;
                                }

                                let result = match method.as_str() {
                                    "Browser.getVersion" => crate::runtime::test_browser_version_result(),
                                    "Target.getBrowserContexts" => json!({"browserContextIds": []}),
                                    "Target.createBrowserContext" => json!({"browserContextId": "context-1"}),
                                    "Target.createTarget" => json!({"targetId": "page-target"}),
                                    "Target.attachToTarget" => json!({"sessionId": "page-session"}),
                                    "Target.closeTarget" => json!({"success": true}),
                                    "Page.navigate" => json!({"frameId": "main", "loaderId": "loader-main"}),
                                    "Runtime.evaluate" => json!({"result": {"type": "number", "value": 42}}),
                                    "Page.getFrameTree" => match session_id.as_deref() {
                                        Some("page-session") => json!({"frameTree": oopif_frame_tree("main", None, None)}),
                                        Some("child-session") => {
                                            child_frame_tree_requests += 1;
                                            if child_frame_tree_requests == 1 {
                                                json!({"frameTree": oopif_frame_tree("child", Some("main"), None)})
                                            } else {
                                                write.send(Message::Text(
                                                    attached_oopif_event(
                                                        "child-session",
                                                        "nested-session",
                                                        "nested",
                                                        "child",
                                                    ).to_string().into(),
                                                )).await.unwrap();
                                                let mut frame_tree = oopif_frame_tree(
                                                    "child",
                                                    Some("main"),
                                                    None,
                                                );
                                                frame_tree["childFrames"] = json!([
                                                    oopif_frame_tree("same", Some("child"), None),
                                                    oopif_frame_tree("nested", Some("child"), None),
                                                ]);
                                                json!({"frameTree": frame_tree})
                                            }
                                        }
                                        Some("nested-session") => json!({
                                            "frameTree": oopif_frame_tree("nested", Some("child"), None)
                                        }),
                                        other => panic!("unexpected Page.getFrameTree route: {other:?}"),
                                    },
                                    _ => json!({}),
                                };
                                let mut response = json!({"id": id, "result": result});
                                if let Some(session_id) = command.get("sessionId") {
                                    response["sessionId"] = session_id.clone();
                                }
                                write.send(Message::Text(response.to_string().into())).await.unwrap();
                                if method == "Page.navigate" {
                                    write.send(Message::Text(
                                        main_frame_navigated_event(&command["sessionId"])
                                            .to_string()
                                            .into(),
                                    )).await.unwrap();
                                }
                            }
                            Message::Ping(payload) => write.send(Message::Pong(payload)).await.unwrap(),
                            Message::Close(_) => break,
                            _ => {}
                        }
                    }
                }
            }
        });

        let runtime = BrowserRuntime::connect(format!("ws://{address}"))
            .await
            .unwrap();
        let route_options = TargetRouteOptions::default()
            .viewport(Viewport::new(800, 600).unwrap())
            .locale("en-US")
            .unwrap();
        let session = runtime
            .isolated_session(
                IsolatedSessionOptions::default()
                    .context(ContextOptions::default().target_route(route_options)),
            )
            .await
            .unwrap();
        let page = session.new_page("https://main.test/").await.unwrap();
        let store = page.frame_store().await.unwrap().clone();
        store.enable_runtime_events().await.unwrap();
        let mut events = page.subscribe_events_without_preparation_for_test();

        attach_child.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), nested_config_seen.notified())
            .await
            .expect("nested route should reach the deterministic configuration barrier");

        let blocked_frames = page.frames().await.unwrap();
        let same_session_descendant = blocked_frames
            .iter()
            .find(|frame| frame.id().as_str() == "same")
            .expect("same-session descendant should publish with its parent route");
        assert_eq!(
            same_session_descendant
                .parent()
                .await
                .unwrap()
                .unwrap()
                .id()
                .as_str(),
            "child"
        );
        assert_eq!(
            same_session_descendant.cdp_session().await.unwrap().id(),
            "child-session"
        );
        let leaked_nested = blocked_frames
            .iter()
            .find(|frame| frame.id().as_str() == "nested")
            .cloned();
        let leaked_locator_route = leaked_nested
            .as_ref()
            .and_then(|frame| store.locator_route(frame).ok())
            .map(|route| route.session_id);

        let provisional_route = {
            let mut state = store.state.write();
            let snapshot = store
                .identity
                .snapshot(&state.graph, "nested")
                .expect("post-resume tree should preserve provisional nested identity");
            let session = state
                .sessions
                .get("child-session")
                .expect("child route should already be active")
                .clone();
            // Model a waiter admitted during the historical publication bug.
            // Production handles cannot make this transition now, but an
            // already-frozen route must still wake when commit changes session.
            state.graph.set_route_active("nested", true);
            LocatorFrameRoute {
                page_generation: snapshot.page_generation,
                document_epoch: snapshot.document_epoch,
                frame_id: FrameId::new("nested"),
                session_id: "child-session".to_owned(),
                session,
                loader_id: "loader-nested".to_owned(),
            }
        };
        let waiter_store = Arc::clone(&store);
        let mut provisional_waiter =
            tokio::spawn(async move { waiter_store.main_world_context(&provisional_route).await });
        tokio::task::yield_now().await;

        release_nested_config.notify_one();

        let waiter_result =
            tokio::time::timeout(Duration::from_millis(500), &mut provisional_waiter).await;
        if waiter_result.is_err() {
            provisional_waiter.abort();
        }
        let waiter_woke_for_route_change = matches!(
            waiter_result,
            Ok(Ok(Err(error))) if error.to_string().contains("route changed")
        );

        let route_events = tokio::time::timeout(Duration::from_secs(1), async {
            let mut routes = Vec::new();
            while routes.len() < 2 {
                let fact = events.next().await.unwrap().unwrap();
                if let PageEvent::FrameRouteChanged {
                    frame_id,
                    previous_session_id,
                    session_id,
                    target_id,
                } = fact.event()
                {
                    routes.push((
                        frame_id.as_str().to_owned(),
                        previous_session_id.clone(),
                        session_id.clone(),
                        target_id.clone(),
                    ));
                }
            }
            routes
        })
        .await
        .expect("child and nested routes should each publish once");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.next())
                .await
                .is_err(),
            "route commit must not publish duplicate page events"
        );

        let active_frames = page.frames().await.unwrap();
        let child = active_frames
            .iter()
            .find(|frame| frame.id().as_str() == "child")
            .unwrap();
        let nested = active_frames
            .iter()
            .find(|frame| frame.id().as_str() == "nested")
            .unwrap();
        assert_eq!(child.parent().await.unwrap().unwrap().id().as_str(), "main");
        assert_eq!(
            nested.parent().await.unwrap().unwrap().id().as_str(),
            "child"
        );
        assert_eq!(child.cdp_session().await.unwrap().id(), "child-session");
        assert_eq!(nested.cdp_session().await.unwrap().id(), "nested-session");

        let evaluated: i64 = tokio::time::timeout(
            Duration::from_secs(1),
            child.evaluate("globalThis.childValue"),
        )
        .await
        .expect("child evaluation must not wait for a lost main-world context")
        .unwrap();
        assert_eq!(evaluated, 42);

        let log = command_log.lock().await.clone();
        assert!(log.iter().any(|entry| {
            entry.0 == "Runtime.evaluate"
                && entry.1.as_deref() == Some("child-session")
                && entry.2.as_deref() == Some("child-current")
        }));
        let position = |method: &str, session_id: &str| {
            log.iter()
                .position(|entry| entry.0 == method && entry.1.as_deref() == Some(session_id))
                .unwrap_or_else(|| panic!("missing {method} for {session_id}: {log:?}"))
        };
        for session_id in ["child-session", "nested-session"] {
            assert!(
                position("Emulation.setLocaleOverride", session_id)
                    < position("Runtime.runIfWaitingForDebugger", session_id),
                "route configuration must precede resume for {session_id}"
            );
        }
        assert!(
            log.iter().all(|entry| {
                entry.0 != "Emulation.setDeviceMetricsOverride"
                    || entry.1.as_deref() == Some("page-session")
            }),
            "viewport must not be replayed into OOPIF routes: {log:?}"
        );

        assert_eq!(
            route_events
                .iter()
                .filter(|event| event.0 == "child")
                .cloned()
                .collect::<Vec<_>>(),
            vec![(
                "child".to_owned(),
                "page-session".to_owned(),
                "child-session".to_owned(),
                Some("child".to_owned()),
            )]
        );
        assert_eq!(
            route_events
                .iter()
                .filter(|event| event.0 == "nested")
                .cloned()
                .collect::<Vec<_>>(),
            vec![(
                "nested".to_owned(),
                "child-session".to_owned(),
                "nested-session".to_owned(),
                Some("nested".to_owned()),
            )]
        );

        assert!(
            leaked_nested.is_none(),
            "nested frame leaked before route commit"
        );
        assert!(
            leaked_locator_route.is_none(),
            "locator froze provisional nested route to {leaked_locator_route:?}"
        );
        assert!(
            waiter_woke_for_route_change,
            "provisional route waiter did not wake promptly on nested commit"
        );

        assert!(runtime.close().await.is_complete());
        drop(attach_child);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn future_oopif_failure_is_terminal_and_retains_cleanup_failures() {
        use crate::runtime::{
            ActionCompletion, BrowserRuntime, ContextOptions, EventStreamCloseReason,
            IsolatedSessionOptions, OperationPhase, TargetRouteOptions,
        };

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (attach, mut attach_rx) =
            tokio::sync::mpsc::unbounded_channel::<(&'static str, &'static str)>();
        let methods = Arc::new(tokio::sync::Mutex::new(
            Vec::<(String, Option<String>)>::new(),
        ));
        let close_started = Arc::new(tokio::sync::Notify::new());
        let release_close = Arc::new(tokio::sync::Notify::new());
        let server_methods = Arc::clone(&methods);
        let server_close_started = Arc::clone(&close_started);
        let server_release_close = Arc::clone(&release_close);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut write, mut read) = websocket.split();
            loop {
                tokio::select! {
                    trigger = attach_rx.recv() => {
                        let Some((session_id, frame_id)) = trigger else { break; };
                        write.send(Message::Text(
                            attached_oopif_event("page-session", session_id, frame_id, "main")
                                .to_string().into(),
                        )).await.unwrap();
                    }
                    message = read.next() => {
                        let Some(message) = message else { break; };
                        let Message::Text(text) = message.unwrap() else { continue; };
                        let command: Value = serde_json::from_str(&text).unwrap();
                        let id = command["id"].as_u64().unwrap();
                        let method = command["method"].as_str().unwrap().to_owned();
                        let session_id = command.get("sessionId").and_then(Value::as_str).map(str::to_owned);
                        server_methods.lock().await.push((method.clone(), session_id.clone()));

                        let child = session_id.as_deref().is_some_and(|id| id.starts_with("child-session"));
                        let fail = (method == "Emulation.setLocaleOverride" && child)
                            || (method == "Target.detachFromTarget"
                                && command["params"]["sessionId"]
                                    .as_str()
                                    .is_some_and(|id| id.starts_with("child-session")));
                        let mut response = if fail {
                            json!({"id": id, "error": {"code": -32000, "message": format!("forced {method} failure")}})
                        } else {
                            let result = match method.as_str() {
                                "Browser.getVersion" => crate::runtime::test_browser_version_result(),
                                "Target.getBrowserContexts" => json!({"browserContextIds": []}),
                                "Target.createBrowserContext" => json!({"browserContextId": "context-1"}),
                                "Target.createTarget" => json!({"targetId": "page-target"}),
                                "Target.attachToTarget" => json!({"sessionId": "page-session"}),
                                "Target.closeTarget" => json!({"success": true}),
                                "Page.navigate" => json!({"frameId": "main", "loaderId": "loader-main"}),
                                "Page.getFrameTree" => match session_id.as_deref() {
                                    Some("page-session") => json!({"frameTree": oopif_frame_tree("main", None, None)}),
                                    Some(route) if route.starts_with("child-session") => {
                                        let frame_id = route.replace("session", "frame");
                                        json!({"frameTree": oopif_frame_tree(&frame_id, Some("main"), None)})
                                    }
                                    other => panic!("unexpected Page.getFrameTree route: {other:?}"),
                                },
                                _ => json!({}),
                            };
                            json!({"id": id, "result": result})
                        };
                        if let Some(session_id) = command.get("sessionId") {
                            response["sessionId"] = session_id.clone();
                        }
                        if method == "Target.disposeBrowserContext" {
                            server_close_started.notify_one();
                            server_release_close.notified().await;
                        }
                        write.send(Message::Text(response.to_string().into())).await.unwrap();
                        if method == "Page.navigate" {
                            write.send(Message::Text(
                                main_frame_navigated_event(&command["sessionId"])
                                    .to_string()
                                    .into(),
                            )).await.unwrap();
                        }
                    }
                }
            }
        });

        let runtime = BrowserRuntime::connect(format!("ws://{address}"))
            .await
            .unwrap();
        let session = runtime
            .isolated_session(
                IsolatedSessionOptions::default().context(
                    ContextOptions::default()
                        .target_route(TargetRouteOptions::default().locale("en-US").unwrap()),
                ),
            )
            .await
            .unwrap();
        let page = session.new_page("https://main.test/").await.unwrap();
        let main_frame = page.main_frame().await.unwrap();
        let store = page.frame_store().await.unwrap().clone();
        let mut events = page.subscribe_events_without_preparation_for_test();

        attach.send(("child-session-1", "child-frame-1")).unwrap();
        let stream_error = tokio::time::timeout(Duration::from_secs(1), events.next())
            .await
            .expect("future route failure should close page events")
            .unwrap()
            .unwrap_err();
        assert_eq!(stream_error.reason(), EventStreamCloseReason::RouteFailed);
        let event_error = stream_error.browser_error().unwrap();
        let mut late_events = page.subscribe_events().await.unwrap();
        let late_error = late_events.next().await.unwrap().unwrap_err();
        assert_eq!(late_error.reason(), EventStreamCloseReason::RouteFailed);
        assert_eq!(
            late_error.browser_error().unwrap().route_failure(),
            event_error.route_failure()
        );
        assert!(late_events.next().await.is_none());
        let route = event_error.route_failure().unwrap();
        assert_eq!(route.frame_id().as_str(), "child-frame-1");
        assert_eq!(route.target_id(), "child-frame-1");
        assert_eq!(route.session_id(), "child-session-1");
        assert_eq!(
            event_error.operation_name(),
            Some("Emulation.setLocaleOverride")
        );
        assert_eq!(event_error.phase(), OperationPhase::Dispatch);
        assert_eq!(event_error.action_completed(), ActionCompletion::NotStarted);
        assert!(store.handle("child-frame-1").is_none());

        let before_rejected_operations = methods.lock().await.len();
        let click_error = page.click_at(1.0, 1.0).await.unwrap_err();
        let evaluate_error = main_frame
            .evaluate::<serde_json::Value>("globalThis.value")
            .await
            .unwrap_err();
        assert_eq!(methods.lock().await.len(), before_rejected_operations);
        for rejected in [&click_error, &evaluate_error] {
            assert_eq!(rejected.route_failure(), event_error.route_failure());
            assert_eq!(rejected.operation_name(), event_error.operation_name());
            assert_eq!(rejected.phase(), event_error.phase());
            assert_eq!(rejected.action_completed(), event_error.action_completed());
        }
        let queried = page.terminal_route_error().unwrap();
        assert_eq!(queried.route_failure(), event_error.route_failure());

        attach.send(("child-session-2", "child-frame-2")).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let detach_count = methods
                    .lock()
                    .await
                    .iter()
                    .filter(|(method, _)| method == "Target.detachFromTarget")
                    .count();
                if detach_count == 2 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("both failed routes should finish detach cleanup");
        assert_eq!(
            page.terminal_route_error()
                .unwrap()
                .route_failure()
                .unwrap()
                .session_id(),
            "child-session-1"
        );

        let cancelled_session = session.clone();
        let cancelled_close = tokio::spawn(async move { cancelled_session.close().await });
        tokio::time::timeout(Duration::from_secs(1), close_started.notified())
            .await
            .expect("session close should reach its page target cleanup");
        cancelled_close.abort();
        assert!(cancelled_close.await.unwrap_err().is_cancelled());

        let first_session = session.clone();
        let second_session = session.clone();
        let first_close = tokio::spawn(async move { first_session.close().await });
        let second_close = tokio::spawn(async move { second_session.close().await });
        release_close.notify_one();
        let session_report = first_close.await.unwrap();
        assert_eq!(second_close.await.unwrap(), session_report);
        let page_report = page.close().await;
        for resource in [
            "route:child-session-1",
            "oopif-route:child-session-1",
            "auto-attached-target-detach:child-session-2",
        ] {
            assert_eq!(
                page_report
                    .failures()
                    .iter()
                    .filter(|failure| failure.resource() == resource)
                    .count(),
                1,
                "cleanup failure {resource} must be retained exactly once: {page_report:?}"
            );
        }
        let runtime_report = runtime.close().await;
        for report in [&session_report, &runtime_report] {
            for resource in [
                "route:child-session-1",
                "oopif-route:child-session-1",
                "auto-attached-target-detach:child-session-2",
            ] {
                assert_eq!(
                    report
                        .failures()
                        .iter()
                        .filter(|failure| failure.resource() == resource)
                        .count(),
                    1,
                    "root report lost or duplicated {resource}: {report:?}"
                );
            }
        }
        drop(attach);
        server.await.unwrap();
    }

    struct AttachedTargetFixture {
        url: String,
        events: tokio::sync::mpsc::UnboundedSender<Value>,
        commands: Arc<tokio::sync::Mutex<Vec<Value>>>,
        server: tokio::task::JoinHandle<()>,
    }

    async fn start_attached_target_fixture(
        initial: Option<Value>,
        fail_resume: bool,
        fail_detach: bool,
        detach_barrier: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
    ) -> AttachedTargetFixture {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (events, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let commands = Arc::new(tokio::sync::Mutex::new(Vec::<Value>::new()));
        let server_commands = Arc::clone(&commands);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut write, mut read) = websocket.split();
            let mut initial = initial;
            loop {
                tokio::select! {
                    event = event_rx.recv() => {
                        let Some(event) = event else { break; };
                        write.send(Message::Text(event.to_string().into())).await.unwrap();
                    }
                    message = read.next() => {
                        let Some(message) = message else { break; };
                        let Message::Text(text) = message.unwrap() else { continue; };
                        let command: Value = serde_json::from_str(&text).unwrap();
                        server_commands.lock().await.push(command.clone());
                        let id = command["id"].as_u64().unwrap();
                        let method = command["method"].as_str().unwrap();
                        let routed_session = command.get("sessionId").and_then(Value::as_str);
                        if method == "Target.setAutoAttach"
                            && routed_session == Some("page-session")
                        {
                            if let Some(event) = initial.take() {
                                write.send(Message::Text(event.to_string().into())).await.unwrap();
                            }
                        }
                        let worker_session = routed_session == Some("worker-session")
                            || command["params"]["sessionId"] == "worker-session";
                        let fails = (fail_resume
                            && method == "Runtime.runIfWaitingForDebugger"
                            && worker_session)
                            || (fail_detach
                                && method == "Target.detachFromTarget"
                                && worker_session)
                            || (method == "Network.setUserAgentOverride"
                                && routed_session == Some("failing-config-worker"));
                        if method == "Target.detachFromTarget" && worker_session {
                            if let Some((started, release)) = detach_barrier.as_ref() {
                                started.notify_one();
                                release.notified().await;
                            }
                        }
                        let mut response = if fails {
                            json!({"id": id, "error": {"code": -32000, "message": format!("forced {method} failure")}})
                        } else {
                            let result = match method {
                                "Browser.getVersion" => crate::runtime::test_browser_version_result(),
                                "Target.getBrowserContexts" => json!({"browserContextIds": []}),
                                "Target.createBrowserContext" => json!({"browserContextId": "context-1"}),
                                "Target.createTarget" => json!({"targetId": "page-target"}),
                                "Target.attachToTarget" => json!({"sessionId": "page-session"}),
                                "Target.closeTarget" => json!({"success": true}),
                                "Page.navigate" => json!({"frameId": "main", "loaderId": "loader-main"}),
                                "Page.getFrameTree" => match routed_session {
                                    Some("iframe-session") => json!({"frameTree": oopif_frame_tree("iframe-target", Some("main"), None)}),
                                    _ => json!({"frameTree": oopif_frame_tree("main", None, None)}),
                                },
                                _ => json!({}),
                            };
                            json!({"id": id, "result": result})
                        };
                        if let Some(session_id) = command.get("sessionId") {
                            response["sessionId"] = session_id.clone();
                        }
                        write.send(Message::Text(response.to_string().into())).await.unwrap();
                        if method == "Page.navigate" && !fails {
                            write.send(Message::Text(
                                main_frame_navigated_event(&command["sessionId"])
                                    .to_string()
                                    .into(),
                            )).await.unwrap();
                        }
                        if method == "Target.closeTarget" && !fails {
                            write
                                .send(Message::Text(
                                    json!({
                                        "method": "Target.targetDestroyed",
                                        "params": {"targetId": "page-target"}
                                    })
                                    .to_string()
                                    .into(),
                                ))
                                .await
                                .unwrap();
                        }
                    }
                }
            }
        });
        AttachedTargetFixture {
            url: format!("ws://{address}"),
            events,
            commands,
            server,
        }
    }

    fn attached_target_event(
        parent_session_id: &str,
        session_id: Option<&str>,
        target_id: Option<&str>,
        target_type: Option<&str>,
        waiting_for_debugger: bool,
        complete: bool,
    ) -> Value {
        let mut target_info = json!({});
        if let Some(target_id) = target_id {
            target_info["targetId"] = json!(target_id);
        }
        if let Some(target_type) = target_type {
            target_info["type"] = json!(target_type);
        }
        if complete {
            target_info["title"] = json!("");
            target_info["url"] = json!(format!("https://{}.test/", target_id.unwrap_or("target")));
            target_info["attached"] = json!(true);
            target_info["canAccessOpener"] = json!(false);
            if target_type == Some("iframe") {
                target_info["parentFrameId"] = json!("main");
            }
        }
        let mut params = json!({
            "targetInfo": target_info,
            "waitingForDebugger": waiting_for_debugger,
        });
        if let Some(session_id) = session_id {
            params["sessionId"] = json!(session_id);
        }
        json!({
            "method": "Target.attachedToTarget",
            "sessionId": parent_session_id,
            "params": params,
        })
    }

    async fn wait_for_command_count(
        commands: &tokio::sync::Mutex<Vec<Value>>,
        method: &str,
        count: usize,
    ) {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if commands
                    .lock()
                    .await
                    .iter()
                    .filter(|command| command["method"] == method)
                    .count()
                    >= count
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("missing {count} {method} commands"));
    }

    async fn fixture_page(
        fixture: &AttachedTargetFixture,
    ) -> (crate::runtime::BrowserRuntime, Page) {
        let runtime = crate::runtime::BrowserRuntime::connect(fixture.url.clone())
            .await
            .unwrap();
        let session = runtime.default_session().await.unwrap();
        let page = session.new_page("https://main.test/").await.unwrap();
        (runtime, page)
    }

    fn assert_salvaged_target_failure(
        failure: &crate::runtime::TargetFailure,
        expected_target_id: Option<&str>,
        expected_session_id: Option<&str>,
        expected_target_url: Option<&str>,
        expected_field_errors: &[&str],
    ) {
        assert_eq!(failure.target_id(), expected_target_id);
        assert_eq!(failure.session_id(), expected_session_id);
        assert_eq!(failure.target_url(), expected_target_url);
        assert!(failure.event_error().is_some());
        assert_eq!(
            failure
                .field_errors()
                .iter()
                .map(crate::runtime::TargetFieldError::field)
                .collect::<Vec<_>>(),
            expected_field_errors
        );
    }

    #[allow(clippy::too_many_arguments)]
    async fn assert_malformed_attached_target_salvage(
        initial: bool,
        event: Value,
        expected_target_id: Option<&str>,
        expected_session_id: Option<&str>,
        expected_target_url: Option<&str>,
        expected_field_errors: &[&str],
        expected_resume_session: Option<&str>,
        expected_detach_session: Option<&str>,
        expected_detach_target: Option<&str>,
    ) {
        let fixture =
            start_attached_target_fixture(initial.then(|| event.clone()), false, false, None).await;
        let runtime = crate::runtime::BrowserRuntime::connect(fixture.url.clone())
            .await
            .unwrap();
        let session = runtime.default_session().await.unwrap();
        let page = if initial {
            let error = session.new_page("https://main.test/").await.unwrap_err();
            let failure = error.target_failure().expect("typed target failure");
            assert_salvaged_target_failure(
                failure,
                expected_target_id,
                expected_session_id,
                expected_target_url,
                expected_field_errors,
            );
            None
        } else {
            let page = session.new_page("https://main.test/").await.unwrap();
            fixture.events.send(event).unwrap();
            tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    if page.terminal_route_error().is_some() {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("malformed future attachment should be terminal");
            let error = page.terminal_route_error().unwrap();
            let failure = error.target_failure().expect("typed target failure");
            assert_salvaged_target_failure(
                failure,
                expected_target_id,
                expected_session_id,
                expected_target_url,
                expected_field_errors,
            );
            Some(page)
        };

        let commands = fixture.commands.lock().await.clone();
        let resume = commands
            .iter()
            .filter(|command| command["method"] == "Runtime.runIfWaitingForDebugger")
            .collect::<Vec<_>>();
        let detach = commands
            .iter()
            .filter(|command| command["method"] == "Target.detachFromTarget")
            .collect::<Vec<_>>();
        assert_eq!(resume.len(), usize::from(expected_resume_session.is_some()));
        if let Some(session_id) = expected_resume_session {
            assert_eq!(resume[0]["sessionId"], session_id);
        }
        assert_eq!(
            detach.len(),
            usize::from(expected_detach_session.is_some() || expected_detach_target.is_some()),
            "salvage detach must execute exactly once: {commands:?}"
        );
        if let Some(session_id) = expected_detach_session {
            assert_eq!(detach[0]["params"]["sessionId"], session_id);
        }
        if let Some(target_id) = expected_detach_target {
            assert_eq!(detach[0]["params"]["targetId"], target_id);
        }

        if let Some(page) = page {
            let _ = page.close().await;
        }
        let _ = runtime.close().await;
        drop(fixture.events);
        fixture.server.await.unwrap();
    }

    #[tokio::test]
    async fn malformed_attached_target_fields_salvage_independently_initial_and_future() {
        for initial in [true, false] {
            let mut waiting_invalid = attached_target_event(
                "page-session",
                Some("worker-session"),
                Some("worker-target"),
                Some("worker"),
                true,
                true,
            );
            waiting_invalid["params"]["waitingForDebugger"] = json!("invalid");
            assert_malformed_attached_target_salvage(
                initial,
                waiting_invalid,
                Some("worker-target"),
                Some("worker-session"),
                Some("https://worker-target.test/"),
                &["waitingForDebugger"],
                Some("worker-session"),
                Some("worker-session"),
                None,
            )
            .await;

            let mut waiting_missing = attached_target_event(
                "page-session",
                Some("worker-session"),
                Some("worker-target"),
                Some("worker"),
                true,
                true,
            );
            waiting_missing["params"]
                .as_object_mut()
                .unwrap()
                .remove("waitingForDebugger");
            assert_malformed_attached_target_salvage(
                initial,
                waiting_missing,
                Some("worker-target"),
                Some("worker-session"),
                Some("https://worker-target.test/"),
                &["waitingForDebugger"],
                Some("worker-session"),
                Some("worker-session"),
                None,
            )
            .await;

            let mut reserved_invalid = attached_target_event(
                "page-session",
                Some("worker-session"),
                Some("worker-target"),
                Some("worker"),
                true,
                true,
            );
            reserved_invalid["params"]["targetInfo"]["attached"] = json!("invalid");
            assert_malformed_attached_target_salvage(
                initial,
                reserved_invalid,
                Some("worker-target"),
                Some("worker-session"),
                Some("https://worker-target.test/"),
                &[],
                Some("worker-session"),
                Some("worker-session"),
                None,
            )
            .await;

            let mut session_invalid = attached_target_event(
                "page-session",
                Some("worker-session"),
                Some("worker-target"),
                Some("worker"),
                true,
                true,
            );
            session_invalid["params"]["sessionId"] = json!(7);
            assert_malformed_attached_target_salvage(
                initial,
                session_invalid,
                Some("worker-target"),
                None,
                Some("https://worker-target.test/"),
                &["sessionId"],
                None,
                None,
                Some("worker-target"),
            )
            .await;

            let mut all_invalid = attached_target_event(
                "page-session",
                Some("worker-session"),
                Some("worker-target"),
                Some("worker"),
                true,
                true,
            );
            all_invalid["params"]["sessionId"] = json!([]);
            all_invalid["params"]["targetInfo"]["targetId"] = json!({});
            all_invalid["params"]["targetInfo"]["type"] = json!(false);
            all_invalid["params"]["targetInfo"]["url"] = json!(3);
            all_invalid["params"]["waitingForDebugger"] = Value::Null;
            assert_malformed_attached_target_salvage(
                initial,
                all_invalid,
                None,
                None,
                None,
                &[
                    "sessionId",
                    "targetInfo.targetId",
                    "targetInfo.type",
                    "targetInfo.url",
                    "waitingForDebugger",
                ],
                None,
                None,
                None,
            )
            .await;
        }
    }

    #[tokio::test]
    async fn malformed_initial_target_is_salvaged_and_fails_page_creation() {
        let initial = attached_target_event(
            "page-session",
            Some("worker-session"),
            Some("worker-target"),
            Some("worker"),
            true,
            false,
        );
        let fixture = start_attached_target_fixture(Some(initial), false, false, None).await;
        let runtime = crate::runtime::BrowserRuntime::connect(fixture.url.clone())
            .await
            .unwrap();
        let session = runtime.default_session().await.unwrap();

        let error = session.new_page("https://main.test/").await.unwrap_err();
        let target = error.target_failure().expect("structured target failure");
        assert_eq!(target.target_id(), Some("worker-target"));
        assert_eq!(target.session_id(), Some("worker-session"));
        let commands = fixture.commands.lock().await.clone();
        let resume = commands
            .iter()
            .position(|command| command["method"] == "Runtime.runIfWaitingForDebugger")
            .expect("salvage resume");
        let detach = commands
            .iter()
            .position(|command| command["method"] == "Target.detachFromTarget")
            .expect("salvage detach");
        assert!(resume < detach);
        assert_eq!(commands[resume]["sessionId"], "worker-session");
        assert_eq!(commands[detach]["sessionId"], "page-session");
        assert!(commands
            .iter()
            .any(|command| command["method"] == "Target.closeTarget"));

        let _ = runtime.close().await;
        fixture.server.await.unwrap();
    }

    #[tokio::test]
    async fn observed_dedicated_worker_is_configured_before_resume_and_retained_until_detached() {
        use crate::runtime::{
            ContextOptions, Geolocation, HttpHeaders, IsolatedSessionOptions, NetworkEvent,
            TargetRouteOptions, UserAgentOverride, Viewport,
        };

        let fixture = start_attached_target_fixture(None, false, false, None).await;
        let runtime = crate::runtime::BrowserRuntime::connect(fixture.url.clone())
            .await
            .unwrap();
        let route = TargetRouteOptions::default()
            .viewport(Viewport::new(800, 600).unwrap())
            .locale("en-US")
            .unwrap()
            .timezone("UTC")
            .unwrap()
            .geolocation(Geolocation::new(1.0, 2.0).unwrap())
            .user_agent(UserAgentOverride::new("Worker UA").unwrap())
            .http_headers(HttpHeaders::new([("x-worker-test", "enabled")]).unwrap());
        let session = runtime
            .isolated_session(
                IsolatedSessionOptions::default().context(
                    ContextOptions::default()
                        .target_route(route)
                        .ignore_https_errors(true),
                ),
            )
            .await
            .unwrap();
        let page = session.new_page("https://main.test/").await.unwrap();
        let mut network = page.subscribe_network_events().await.unwrap();

        fixture
            .events
            .send(attached_target_event(
                "page-session",
                Some("worker-session"),
                Some("worker-target"),
                Some("worker"),
                true,
                true,
            ))
            .unwrap();
        wait_for_command_count(&fixture.commands, "Runtime.runIfWaitingForDebugger", 1).await;

        let commands = fixture.commands.lock().await.clone();
        let worker_position = |method: &str| {
            commands.iter().position(|command| {
                command["method"] == method && command["sessionId"] == "worker-session"
            })
        };
        assert!(
            worker_position("Network.enable").unwrap()
                < worker_position("Network.setUserAgentOverride").unwrap()
        );
        assert!(
            worker_position("Network.setUserAgentOverride").unwrap()
                < worker_position("Network.setExtraHTTPHeaders").unwrap()
        );
        assert!(
            worker_position("Network.setExtraHTTPHeaders").unwrap()
                < worker_position("Runtime.runIfWaitingForDebugger").unwrap()
        );
        assert!(worker_position("Target.detachFromTarget").is_none());
        assert!(commands.iter().all(|command| {
            command["sessionId"] != "worker-session"
                || !(command["method"].as_str().is_some_and(|method| {
                    method.starts_with("Emulation.") || method.starts_with("Security.")
                }))
        }));

        fixture
            .events
            .send(json!({
                "method": "Network.requestWillBeSent",
                "sessionId": "worker-session",
                "params": {
                    "requestId": "worker-request",
                    "loaderId": "",
                    "documentURL": "https://worker.test/",
                    "request": {"url": "https://worker.test/data", "method": "GET", "headers": {}},
                    "timestamp": 1.0,
                    "wallTime": 1.0,
                    "initiator": {"type": "script"},
                    "type": "Fetch"
                }
            }))
            .unwrap();
        let request = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let event = network.next().await.unwrap().unwrap();
                if matches!(event.event(), NetworkEvent::RequestStarted(_)) {
                    break event;
                }
            }
        })
        .await
        .expect("worker network route should publish requests");
        assert_eq!(
            request.metadata().identity().routed_session_id(),
            Some("worker-session")
        );
        assert!(page
            .frames()
            .await
            .unwrap()
            .iter()
            .all(|frame| frame.id().as_str() != "worker-target"));

        fixture
            .events
            .send(json!({
                "method": "Target.detachedFromTarget",
                "params": {"sessionId": "worker-session", "targetId": "worker-target"}
            }))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let event = network.next().await.unwrap().unwrap();
                if matches!(event.event(), NetworkEvent::RouteClosed { routed_session_id, .. } if routed_session_id == "worker-session") {
                    break;
                }
            }
        })
        .await
        .expect("natural worker detach should close its network route");

        let report = page.close().await;
        assert!(report.is_complete(), "page cleanup failed: {report:?}");
        let commands = fixture.commands.lock().await.clone();
        assert_eq!(
            commands
                .iter()
                .filter(|command| command["method"] == "Target.detachFromTarget"
                    && command["params"]["sessionId"] == "worker-session")
                .count(),
            0,
            "natural detach must disarm Page-owned worker detach"
        );
        assert!(commands
            .iter()
            .all(|command| command["method"] != "Target.closeTarget"
                || command["params"]["targetId"] != "worker-target"));

        assert!(runtime.close().await.is_complete());
        drop(fixture.events);
        fixture.server.await.unwrap();
    }

    #[tokio::test]
    async fn auxiliary_configuration_failure_resumes_detaches_and_is_terminal() {
        use crate::runtime::{
            ContextOptions, IsolatedSessionOptions, TargetRouteOptions, UserAgentOverride,
        };

        let fixture = start_attached_target_fixture(None, false, false, None).await;
        let runtime = crate::runtime::BrowserRuntime::connect(fixture.url.clone())
            .await
            .unwrap();
        let session = runtime
            .isolated_session(
                IsolatedSessionOptions::default().context(
                    ContextOptions::default().target_route(
                        TargetRouteOptions::default()
                            .user_agent(UserAgentOverride::new("Worker UA").unwrap()),
                    ),
                ),
            )
            .await
            .unwrap();
        let page = session.new_page("https://main.test/").await.unwrap();
        fixture
            .events
            .send(attached_target_event(
                "page-session",
                Some("failing-config-worker"),
                Some("failing-config-target"),
                Some("worker"),
                true,
                true,
            ))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if page.terminal_route_error().is_some() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("worker configuration failure should be terminal");

        let error = page.terminal_route_error().unwrap();
        assert_eq!(error.operation_name(), Some("Network.setUserAgentOverride"));
        assert_eq!(
            error.target_failure().unwrap().session_id(),
            Some("failing-config-worker")
        );
        let commands = fixture.commands.lock().await.clone();
        let position = |method: &str| {
            commands
                .iter()
                .position(|command| {
                    command["method"] == method
                        && (command["sessionId"] == "failing-config-worker"
                            || command["params"]["sessionId"] == "failing-config-worker")
                })
                .unwrap_or_else(|| panic!("missing {method}: {commands:?}"))
        };
        assert!(position("Network.enable") < position("Network.setUserAgentOverride"));
        assert!(
            position("Network.setUserAgentOverride") < position("Runtime.runIfWaitingForDebugger")
        );
        assert!(position("Runtime.runIfWaitingForDebugger") < position("Target.detachFromTarget"));
        assert!(commands.iter().all(|command| {
            command["method"] != "Target.closeTarget"
                || command["params"]["targetId"] != "failing-config-target"
        }));

        let _ = page.close().await;
        let _ = runtime.close().await;
        drop(fixture.events);
        fixture.server.await.unwrap();
    }

    #[tokio::test]
    async fn nested_worker_keeps_direct_parent_without_entering_frame_graph() {
        let fixture = start_attached_target_fixture(None, false, false, None).await;
        let (runtime, page) = fixture_page(&fixture).await;
        let _network = page.subscribe_network_events().await.unwrap();
        let store = page.frame_store().await.unwrap().clone();

        fixture
            .events
            .send(attached_target_event(
                "page-session",
                Some("worker-session"),
                Some("worker-target"),
                Some("worker"),
                true,
                true,
            ))
            .unwrap();
        wait_for_command_count(&fixture.commands, "Runtime.runIfWaitingForDebugger", 1).await;
        fixture
            .events
            .send(attached_target_event(
                "worker-session",
                Some("nested-worker-session"),
                Some("nested-worker-target"),
                Some("worker"),
                true,
                true,
            ))
            .unwrap();
        wait_for_command_count(&fixture.commands, "Runtime.runIfWaitingForDebugger", 2).await;

        assert_eq!(
            store
                .auxiliary_targets
                .direct_parent_session_id("nested-worker-session")
                .as_deref(),
            Some("worker-session")
        );
        assert!(page.frames().await.unwrap().iter().all(|frame| {
            !matches!(
                frame.id().as_str(),
                "worker-target" | "nested-worker-target"
            )
        }));

        for (session_id, target_id) in [
            ("nested-worker-session", "nested-worker-target"),
            ("worker-session", "worker-target"),
        ] {
            fixture
                .events
                .send(json!({
                    "method": "Target.detachedFromTarget",
                    "params": {"sessionId": session_id, "targetId": target_id}
                }))
                .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            while store
                .auxiliary_targets
                .direct_parent_session_id("worker-session")
                .is_some()
                || store
                    .auxiliary_targets
                    .direct_parent_session_id("nested-worker-session")
                    .is_some()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("natural detach should drain the nested worker family");

        assert!(page.close().await.is_complete());
        assert!(runtime.close().await.is_complete());
        drop(fixture.events);
        fixture.server.await.unwrap();
    }

    #[tokio::test]
    async fn future_workers_resume_then_detach_without_publishing_frame_routes() {
        let fixture = start_attached_target_fixture(None, false, false, None).await;
        let (runtime, page) = fixture_page(&fixture).await;
        fixture
            .events
            .send(attached_target_event(
                "page-session",
                Some("paused-worker"),
                Some("paused-target"),
                Some("worker"),
                true,
                true,
            ))
            .unwrap();
        wait_for_command_count(&fixture.commands, "Target.detachFromTarget", 1).await;
        fixture
            .events
            .send(attached_target_event(
                "page-session",
                Some("running-worker"),
                Some("running-target"),
                Some("service_worker"),
                false,
                true,
            ))
            .unwrap();
        wait_for_command_count(&fixture.commands, "Target.detachFromTarget", 2).await;
        fixture
            .events
            .send(attached_target_event(
                "page-session",
                Some("shared-worker"),
                Some("shared-target"),
                Some("shared_worker"),
                true,
                true,
            ))
            .unwrap();
        wait_for_command_count(&fixture.commands, "Target.detachFromTarget", 3).await;
        fixture
            .events
            .send(attached_target_event(
                "page-session",
                Some("iframe-session"),
                Some("iframe-target"),
                Some("iframe"),
                true,
                true,
            ))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if page
                    .frames()
                    .await
                    .unwrap()
                    .iter()
                    .any(|frame| frame.id().as_str() == "iframe-target")
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("iframe route should initialize");

        assert!(page.terminal_route_error().is_none());
        let commands = fixture.commands.lock().await.clone();
        let position = |method: &str, session: &str| {
            commands.iter().position(|command| {
                command["method"] == method
                    && (command["sessionId"] == session
                        || command["params"]["sessionId"] == session)
            })
        };
        assert!(
            position("Runtime.runIfWaitingForDebugger", "paused-worker").unwrap()
                < position("Target.detachFromTarget", "paused-worker").unwrap()
        );
        assert!(position("Runtime.runIfWaitingForDebugger", "running-worker").is_none());
        assert!(position("Target.detachFromTarget", "running-worker").is_some());
        assert!(
            position("Runtime.runIfWaitingForDebugger", "shared-worker").unwrap()
                < position("Target.detachFromTarget", "shared-worker").unwrap()
        );
        assert!(position("Target.detachFromTarget", "iframe-session").is_none());

        assert!(runtime.close().await.is_complete());
        drop(fixture.events);
        fixture.server.await.unwrap();
    }

    #[tokio::test]
    async fn malformed_future_target_salvages_then_fails_closed() {
        let fixture = start_attached_target_fixture(None, false, false, None).await;
        let (runtime, page) = fixture_page(&fixture).await;
        let mut events = page.subscribe_events_without_preparation_for_test();
        fixture
            .events
            .send(attached_target_event(
                "page-session",
                Some("worker-session"),
                Some("worker-target"),
                Some("worker"),
                true,
                false,
            ))
            .unwrap();

        let stream_error = tokio::time::timeout(Duration::from_secs(1), events.next())
            .await
            .expect("malformed target should terminate events")
            .unwrap()
            .unwrap_err();
        let error = stream_error.browser_error().unwrap();
        assert_eq!(
            error.target_failure().unwrap().target_id(),
            Some("worker-target")
        );
        wait_for_command_count(&fixture.commands, "Target.detachFromTarget", 1).await;
        fixture
            .events
            .send(attached_target_event(
                "page-session",
                Some("late-worker"),
                Some("late-target"),
                Some("worker"),
                true,
                true,
            ))
            .unwrap();
        wait_for_command_count(&fixture.commands, "Target.detachFromTarget", 2).await;
        assert_eq!(
            fixture
                .commands
                .lock()
                .await
                .iter()
                .filter(|command| command["method"] == "Target.detachFromTarget")
                .count(),
            2,
            "terminal pages must salvage later targets without reopening frame routing"
        );
        let rejected = page.frames().await.unwrap_err();
        assert_eq!(
            rejected.target_failure().unwrap().target_id(),
            Some("worker-target")
        );

        let _ = page.close().await;
        let _ = runtime.close().await;
        drop(fixture.events);
        fixture.server.await.unwrap();
    }

    #[tokio::test]
    async fn malformed_future_without_identity_is_terminal_without_salvage_commands() {
        let fixture = start_attached_target_fixture(None, false, false, None).await;
        let (runtime, page) = fixture_page(&fixture).await;
        fixture
            .events
            .send(attached_target_event(
                "page-session",
                None,
                None,
                Some("worker"),
                true,
                false,
            ))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if page.terminal_route_error().is_some() {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("identity-less malformed target should be terminal");
        let error = page.terminal_route_error().unwrap();
        assert_eq!(error.target_failure().unwrap().target_id(), None);
        assert_eq!(error.target_failure().unwrap().session_id(), None);
        let commands = fixture.commands.lock().await.clone();
        assert!(!commands
            .iter()
            .any(|command| command["method"] == "Runtime.runIfWaitingForDebugger"));
        assert!(!commands
            .iter()
            .any(|command| command["method"] == "Target.detachFromTarget"));

        let _ = runtime.close().await;
        drop(fixture.events);
        fixture.server.await.unwrap();
    }

    #[tokio::test]
    async fn malformed_target_cleanup_failures_reach_close_report_exactly_once() {
        let fixture = start_attached_target_fixture(None, true, true, None).await;
        let (runtime, page) = fixture_page(&fixture).await;
        let mut event = attached_target_event(
            "page-session",
            Some("worker-session"),
            Some("worker-target"),
            Some("worker"),
            true,
            true,
        );
        event["params"]["waitingForDebugger"] = json!("invalid");
        fixture.events.send(event).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if page.terminal_route_error().is_some() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("malformed cleanup failure should become terminal");

        let first = page.clone();
        let second = page.clone();
        let (first_report, second_report) = tokio::join!(first.close(), second.close());
        assert_eq!(first_report, second_report);
        for resource in [
            "auto-attached-target-resume:worker-session",
            "auto-attached-target-detach:worker-session",
        ] {
            assert_eq!(
                first_report
                    .failures()
                    .iter()
                    .filter(|failure| failure.resource() == resource)
                    .count(),
                1,
                "{resource} must reach CloseReport exactly once: {first_report:?}"
            );
        }
        let commands = fixture.commands.lock().await.clone();
        assert_eq!(
            commands
                .iter()
                .filter(|command| command["method"] == "Runtime.runIfWaitingForDebugger")
                .count(),
            1
        );
        assert_eq!(
            commands
                .iter()
                .filter(|command| command["method"] == "Target.detachFromTarget")
                .count(),
            1
        );

        let _ = runtime.close().await;
        drop(fixture.events);
        fixture.server.await.unwrap();
    }

    #[tokio::test]
    async fn worker_cleanup_failures_are_terminal_and_retained_exactly_once() {
        let fixture = start_attached_target_fixture(None, true, true, None).await;
        let (runtime, page) = fixture_page(&fixture).await;
        fixture
            .events
            .send(attached_target_event(
                "page-session",
                Some("worker-session"),
                Some("worker-target"),
                Some("shared_worker"),
                true,
                true,
            ))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if page.terminal_route_error().is_some() {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("worker cleanup failure should become terminal");

        let first = page.clone();
        let second = page.clone();
        let (first_report, second_report) = tokio::join!(first.close(), second.close());
        assert_eq!(first_report, second_report);
        for resource in [
            "auxiliary-target-resume:worker-session",
            "auxiliary-target-detach:worker-session",
        ] {
            assert_eq!(
                first_report
                    .failures()
                    .iter()
                    .filter(|failure| failure.resource() == resource)
                    .count(),
                1,
                "{resource} must be reported exactly once: {first_report:?}"
            );
        }
        let commands = fixture.commands.lock().await.clone();
        assert_eq!(
            commands
                .iter()
                .filter(|command| command["method"] == "Runtime.runIfWaitingForDebugger")
                .count(),
            1
        );
        assert_eq!(
            commands
                .iter()
                .filter(|command| command["method"] == "Target.detachFromTarget"
                    && command["params"]["sessionId"] == "worker-session")
                .count(),
            1
        );

        let _ = runtime.close().await;
        drop(fixture.events);
        fixture.server.await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_page_close_does_not_duplicate_inflight_worker_detach() {
        let detach_started = Arc::new(tokio::sync::Notify::new());
        let release_detach = Arc::new(tokio::sync::Notify::new());
        let fixture = start_attached_target_fixture(
            None,
            false,
            false,
            Some((Arc::clone(&detach_started), Arc::clone(&release_detach))),
        )
        .await;
        let (runtime, page) = fixture_page(&fixture).await;
        let _network = page.subscribe_network_events().await.unwrap();
        fixture
            .events
            .send(attached_target_event(
                "page-session",
                Some("worker-session"),
                Some("worker-target"),
                Some("worker"),
                true,
                true,
            ))
            .unwrap();
        wait_for_command_count(&fixture.commands, "Runtime.runIfWaitingForDebugger", 1).await;

        let cancelled_page = page.clone();
        let cancelled_close = tokio::spawn(async move { cancelled_page.close().await });
        tokio::time::timeout(Duration::from_secs(1), detach_started.notified())
            .await
            .expect("Page close should reach the retained worker detach barrier");
        tokio::task::yield_now().await;
        cancelled_close.abort();
        assert!(cancelled_close.await.unwrap_err().is_cancelled());

        let final_page = page.clone();
        let final_close = tokio::spawn(async move { final_page.close().await });
        release_detach.notify_one();
        let report = final_close.await.unwrap();
        assert!(report.is_complete(), "page cleanup failed: {report:?}");
        let commands = fixture.commands.lock().await.clone();
        assert_eq!(
            commands
                .iter()
                .filter(
                    |command| command["method"] == "Runtime.runIfWaitingForDebugger"
                        && command["sessionId"] == "worker-session"
                )
                .count(),
            1
        );
        assert_eq!(
            commands
                .iter()
                .filter(|command| command["method"] == "Target.detachFromTarget"
                    && command["params"]["sessionId"] == "worker-session")
                .count(),
            1
        );

        assert!(runtime.close().await.is_complete());
        drop(fixture.events);
        fixture.server.await.unwrap();
    }

    async fn serve_fixture(listener: tokio::net::TcpListener, body: String) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let newline = String::from_utf8(vec![13, 10]).unwrap();
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let body = body.clone();
            let newline = newline.clone();
            tokio::spawn(async move {
                let mut request = [0_u8; 2048];
                let _ = stream.read(&mut request).await;
                let response = format!(
                    "HTTP/1.1 200 OK{newline}Content-Type: text/html{newline}Content-Length: {}{newline}Connection: close{newline}{newline}{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    }

    #[tokio::test]
    #[ignore = "requires installed Chrome and loopback sockets"]
    async fn live_oopif_keeps_logical_frame_and_routes_to_its_session() {
        use crate::runtime::{BrowserRuntime, LaunchOptions, PageEvent};
        use std::time::Duration;

        let grandchild_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind grandchild origin");
        let grandchild_port = grandchild_listener.local_addr().unwrap().port();
        let child_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind child origin");
        let child_port = child_listener.local_addr().unwrap().port();
        let parent_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind parent origin");
        let parent_port = parent_listener.local_addr().unwrap().port();
        let child_server = tokio::spawn(serve_fixture(
            child_listener,
            format!(
                r#"<html><body>child<iframe src="http://127.0.0.1:{grandchild_port}/"></iframe></body></html>"#
            ),
        ));
        let grandchild_server = tokio::spawn(serve_fixture(
            grandchild_listener,
            "<html><body>grandchild</body></html>".to_owned(),
        ));
        let parent_server = tokio::spawn(serve_fixture(
            parent_listener,
            format!(
                r#"<html><body><iframe src="http://localhost:{child_port}/"></iframe></body></html>"#
            ),
        ));

        let runtime = BrowserRuntime::launch(
            LaunchOptions::default()
                .headless(true)
                .arg("--site-per-process"),
        )
        .await
        .expect("launch Chrome");
        let session = runtime.default_session().await.expect("default session");
        let page = session
            .new_page(format!("http://127.0.0.1:{parent_port}/"))
            .await
            .expect("open fixture");
        let main = page.main_frame().await.expect("main frame");
        let main_session_id = main.cdp_session().await.unwrap().id().to_owned();

        let frames_result = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let frames = page.frames().await.unwrap();
                if frames.len() >= 3 {
                    let mut route_ids = BTreeSet::new();
                    for frame in &frames {
                        route_ids.insert(frame.cdp_session().await.unwrap().id().to_owned());
                    }
                    if route_ids.len() >= 3 {
                        break frames;
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        if frames_result.is_err() {
            let targets = cdpkit::target::methods::GetTargets::new()
                .send(runtime.cdp())
                .await
                .unwrap();
            eprintln!("targets: {:#?}", targets.target_infos);
            for frame in page.frames().await.unwrap() {
                let route = frame
                    .cdp_session()
                    .await
                    .map(|session| session.id().to_owned());
                eprintln!("frame={} route={route:?}", frame.id());
            }
        }
        let frames = frames_result.expect("nested OOPIF routes did not appear");
        let mut child = None;
        for frame in &frames {
            if frame
                .parent()
                .await
                .unwrap()
                .as_ref()
                .is_some_and(|parent| parent.id() == main.id())
            {
                child = Some(frame);
                break;
            }
        }
        let child = child.expect("child frame");
        let mut grandchild = None;
        for frame in &frames {
            if frame
                .parent()
                .await
                .unwrap()
                .as_ref()
                .is_some_and(|parent| parent.id() == child.id())
            {
                grandchild = Some(frame);
                break;
            }
        }
        let grandchild = grandchild.expect("grandchild frame");
        assert!(child.parent().await.unwrap().is_some());
        assert_ne!(child.cdp_session().await.unwrap().id(), main_session_id);
        assert_ne!(
            grandchild.cdp_session().await.unwrap().id(),
            main_session_id
        );
        assert_ne!(
            grandchild.cdp_session().await.unwrap().id(),
            child.cdp_session().await.unwrap().id()
        );

        let mut events = page
            .subscribe_events()
            .await
            .expect("subscribe typed events");
        cdpkit::runtime::methods::Evaluate::new(
            r#"console.log('typed-ready');
               history.pushState({}, '', '#typed-event');
               const frame = document.createElement('iframe');
               frame.src = 'about:blank';
               document.body.append(frame);
               setTimeout(() => frame.remove(), 100);
               setTimeout(() => { throw new Error('typed-boom'); }, 0);"#,
        )
        .send(page.cdp_session())
        .await
        .expect("trigger browser events");

        let observed = tokio::time::timeout(Duration::from_secs(5), async {
            let mut console = false;
            let mut error = false;
            let mut same_document = false;
            let mut attached = false;
            let mut detached = false;
            while !(console && error && same_document && attached && detached) {
                let event = events
                    .next()
                    .await
                    .expect("page event stream open")
                    .expect("page event");
                match event.event() {
                    PageEvent::Console(message) => {
                        console |= message
                            .arguments
                            .iter()
                            .any(|argument| argument.value == Some(json!("typed-ready")))
                    }
                    PageEvent::JavaScriptError(error_fact) => {
                        error |= error_fact
                            .exception_description
                            .as_deref()
                            .is_some_and(|description| description.contains("typed-boom"))
                    }
                    PageEvent::FrameNavigated {
                        same_document: true,
                        ..
                    } => same_document = true,
                    PageEvent::FrameAttached { .. } => attached = true,
                    PageEvent::FrameDetached { .. } => detached = true,
                    _ => {}
                }
            }
        })
        .await;
        assert!(
            observed.is_ok(),
            "typed console/error/navigation/frame events did not all arrive"
        );
        assert!(runtime.close().await.is_complete());
        parent_server.abort();
        child_server.abort();
        grandchild_server.abort();
    }
}
