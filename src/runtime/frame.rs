use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Weak};

use cdpkit::page::events::{FrameAttached, FrameDetached, FrameNavigated};
use cdpkit::page::methods::{Enable, GetFrameTree};
use cdpkit::page::types::{FrameDetachedReason, FrameTree};
use cdpkit::target::events::{AttachedToTarget, DetachedFromTarget};
use cdpkit::target::methods::{DetachFromTarget, SetAutoAttach};
use futures::StreamExt;
use parking_lot::RwLock;
use tokio_util::sync::CancellationToken;

use crate::runtime::{
    BrowserError, DocumentEpoch, FrameId, InvalidationReason, Page, PageGeneration, PageInner,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameSnapshot {
    pub page_generation: PageGeneration,
    pub document_epoch: DocumentEpoch,
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

#[derive(Debug, Clone)]
struct FrameRecord {
    parent: Option<String>,
    children: BTreeSet<String>,
    loader_id: Option<String>,
    document_epoch: DocumentEpoch,
    route_session_id: String,
    route_target_id: Option<String>,
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

    fn route_to_session(&mut self, frame_id: &str, session_id: &str, target_id: Option<&str>) {
        self.attach(frame_id, None);
        if let Some(record) = self.frames.get_mut(frame_id) {
            record.route_session_id = session_id.to_owned();
            record.route_target_id = target_id.map(str::to_owned);
        }
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
        self.route_to_session(frame_id, session_id, Some(target_id));
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

    fn reroute_session(&mut self, session_id: &str, fallback_session_id: &str) {
        for record in self.frames.values_mut() {
            if record.route_session_id == session_id {
                record.route_session_id = fallback_session_id.to_owned();
                record.route_target_id = None;
            }
        }
    }
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
}

struct FrameState {
    graph: FrameGraph,
    sessions: HashMap<String, cdpkit::Session>,
    child_sessions: HashMap<String, ChildSessionOwnership>,
    next_attach_token: u64,
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

struct OopifAttachClaim {
    token: AttachToken,
    cancel: CancellationToken,
}

enum OopifAttachStart {
    Initialize(OopifAttachClaim),
    Idempotent,
    Conflict,
    ForeignParent,
    RouteUnavailable,
}

impl FrameState {
    fn routed_session(&self, frame_id: &str) -> Option<cdpkit::Session> {
        let session_id = self.graph.route_session(frame_id)?;
        self.sessions.get(session_id).cloned()
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
            SessionAttachDisposition::Idempotent => return OopifAttachStart::Idempotent,
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

    fn detach_child_session(&mut self, session_id: &str) {
        if !self
            .child_sessions
            .get(session_id)
            .is_some_and(|ownership| !matches!(ownership.phase, ChildSessionPhase::Detached(_)))
        {
            return;
        }
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
    }
}

pub(crate) struct FrameStore {
    page: Weak<PageInner>,
    runtime: super::BrowserRuntime,
    identity: FrameStoreIdentity,
    state: RwLock<FrameState>,
    cancel: CancellationToken,
}

impl FrameStore {
    fn acknowledge_oopif_initialization(&self, session_id: &str, token: AttachToken) {
        let mut state = self.state.write();
        state.abandon_oopif_attach(session_id, token);
        state.acknowledge_detached(session_id, token);
    }

    pub(crate) async fn initialize(page: Page) -> Result<Arc<Self>, BrowserError> {
        let runtime = page.runtime().clone();
        let identity = FrameStoreIdentity::new(page.generation());
        let main_session = page.cdp_session().clone();
        let mut target_detached = runtime.cdp().observe(["Target.detachedFromTarget"]).await?;
        let (main_events, main_target_attached, tree) =
            Self::prepare_frame_session(&main_session).await?;

        let mut graph = FrameGraph::new(main_session.id());
        Self::merge_tree(&mut graph, &tree, main_session.id());
        let mut sessions = HashMap::new();
        sessions.insert(main_session.id().to_owned(), main_session.clone());
        let store = Arc::new(Self {
            page: page.downgrade_inner(),
            runtime,
            identity,
            state: RwLock::new(FrameState {
                graph,
                sessions,
                child_sessions: HashMap::new(),
                next_attach_token: 0,
            }),
            cancel: CancellationToken::new(),
        });

        Self::spawn_frame_reducer(&store, main_events, store.cancel.child_token());
        Self::spawn_target_attach_reducer(&store, main_target_attached, store.cancel.child_token());
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
                                    store.state.write().detach_child_session(&event.session_id);
                                }
                                Err(error) => tracing::warn!(%error, "invalid detachedFromTarget payload"),
                            }
                        }
                        None => break,
                    },
                }
            }
        });
        SetAutoAttach::new(true, false)
            .with_flatten(true)
            .send(&main_session)
            .await?;
        Ok(store)
    }

    async fn prepare_frame_session(
        session: &cdpkit::Session,
    ) -> Result<(FrameEventStreams, cdpkit::RawEventStream, FrameTree), BrowserError> {
        let attached = FrameAttached::subscribe(session).await?;
        let detached = FrameDetached::subscribe(session).await?;
        let navigated = FrameNavigated::subscribe(session).await?;
        let target_attached = session.observe(["Target.attachedToTarget"]).await?;
        Enable::new().send(session).await?;
        let tree = GetFrameTree::new().send(session).await?.frame_tree;
        Ok((
            FrameEventStreams {
                attached,
                detached,
                navigated,
            },
            target_attached,
            tree,
        ))
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
                            match serde_json::from_value::<AttachedToTarget>((*event.params).clone()) {
                                Ok(event) if event.target_info.type_ == "iframe" => {
                                    store.initialize_oopif(parent_session_id.as_deref(), event).await;
                                }
                                Ok(_) => {},
                                Err(error) => tracing::warn!(%error, "invalid attachedToTarget payload"),
                            }
                        }
                        None => break,
                    },
                }
            }
        });
    }

    fn spawn_frame_reducer(
        store: &Arc<Self>,
        streams: FrameEventStreams,
        cancel: CancellationToken,
    ) {
        let weak_store = Arc::downgrade(store);
        let FrameEventStreams {
            mut attached,
            mut detached,
            mut navigated,
        } = streams;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    event = attached.next() => match event {
                        Some(Ok(event)) => {
                            let Some(store) = weak_store.upgrade() else { break; };
                            store.state.write().graph.attach(&event.frame_id, Some(&event.parent_frame_id));
                        }
                        Some(Err(error)) => tracing::warn!(%error, "frameAttached stream failed"),
                        None => break,
                    },
                    event = detached.next() => match event {
                        Some(Ok(event)) => {
                            let Some(store) = weak_store.upgrade() else { break; };
                            if should_remove_detached_frame(&event.reason) {
                                store.state.write().graph.detach(&event.frame_id);
                            }
                        }
                        Some(Err(error)) => tracing::warn!(%error, "frameDetached stream failed"),
                        None => break,
                    },
                    event = navigated.next() => match event {
                        Some(Ok(event)) => {
                            let Some(store) = weak_store.upgrade() else { break; };
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
                            if is_page_main && changed {
                                let Some(page) = store.page() else { break; };
                                page.lifecycle().commit_new_document();
                            }
                        }
                        Some(Err(error)) => tracing::warn!(%error, "frameNavigated stream failed"),
                        None => break,
                    },
                }
            }
        });
    }

    async fn initialize_oopif(
        self: &Arc<Self>,
        parent_session_id: Option<&str>,
        event: AttachedToTarget,
    ) {
        let frame_id = event.target_info.target_id.clone();
        let parent_frame_id = event.target_info.parent_frame_id.as_deref();
        let start = self.state.write().begin_oopif_attach(
            &event.session_id,
            &frame_id,
            parent_frame_id,
            parent_session_id,
            self.cancel.child_token(),
        );
        let claim = match start {
            OopifAttachStart::Initialize(claim) => claim,
            OopifAttachStart::Idempotent => return,
            OopifAttachStart::Conflict => {
                tracing::warn!(
                    session_id = %event.session_id,
                    %frame_id,
                    ?parent_session_id,
                    "ignored conflicting OOPIF session ownership event"
                );
                return;
            }
            OopifAttachStart::ForeignParent => return,
            OopifAttachStart::RouteUnavailable => {
                self.rollback_oopif_session(
                    &event.session_id,
                    &BrowserError::operation("attach OOPIF", super::OperationPhase::Preparation)
                        .with_message("OOPIF frame already has an active route"),
                )
                .await;
                return;
            }
        };

        let session = self.runtime.cdp().session(event.session_id.clone());
        let prepared = tokio::select! {
            _ = claim.cancel.cancelled() => {
                self.acknowledge_oopif_initialization(&event.session_id, claim.token);
                return;
            },
            prepared = Self::prepare_frame_session(&session) => prepared,
        };
        let (streams, target_attached, tree) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let owned = self
                    .state
                    .write()
                    .abandon_oopif_attach(&event.session_id, claim.token);
                if owned {
                    self.rollback_oopif_session(&event.session_id, &error).await;
                }
                self.acknowledge_oopif_initialization(&event.session_id, claim.token);
                return;
            }
        };
        let auto_attach = SetAutoAttach::new(true, false)
            .with_flatten(true)
            .send(&session);
        if let Err(error) = tokio::select! {
            _ = claim.cancel.cancelled() => {
                self.acknowledge_oopif_initialization(&event.session_id, claim.token);
                return;
            },
            result = auto_attach => result,
        } {
            let error = BrowserError::from(error);
            let owned = self
                .state
                .write()
                .abandon_oopif_attach(&event.session_id, claim.token);
            if owned {
                self.rollback_oopif_session(&event.session_id, &error).await;
            }
            self.acknowledge_oopif_initialization(&event.session_id, claim.token);
            return;
        }

        let reducer_cancel = claim.cancel.clone();
        let (accepted, should_rollback) = {
            let mut state = self.state.write();
            if !state.is_initializing_oopif_attach(&event.session_id, claim.token) {
                (false, false)
            } else {
                let accepted = state.graph.route_oopif(
                    &frame_id,
                    parent_frame_id,
                    parent_session_id,
                    &event.session_id,
                    &event.target_info.target_id,
                );
                if accepted {
                    Self::merge_tree(&mut state.graph, &tree, &event.session_id);
                    state.graph.route_to_session(
                        &frame_id,
                        &event.session_id,
                        Some(&event.target_info.target_id),
                    );
                    state.sessions.insert(event.session_id.clone(), session);
                    debug_assert!(state.activate_oopif_attach(&event.session_id, claim.token));
                } else {
                    state.abandon_oopif_attach(&event.session_id, claim.token);
                }
                (accepted, !accepted)
            }
        };
        if accepted {
            Self::spawn_frame_reducer(self, streams, reducer_cancel.clone());
            Self::spawn_target_attach_reducer(self, target_attached, reducer_cancel);
        } else {
            if should_rollback {
                self.rollback_oopif_session(
                    &event.session_id,
                    &BrowserError::operation("attach OOPIF", super::OperationPhase::Preparation)
                        .with_message("OOPIF parent route changed during initialization"),
                )
                .await;
            }
            self.acknowledge_oopif_initialization(&event.session_id, claim.token);
        }
    }

    async fn rollback_oopif_session(&self, session_id: &str, error: &BrowserError) {
        tracing::warn!(%error, %session_id, "failed to initialize OOPIF session");
        if let Err(cleanup_error) = DetachFromTarget::new()
            .with_session_id(session_id.to_owned())
            .send(self.runtime.cdp())
            .await
        {
            tracing::warn!(%cleanup_error, %session_id, "failed to detach unusable OOPIF session");
        }
    }

    fn merge_tree(graph: &mut FrameGraph, tree: &FrameTree, route_session_id: &str) {
        graph.navigate(
            &tree.frame.id,
            tree.frame.parent_id.as_deref(),
            &tree.frame.loader_id,
        );
        graph.route_to_session(&tree.frame.id, route_session_id, None);
        if let Some(children) = &tree.child_frames {
            for child in children {
                Self::merge_tree(graph, child, route_session_id);
            }
        }
    }

    pub(crate) fn handle(&self, frame_id: &str) -> Option<Frame> {
        let page = self.page()?;
        let snapshot = self.identity.snapshot(&self.state.read().graph, frame_id)?;
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
        self.state.read().graph.ids()
    }

    fn validate(&self, frame: &Frame) -> Result<(), BrowserError> {
        let page = self.page().ok_or_else(|| {
            BrowserError::operation("use frame", super::OperationPhase::Preparation)
                .with_message("page was dropped")
        })?;
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
        if current.document_epoch != frame.snapshot.document_epoch {
            return Err(self.invalidation_error(frame, InvalidationReason::DocumentChanged));
        }
        Ok(())
    }

    fn invalidation_error(&self, frame: &Frame, reason: InvalidationReason) -> BrowserError {
        BrowserError::operation("use frame", super::OperationPhase::Preparation)
            .with_message(format!("frame {} is stale: {reason:?}", frame.id))
    }

    pub(crate) fn cancel(&self) {
        self.cancel.cancel();
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
    use tokio_tungstenite::tungstenite::Message;

    fn graph() -> FrameGraph {
        FrameGraph::new("page-session")
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

    async fn start_frame_store_cdp_server() -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut write, mut read) = websocket.split();
            while let Some(Ok(Message::Text(text))) = read.next().await {
                let command: Value = serde_json::from_str(&text).unwrap();
                let id = command["id"].as_u64().unwrap();
                let method = command["method"].as_str().unwrap();
                let result = match method {
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
                    "Page.enable" | "Target.setAutoAttach" => json!({}),
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
            }
        });
        (format!("ws://{address}"), server)
    }

    #[tokio::test]
    async fn initialized_frame_store_handle_stays_stale_after_target_destroyed() {
        use crate::runtime::{BrowserRuntime, BrowserSessionId, InvalidationReason, PageOwnership};

        let (url, server) = start_frame_store_cdp_server().await;
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

    #[test]
    fn oopif_route_changes_without_changing_frame_identity() {
        let mut graph = graph();
        graph.navigate("main", None, "loader-main");
        graph.navigate("child", Some("main"), "loader-child");
        let before = graph.ids();

        graph.route_to_session("child", "oopif-session", Some("oopif-target"));
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
        }
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

        state.detach_child_session("child-session");

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
            OopifAttachStart::Idempotent
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

        state.detach_child_session("child-session");

        assert!(descendant_claim.cancel.is_cancelled());
        assert!(!state.child_sessions.contains_key("child-session"));
        assert_eq!(
            state.child_session_phase("grandchild-session"),
            Some(ChildSessionPhase::Detached(descendant_claim.token))
        );
        assert_eq!(state.graph.route_session("child"), Some("page-session"));
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
        state.detach_child_session("child-session");

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
        use crate::runtime::{BrowserRuntime, LaunchOptions};
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
                r#"<html><body>child<iframe src="http://grandchild.test:{grandchild_port}/"></iframe></body></html>"#
            ),
        ));
        let grandchild_server = tokio::spawn(serve_fixture(
            grandchild_listener,
            "<html><body>grandchild</body></html>".to_owned(),
        ));
        let parent_server = tokio::spawn(serve_fixture(
            parent_listener,
            format!(
                r#"<html><body><iframe src="http://child.test:{child_port}/"></iframe></body></html>"#
            ),
        ));

        let runtime = BrowserRuntime::launch(
            LaunchOptions::default()
                .headless(true)
                .arg("--site-per-process")
                .arg("--host-resolver-rules=MAP *.test 127.0.0.1")
                .arg("--no-proxy-server"),
        )
        .await
        .expect("launch Chrome");
        let session = runtime.default_session().await.expect("default session");
        let page = session
            .new_page(format!("http://parent.test:{parent_port}/"))
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
        assert!(runtime.close().await.is_complete());
        parent_server.abort();
        child_server.abort();
        grandchild_server.abort();
    }
}
