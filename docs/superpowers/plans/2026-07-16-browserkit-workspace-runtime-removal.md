# browserkit Workspace Runtime Removal Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace browserkit's workspace/session dual runtime with one session-only runtime while preserving useful page, storage, dialog, attach, network-blocking, and raw-CDP capabilities.

**Architecture:** Add tab ownership, a single session/target resolver, and browser-scoped target lifecycle watchers before moving every retained capability onto sessions. Upgrade persistence from schema v2 to session-only v3 with a one-way backup-and-migrate reader, then remove all workspace state, commands, routes, modules, tests, configuration, and documentation in the same breaking migration.

**Tech Stack:** Rust 1.75, tokio, clap, serde/serde_json, dashmap, parking_lot, cdpkit path dependency, Chrome DevTools Protocol, cargo test, cargo clippy

## Global Constraints

- `cdpkit-rs` remains the typed CDP protocol layer; browser lifecycle and automation policy remain in browserkit.
- The final runtime has one state tree: `DaemonState.sessions`. Do not introduce `WorkspaceOrSession`, a `wid` compatibility adapter, or a second transitional runtime abstraction.
- A tab created by browserkit is `Owned`; a user tab adopted by `attach` is `Attached`.
- Closing or cleaning up an `Attached` tab must detach browserkit without closing the Chrome target.
- Explicit missing session or target values never fall back to defaults.
- Isolated-session cookie operations always use the session BrowserContext and never fall back to browser-wide operations.
- Supported daemon commands have one canonical route; remove `v2.*`, `ws.*`, `tab.*`, `nav.*`, `page.*`, and old `storage.*` aliases.
- CLI and daemon responses remain JSON-only.
- State schema v2 is read only by the one-way migration module; schema v3 never writes workspace fields.
- Every production behavior change follows red-green TDD and ends in a focused commit.
- Do not modify cdpkit-rs unless a missing protocol binding is proven. If required, stop this plan, implement and release that protocol-layer change first, then update browserkit's dependency deliberately.
- Do not stage or commit `.codex/`.

## File Map

### New files

- `src/daemon/target_lifecycle.rs`: browser-scoped target watcher, target ownership lookup, new-tab events, and session tab registration/removal.
- `src/daemon/handler/attach.rs`: session-native adoption of an existing browser target.
- `src/daemon/handler/inspect.rs`: session-native find, search, HTML, console, and PDF commands.
- `src/daemon/persist/migrate_v2.rs`: v2-only structs and deterministic v2-to-v3 conversion.
- `src/daemon/persist/fixtures/state-v2-mixed.json`: migration fixture containing sessions, isolated workspace, attached workspace, conflicts, and dropped tabs.

### Core files modified throughout the plan

- `src/daemon/session.rs`: `TabOwnership`, session tab runtime data, and ownership-aware helpers.
- `src/daemon/state.rs`: session-only daemon state, watcher registry, target event channel, and migration report.
- `src/daemon/handler/common.rs`: the only session/target resolver used by page-facing handlers.
- `src/daemon/handler/open.rs`, `tabs.rs`, `act.rs`: owned tab creation, ownership-aware close, and click new-tab reporting.
- `src/daemon/console.rs`, `dialog.rs`: session/target keyed subscription state.
- `src/daemon/handler/storage.rs`, `dialog.rs`, `network.rs`, `debug.rs`: retained capabilities moved to sessions.
- `src/daemon/handler/browser.rs`, `daemon.rs`, `connect.rs`: browser discovery, disconnect, status, shutdown, and watcher startup.
- `src/daemon/persist.rs`: schema v3 serialization, loading, restore, and migration integration.
- `src/daemon/server.rs`: session-only idle cleanup and shutdown behavior.
- `src/daemon/handler/mod.rs`: canonical route table and route-removal tests.
- `src/main.rs`: canonical CLI surface and JSON request construction.
- `src/error.rs`, `src/daemon/protocol.rs`: structured errors after workspace error removal.
- `src/config.rs`: removal of workspace limits and workspace timeout configuration.

### Files deleted at the final removal task

- `src/workspace/mod.rs`
- `src/daemon/auto_attach.rs`
- `src/daemon/handler/workspace.rs`
- `src/daemon/handler/tab.rs`
- `src/daemon/handler/nav.rs`
- `src/daemon/handler/page.rs`

### Documentation updated at closeout

- `README.md`
- `AGENTS.md`
- `docs/REDESIGN.md`
- `docs/ROADMAP.md`
- `docs/project-analysis.md`
- `docs/connect-existing-chrome.md`
- `docs/v1-legacy-retention-report.md`
- `docs/bk-browser/SKILL.md`
- `docs/bk-browser/references/commands.md`
- `docs/bk-browser.zip`

---

### Task 1: Add tab ownership and the unified session target resolver

**Files:**
- Modify: `src/daemon/session.rs`
- Modify: `src/daemon/console.rs`
- Modify: `src/daemon/handler/common.rs`
- Modify: `src/daemon/state.rs`
- Modify: `src/error.rs`

**Interfaces:**
- Produces: `TabOwnership::{Owned, Attached}`.
- Produces: `SessionTab::new_owned(...)` and `SessionTab::new_attached(...)`.
- Produces: `SessionTargetContext` and `resolve_session_target(state, params)` for every later handler.
- Produces: `ErrorCode::TargetAlreadyAttached`.

- [ ] **Step 1: Write failing ownership serialization and close-policy tests**

Add to `src/daemon/session.rs`:

```rust
#[test]
fn session_tab_ownership_round_trips() {
    let owned = SessionTab::new_owned("T1".into(), "https://a.test".into(), "A".into());
    let attached = SessionTab::new_attached(
        "T2".into(),
        "https://b.test".into(),
        "B".into(),
        "S2".into(),
    );
    assert_eq!(owned.ownership, TabOwnership::Owned);
    assert_eq!(attached.ownership, TabOwnership::Attached);
    assert_eq!(serde_json::from_str::<SessionTab>(&serde_json::to_string(&attached).unwrap()).unwrap().ownership, TabOwnership::Attached);
}

#[test]
fn session_tab_close_policy_follows_ownership() {
    assert_eq!(TabOwnership::Owned.close_policy(), TabClosePolicy::CloseTarget);
    assert_eq!(TabOwnership::Attached.close_policy(), TabClosePolicy::DetachSession);
}
```

- [ ] **Step 2: Run the ownership tests and confirm red**

Run:

```powershell
cargo test session_tab_ownership_round_trips
cargo test session_tab_close_policy_follows_ownership
```

Expected: FAIL because `TabOwnership`, constructors, and `TabClosePolicy` do not exist.

- [ ] **Step 3: Implement ownership and session console state**

Add these definitions to `src/daemon/session.rs` and use `new_owned` from the existing `Session::add_tab` path:

```rust
use std::collections::VecDeque;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TabOwnership {
    #[default]
    Owned,
    Attached,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabClosePolicy {
    CloseTarget,
    DetachSession,
}

impl TabOwnership {
    pub fn close_policy(self) -> TabClosePolicy {
        match self {
            Self::Owned => TabClosePolicy::CloseTarget,
            Self::Attached => TabClosePolicy::DetachSession,
        }
    }
}

pub type ConsoleLog = Arc<parking_lot::Mutex<VecDeque<crate::page::ConsoleEntry>>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTab {
    pub target_id: String,
    pub url: String,
    pub title: String,
    pub cdp_session_id: String,
    pub ownership: TabOwnership,
    #[serde(skip, default = "new_console_log")]
    pub console_log: ConsoleLog,
}

pub fn new_console_log() -> ConsoleLog {
    Arc::new(parking_lot::Mutex::new(VecDeque::with_capacity(200)))
}
```

Constructors set an empty CDP session ID for new owned targets and preserve the supplied CDP session ID for attached targets. Keep `ownership` backward-compatible with `#[serde(default)]` so existing persisted session tabs deserialize as owned.

- [ ] **Step 4: Write failing resolver tests**

Add to `src/daemon/handler/common.rs`:

```rust
#[test]
fn explicit_missing_session_does_not_fall_back() {
    let state = DaemonState::new();
    state.sessions.insert("default".into(), Session::new_default("localhost:9222".into()));
    let error = resolve_session_selection(&state, Some("missing")).unwrap_err();
    assert_eq!(error_code(&error), "SESSION_NOT_FOUND");
}

#[test]
fn explicit_missing_target_does_not_use_active_target() {
    let state = DaemonState::new();
    let mut session = Session::new_default("localhost:9222".into());
    session.add_tab("T1".into(), "https://a.test".into(), "A".into());
    state.sessions.insert("default".into(), session);
    let error = resolve_target_selection(&state, "default", Some("missing")).unwrap_err();
    assert_eq!(error_code(&error), "TARGET_NOT_FOUND");
}
```

- [ ] **Step 5: Implement the resolver contract**

Replace workspace-oriented common resolution with these session interfaces while keeping legacy helpers temporarily for still-unmigrated handlers:

```rust
#[derive(Clone)]
pub struct SessionTargetContext {
    pub session_name: String,
    pub target_id: String,
    pub browser_host: String,
    pub browser_context_id: Option<String>,
    pub cdp: Arc<cdpkit::CDP>,
    pub cdp_session_id: String,
}

pub fn resolve_session_target(
    state: &DaemonState,
    params: &serde_json::Value,
) -> Result<SessionTargetContext, Response>;
```

The implementation reads string fields `session` and `target`, uses `default` only when `session` is absent, uses `active_target` only when `target` is absent, checks `Session::check_connected`, verifies browser presence, clones the browser CDP handle, and returns structured session/target errors. Add `TargetAlreadyAttached` to `ErrorCode` with code `TARGET_ALREADY_ATTACHED`, a recoverable classification, and hint `detach the target from its current session first`.

- [ ] **Step 6: Run focused tests and commit**

Run:

```powershell
cargo test daemon::session --quiet
cargo test daemon::handler::common --quiet
cargo test error --quiet
cargo clippy --all -- -D warnings
git diff --check
```

Expected: all commands exit 0.

Commit:

```powershell
git add src/daemon/session.rs src/daemon/console.rs src/daemon/handler/common.rs src/daemon/state.rs src/error.rs
git commit -m "Add session tab ownership and target resolution"
```

### Task 2: Build session-native target lifecycle and subscriptions

**Files:**
- Create: `src/daemon/target_lifecycle.rs`
- Modify: `src/daemon/mod.rs`
- Modify: `src/daemon/state.rs`
- Modify: `src/daemon/console.rs`
- Modify: `src/daemon/dialog.rs`

**Interfaces:**
- Consumes: `TabOwnership`, `SessionTab`, and `SessionTargetContext` from Task 1.
- Produces: `TargetLifecycleEvent::{Created, Destroyed, Updated}`.
- Produces: `ensure_target_watcher`, `find_target_owner`, `register_session_tab`, `remove_session_tab`, and `subscribe_target_events`.

- [ ] **Step 1: Write failing pure lifecycle tests**

Create `src/daemon/target_lifecycle.rs` with a test module first:

```rust
#[test]
fn target_owner_is_unique_across_sessions() {
    let state = DaemonState::new();
    let mut first = Session::new_default("localhost:9222".into());
    first.add_tab("T1".into(), "https://a.test".into(), "A".into());
    state.sessions.insert("default".into(), first);
    let mut other = Session::new_default("localhost:9222".into());
    other.name = "other".into();
    state.sessions.insert("other".into(), other);
    assert_eq!(find_target_owner(&state, "T1"), Some("default".into()));
    assert_eq!(register_session_tab(&state, "other", SessionTab::new_attached(
        "T1".into(), "https://a.test".into(), "A".into(), "S1".into()
    )).unwrap_err(), ErrorCode::TargetAlreadyAttached);
}

#[test]
fn destroyed_target_is_removed_from_owning_session() {
    let state = DaemonState::new();
    let mut session = Session::new_default("localhost:9222".into());
    session.add_tab("T1".into(), "https://a.test".into(), "A".into());
    state.sessions.insert("default".into(), session);
    let removed = remove_session_tab(&state, "T1").unwrap();
    assert_eq!(removed.0, "default");
    assert!(state.sessions.get("default").unwrap().tabs.is_empty());
}

#[test]
fn opener_target_maps_new_target_to_the_same_session() {
    let state = DaemonState::new();
    let mut session = Session::new_isolated(
        "agent".into(), "localhost:9222".into(), "CTX1".into()
    );
    session.add_tab("OPENER".into(), "https://a.test".into(), "A".into());
    state.sessions.insert("agent".into(), session);
    assert_eq!(session_for_created_target(&state, Some("OPENER"), Some("CTX1")), Some("agent".into()));
}
```

- [ ] **Step 2: Run lifecycle tests and confirm red**

Run: `cargo test target_lifecycle --quiet`

Expected: FAIL because the module and lifecycle helpers do not exist.

- [ ] **Step 3: Implement lifecycle state and event contracts**

Add to `DaemonState`:

```rust
pub target_watchers: DashMap<String, CancellationToken>,
pub target_events: tokio::sync::broadcast::Sender<TargetLifecycleEvent>,
```

Define:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetLifecycleEvent {
    Created { session: String, target_id: String, opener_id: Option<String> },
    Destroyed { session: String, target_id: String },
    Updated { session: String, target_id: String, url: String, title: String },
}

pub fn subscribe_target_events(state: &DaemonState) -> broadcast::Receiver<TargetLifecycleEvent> {
    state.target_events.subscribe()
}
```

Use these exact pure-state signatures:

```rust
pub fn register_session_tab(
    state: &DaemonState,
    session_name: &str,
    tab: SessionTab,
) -> Result<(), ErrorCode>;

pub fn remove_session_tab(
    state: &DaemonState,
    target_id: &str,
) -> Option<(String, SessionTab)>;
```

`register_session_tab` rejects duplicate ownership, inserts the tab, updates active target, requests persistence, and starts no background task itself. `remove_session_tab` finds the single owner, removes the tab, cancels console/dialog subscriptions for `(session, target)`, emits `Destroyed`, and persists.

- [ ] **Step 4: Re-key console and dialog subscription state**

Change console and dialog APIs from `(wid, tid)` to `(session_name, target_id)`:

```rust
pub type DialogKey = (String, String);

pub fn spawn_console_subscription(
    state: Arc<DaemonState>,
    session_name: String,
    target_id: String,
    cdp: Arc<cdpkit::CDP>,
    cdp_session_id: String,
) -> CancellationToken;

pub fn spawn_dialog_subscription(
    state: Arc<DaemonState>,
    session_name: String,
    target_id: String,
    cdp: Arc<cdpkit::CDP>,
    cdp_session_id: String,
) -> CancellationToken;
```

Console events append to `SessionTab.console_log`. Dialog policies are keyed by session name, pending dialogs by `(session, target)`, and cancellation removes both subscription tokens and pending entries for that target.

- [ ] **Step 5: Implement one watcher per browser**

Implement:

```rust
pub fn ensure_target_watcher(
    state: &Arc<DaemonState>,
    host: &str,
    cdp: Arc<cdpkit::CDP>,
) -> CancellationToken;
```

Use cdpkit Target event streams with `flatten=true`. The watcher handles created, destroyed, and target-info-changed events. A created page target is assigned only when its opener target already belongs to a session or its BrowserContext uniquely identifies an isolated session. It attaches to the target, registers it as `Owned`, starts console/dialog subscriptions, and emits `Created`. Unknown user targets remain untracked until explicit `attach`.

- [ ] **Step 6: Run lifecycle tests and commit**

Run:

```powershell
cargo test target_lifecycle --quiet
cargo test daemon::console --quiet
cargo test daemon::dialog --quiet
cargo clippy --all -- -D warnings
git diff --check
```

Expected: all commands exit 0.

Commit:

```powershell
git add src/daemon/target_lifecycle.rs src/daemon/mod.rs src/daemon/state.rs src/daemon/console.rs src/daemon/dialog.rs
git commit -m "Add session target lifecycle tracking"
```

### Task 3: Add attach, ownership-aware close, and click new-tab reporting

**Files:**
- Create: `src/daemon/handler/attach.rs`
- Modify: `src/daemon/handler/mod.rs`
- Modify: `src/daemon/handler/connect.rs`
- Modify: `src/daemon/handler/open.rs`
- Modify: `src/daemon/handler/tabs.rs`
- Modify: `src/daemon/handler/act.rs`
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: lifecycle APIs from Task 2.
- Produces: canonical daemon route `attach` and CLI `bk attach`.
- Produces: ownership-aware close behavior for tabs and sessions.
- Produces: `data.new_tab` for click when the lifecycle watcher observes a matching created target.

- [ ] **Step 1: Add failing CLI and handler tests for attach**

Add:

```rust
#[test]
fn cli_parses_attach_target_and_pattern() {
    let target = try_parse(&["bk", "attach", "--target", "ABC123"]).unwrap();
    assert!(matches!(target.command, Command::Attach { pattern: None }));
    let pattern = try_parse(&["bk", "attach", "github.com"]).unwrap();
    assert!(matches!(pattern.command, Command::Attach { pattern: Some(ref p) } if p == "github.com"));
}

#[tokio::test]
async fn attach_requires_existing_session() {
    let req = Request { cmd: "attach".into(), params: json!({"target": "T1"}), token: None };
    let value = serde_json::to_value(handle_attach(&req, &Arc::new(DaemonState::new())).await).unwrap();
    assert_eq!(value["error"]["code"], "SESSION_NOT_FOUND");
}

#[test]
fn attach_pattern_requires_one_match() {
    let targets = vec![
        AttachCandidate { target_id: "T1".into(), url: "https://a.test".into(), title: "A".into(), browser_context_id: None },
        AttachCandidate { target_id: "T2".into(), url: "https://a.test/2".into(), title: "A2".into(), browser_context_id: None },
    ];
    assert_eq!(select_attach_target(&targets, None, Some("a.test")).unwrap_err(), ErrorCode::InvalidArgument);
}

#[test]
fn attach_rejects_target_from_another_browser_context() {
    let session = Session::new_isolated("agent".into(), "localhost:9222".into(), "CTX1".into());
    let candidate = AttachCandidate {
        target_id: "T1".into(),
        url: "https://a.test".into(),
        title: "A".into(),
        browser_context_id: None,
    };
    assert_eq!(validate_attach_context(&session, &candidate).unwrap_err(), ErrorCode::InvalidArgument);
}
```

- [ ] **Step 2: Run attach tests and confirm red**

Run:

```powershell
cargo test cli_parses_attach_target_and_pattern
cargo test attach_requires_existing_session
cargo test attach_pattern_requires_one_match
cargo test attach_rejects_target_from_another_browser_context
```

Expected: FAIL because `Command::Attach`, `handle_attach`, and selection helpers do not exist.

- [ ] **Step 3: Implement explicit attach**

Add the CLI shape:

```rust
Attach {
    /// URL, title, or target ID substring; omit when global --target is present.
    pattern: Option<String>,
},
```

Define the pure selection input and signature:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
struct AttachCandidate {
    target_id: String,
    url: String,
    title: String,
    browser_context_id: Option<String>,
}

fn select_attach_target(
    candidates: &[AttachCandidate],
    target_id: Option<&str>,
    pattern: Option<&str>,
) -> Result<AttachCandidate, ErrorCode>;

fn validate_attach_context(
    session: &Session,
    candidate: &AttachCandidate,
) -> Result<(), ErrorCode>;
```

Build params containing `session`, `target`, and `pattern`. The handler resolves the session without requiring an existing tab, lists page targets from the same browser, excludes internal pages using the existing target filter logic, requires exactly one match, rejects targets owned by another session, and rejects targets outside the destination session's BrowserContext. It then attaches with flatten enabled, creates `SessionTab::new_attached`, registers it, starts subscriptions, and returns `{session, target, ownership: "attached"}`.

- [ ] **Step 4: Write and implement ownership-aware close tests**

Add a pure close decision test:

```rust
#[test]
fn close_action_uses_tab_ownership() {
    let owned = SessionTab::new_owned("T1".into(), "about:blank".into(), String::new());
    let attached = SessionTab::new_attached(
        "T2".into(), "https://a.test".into(), "A".into(), "S2".into()
    );
    assert_eq!(close_action(&owned), CloseAction::CloseTarget("T1".into()));
    assert_eq!(close_action(&attached), CloseAction::DetachSession("S2".into()));
}
```

Run `cargo test close_action_uses_tab_ownership` and confirm it fails. Implement this enum and function in `tabs.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
enum CloseAction {
    CloseTarget(String),
    DetachSession(String),
}

fn close_action(tab: &SessionTab) -> CloseAction {
    match tab.ownership.close_policy() {
        TabClosePolicy::CloseTarget => CloseAction::CloseTarget(tab.target_id.clone()),
        TabClosePolicy::DetachSession => CloseAction::DetachSession(tab.cdp_session_id.clone()),
    }
}
```

Execute `Target.closeTarget` only for owned tabs and `Target.detachFromTarget` only when an attached tab has a non-empty CDP session ID. Both paths remove the session tab and subscriptions after CDP success.

- [ ] **Step 5: Connect open and click to the watcher**

Start `ensure_target_watcher` after a browser connects. Make `open` register the created target as owned if the watcher has not already done so. In click execution, subscribe before dispatching the click and wait up to the action timeout for:

```rust
TargetLifecycleEvent::Created {
    session,
    target_id,
    opener_id: Some(opener),
} if session == session_name && opener == current_target
```

Insert the matching `target_id` into `ActionSuccess.data["new_tab"]`; timeout means no new-tab field, not action failure.

- [ ] **Step 6: Run focused tests and commit**

Run:

```powershell
cargo test attach --quiet
cargo test close_action --quiet
cargo test new_tab --quiet
cargo test open --quiet
cargo clippy --all -- -D warnings
git diff --check
```

Expected: all commands exit 0.

Commit:

```powershell
git add src/daemon/handler/attach.rs src/daemon/handler/mod.rs src/daemon/handler/connect.rs src/daemon/handler/open.rs src/daemon/handler/tabs.rs src/daemon/handler/act.rs src/main.rs
git commit -m "Add session-native target attachment"
```

### Task 4: Migrate find, search, HTML, console, and PDF to sessions

**Files:**
- Create: `src/daemon/handler/inspect.rs`
- Modify: `src/daemon/handler/mod.rs`
- Modify: `src/main.rs`
- Modify: `src/page/capture.rs`
- Modify: `src/page/find_elements.rs`
- Modify: `src/page/state.rs`

**Interfaces:**
- Consumes: `resolve_session_target` and `SessionTab.console_log`.
- Produces: canonical routes `find`, `search`, `html`, `console`, and `pdf`.

- [ ] **Step 1: Add failing canonical-route resolution tests**

Add to `inspect.rs`:

```rust
#[tokio::test]
async fn inspect_commands_use_session_resolution() {
    let state = Arc::new(DaemonState::new());
    for (cmd, params) in [
        ("find", json!({"selector": "a"})),
        ("search", json!({"text": "needle"})),
        ("html", json!({})),
        ("console", json!({"level": "all"})),
        ("pdf", json!({})),
    ] {
        let req = Request { cmd: cmd.into(), params, token: None };
        let value = serde_json::to_value(handle_inspect(&req, &state).await).unwrap();
        assert_eq!(value["error"]["code"], "SESSION_NOT_FOUND", "{cmd}");
    }
}
```

- [ ] **Step 2: Run the inspect test and confirm red**

Run: `cargo test inspect_commands_use_session_resolution`

Expected: FAIL because `inspect.rs` and canonical handlers do not exist.

- [ ] **Step 3: Move retained page logic behind the session resolver**

Implement one dispatcher:

```rust
pub async fn handle_inspect(req: &Request, state: &Arc<DaemonState>) -> Response {
    let result = match req.cmd.as_str() {
        "find" => do_find(req, state).await,
        "search" => do_search(req, state).await,
        "html" => do_html(req, state).await,
        "console" => do_console(req, state).await,
        "pdf" => do_pdf(req, state).await,
        _ => unreachable!("canonical inspect route"),
    };
    result.unwrap_or_else(Response::from)
}
```

Move parameter parsing from `handler/page.rs`, but replace every workspace lookup with `resolve_session_target`. Reuse page-layer functions for arbitrary CSS find, scoped/regex search, outer HTML, and `Page.printToPDF`. Console reads the resolved `SessionTab.console_log`. PDF no longer accepts a URL or creates a temporary workspace; it prints the currently selected session target.

- [ ] **Step 4: Make CLI requests canonical and prove shape**

Keep top-level `Find`, `Search`, `Html`, `Console`, and `Pdf`, remove their hidden markers, and send `find`, `search`, `html`, `console`, and `pdf` with global session/target fields. Add:

```rust
#[test]
fn pdf_no_longer_accepts_a_url() {
    assert!(try_parse(&["bk", "pdf", "https://example.com"]).is_err());
    assert!(try_parse(&["bk", "pdf", "--output", "page.pdf"]).is_ok());
}
```

- [ ] **Step 5: Run focused tests and commit**

Run:

```powershell
cargo test inspect --quiet
cargo test pdf_no_longer_accepts_a_url
cargo test page:: --quiet
cargo clippy --all -- -D warnings
git diff --check
```

Expected: all commands exit 0.

Commit:

```powershell
git add src/daemon/handler/inspect.rs src/daemon/handler/mod.rs src/main.rs src/page/capture.rs src/page/find_elements.rs src/page/state.rs
git commit -m "Migrate page inspection to sessions"
```

### Task 5: Move all storage operations under sessions

**Files:**
- Modify: `src/daemon/handler/session.rs`
- Modify: `src/daemon/handler/storage.rs`
- Modify: `src/daemon/handler/mod.rs`
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: session selection and `resolve_session_target`.
- Produces: `session.storage.local.get`, `session.storage.local.set`, `session.storage.export`, and `session.storage.import`.
- Tightens: `session.cookies.*` always uses the resolved session BrowserContext.

- [ ] **Step 1: Write failing isolated-cookie scope tests**

Extract a pure cookie target decision and test it:

```rust
#[test]
fn isolated_cookie_scope_never_falls_back_to_browser() {
    let isolated = Session::new_isolated("agent".into(), "localhost:9222".into(), "CTX1".into());
    assert_eq!(cookie_scope(&isolated).unwrap(), CookieScope::BrowserContext("CTX1".into()));
}

#[test]
fn isolated_cookie_scope_requires_context_id() {
    let mut isolated = Session::new_isolated("agent".into(), "localhost:9222".into(), "CTX1".into());
    isolated.browser_context_id = None;
    let value = serde_json::to_value(cookie_scope(&isolated).unwrap_err()).unwrap();
    assert_eq!(value["error"]["code"], "CHROME_DISCONNECTED");
}
```

- [ ] **Step 2: Run cookie tests and confirm red**

Run:

```powershell
cargo test isolated_cookie_scope_never_falls_back_to_browser
cargo test isolated_cookie_scope_requires_context_id
```

Expected: FAIL because `CookieScope` and `cookie_scope` do not exist.

- [ ] **Step 3: Implement context-safe cookies**

Define:

```rust
enum CookieScope {
    BrowserContext(String),
    DefaultContext,
}

fn cookie_scope(session: &Session) -> Result<CookieScope, Response> {
    match session.mode {
        SessionMode::Default => Ok(CookieScope::DefaultContext),
        SessionMode::Isolated => session
            .browser_context_id
            .clone()
            .map(CookieScope::BrowserContext)
            .ok_or_else(|| Response::error_detail(
                ErrorCode::ChromeDisconnected,
                format!("isolated session '{}' has no BrowserContext", session.name),
                None,
            )),
    }
}
```

Use Storage-domain cookie methods with `browser_context_id` for isolated sessions. Default sessions may use the default context. Do not require an active tab for cookies and do not issue browser-wide operations for an isolated session missing its context.

- [ ] **Step 4: Add failing session storage route tests**

```rust
#[tokio::test]
async fn session_storage_routes_require_session_target() {
    let state = Arc::new(DaemonState::new());
    for cmd in [
        "session.storage.local.get",
        "session.storage.local.set",
        "session.storage.export",
        "session.storage.import",
    ] {
        let req = Request { cmd: cmd.into(), params: json!({}), token: None };
        let value = serde_json::to_value(handle_request(&req, &state, &test_context()).await).unwrap();
        assert_eq!(value["error"]["code"], "SESSION_NOT_FOUND");
    }
}
```

- [ ] **Step 5: Implement session storage CLI and handlers**

Add `SessionAction::Storage { action: SessionStorageAction }` with local get/set, export, and import. The CLI sends only canonical `session.storage.*` routes. LocalStorage uses the resolved target. Export returns `{cookies, local_storage}`; import validates both fields before mutation, writes context-scoped cookies, then writes localStorage on the resolved target.

- [ ] **Step 6: Run focused tests and commit**

Run:

```powershell
cargo test session_storage --quiet
cargo test cookies --quiet
cargo clippy --all -- -D warnings
git diff --check
```

Expected: all commands exit 0.

Commit:

```powershell
git add src/daemon/handler/session.rs src/daemon/handler/storage.rs src/daemon/handler/mod.rs src/main.rs
git commit -m "Move storage operations under sessions"
```

### Task 6: Migrate dialogs and developer CDP/network commands

**Files:**
- Modify: `src/daemon/dialog.rs`
- Modify: `src/daemon/handler/dialog.rs`
- Modify: `src/daemon/handler/debug.rs`
- Modify: `src/daemon/handler/network.rs`
- Modify: `src/daemon/handler/mod.rs`
- Modify: `src/client.rs`
- Modify: `src/main.rs`
- Modify: `src/error.rs`

**Interfaces:**
- Consumes: session-keyed dialog state and the unified resolver.
- Produces: canonical routes `dialog.list`, `dialog.accept`, `dialog.dismiss`, `dialog.policy`, `debug.cdp`, `debug.block`, and `debug.unblock`.
- Removes: monitor, HAR, event-stream placeholders, and the unused CLI streaming read loop.

- [ ] **Step 1: Write failing dialog session-key tests**

```rust
#[test]
fn dialog_state_is_scoped_by_session_and_target() {
    let state = DialogState::new();
    state.set_pending("agent", "T1", pending_dialog("confirm"));
    assert!(state.get_pending("agent", "T1").is_some());
    assert!(state.get_pending("default", "T1").is_none());
    assert_eq!(state.list_pending_for_session("agent").len(), 1);
}
```

Run `cargo test dialog_state_is_scoped_by_session_and_target`.

Expected: FAIL because the API still uses workspace naming and lookup.

- [ ] **Step 2: Implement session-native dialog handlers**

Rename workspace-oriented dialog methods to session methods. Each handler reads `session` and optional `target`, resolves the session target for accept/dismiss, requires one unambiguous pending dialog when target is omitted, and sends `Page.handleJavaScriptDialog` through the resolved CDP session. Policy is stored by session name. Responses use `session` and `target`, never `wid` or `tid`. Update `ErrorCode::DialogBlocking` guidance to `handle the dialog first: bk dialog accept or bk dialog dismiss`.

- [ ] **Step 3: Write failing canonical developer-route tests**

```rust
#[tokio::test]
async fn developer_routes_use_session_resolution() {
    let state = Arc::new(DaemonState::new());
    for cmd in ["debug.cdp", "debug.block", "debug.unblock"] {
        let req = Request { cmd: cmd.into(), params: json!({}), token: None };
        let value = serde_json::to_value(handle_request(&req, &state, &test_context()).await).unwrap();
        assert_eq!(value["error"]["code"], "SESSION_NOT_FOUND", "{cmd}");
    }
}
```

- [ ] **Step 4: Implement canonical developer commands and delete fake streams**

Move raw CDP, request block, and unblock onto the resolver. `debug.cdp` sends the supplied method and object params through the resolved target session. `debug.block` and `debug.unblock` use Network blocked URL patterns on that target. Delete `DebugAction::{Monitor, Har, Events}`, routes `network.monitor`, `network.har`, `cdp.events`, `DaemonClient::read_streaming`, and `run_streaming`; retain no stub response.

- [ ] **Step 5: Run focused tests and commit**

Run:

```powershell
cargo test dialog --quiet
cargo test developer_routes_use_session_resolution
cargo test debug --quiet
cargo test network --quiet
cargo clippy --all -- -D warnings
git diff --check
```

Expected: all commands exit 0.

Commit:

```powershell
git add src/daemon/dialog.rs src/daemon/handler/dialog.rs src/daemon/handler/debug.rs src/daemon/handler/network.rs src/daemon/handler/mod.rs src/client.rs src/main.rs src/error.rs
git commit -m "Migrate dialog and debug commands to sessions"
```

### Task 7: Make browser, daemon, status, and idle cleanup session-only

**Files:**
- Modify: `src/daemon/handler/browser.rs`
- Modify: `src/daemon/handler/daemon.rs`
- Modify: `src/daemon/handler/connect.rs`
- Modify: `src/daemon/handler/workspace.rs`
- Modify: `src/daemon/server.rs`
- Modify: `src/daemon/state.rs`
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: ownership-aware close and watcher cancellation.
- Produces: browser discovery owned by `browser.rs`.
- Produces: admin status and shutdown data containing sessions, not workspaces.

- [ ] **Step 1: Write failing admin response tests**

```rust
#[tokio::test]
async fn daemon_status_reports_sessions_without_workspaces() {
    let state = Arc::new(DaemonState::new());
    state.sessions.insert("default".into(), Session::new_default("localhost:9222".into()));
    let value = serde_json::to_value(handle_daemon_status(&state, &test_context()).await).unwrap();
    assert_eq!(value["data"]["sessions"], 1);
    assert!(value["data"].get("workspaces").is_none());
    assert!(value["data"].get("default_wid").is_none());
}

#[test]
fn browser_list_counts_sessions_by_host() {
    let state = DaemonState::new();
    let mut attached = Session::new_default("localhost:9222".into());
    attached.name = "a".into();
    state.sessions.insert("a".into(), attached);
    state.sessions.insert(
        "b".into(),
        Session::new_isolated("b".into(), "localhost:9222".into(), "CTX".into()),
    );
    assert_eq!(session_count_for_host(&state, "localhost:9222"), 2);
}
```

- [ ] **Step 2: Run admin tests and confirm red**

Run:

```powershell
cargo test daemon_status_reports_sessions_without_workspaces
cargo test browser_list_counts_sessions_by_host
```

Expected: FAIL because admin handlers still report workspace state.

- [ ] **Step 3: Move browser discovery and sessionize admin handlers**

Move `browser.discover` implementation and its tests from `handler/workspace.rs` to `handler/browser.rs`. `browser.list` reports `sessions` per host. `browser.disconnect` closes owned tabs, detaches attached tabs, disposes isolated BrowserContexts, cancels the host watcher, removes the browser, and marks sessions disconnected without touching workspace state.

- [ ] **Step 4: Make daemon stop and status session-only**

`daemon.status` returns daemon uptime, browser count, session count, request count, and limits. Task 8 adds migration report data after the report type exists. `daemon.stop` performs ownership-aware session cleanup, cancels target watchers, persists final state, and then triggers shutdown. Update top-level `bk status` to call canonical session and browser status only; remove `ws.list` and `ws.default` requests.

- [ ] **Step 5: Remove workspace idle cleanup from the server loop**

Keep the existing session idle cleanup, but ensure it uses ownership-aware close and disposes BrowserContexts only for isolated sessions. Delete spawning and invocation of `cleanup_expired_workspaces`; retain the function temporarily only if a still-compiled legacy handler test references it, then remove it in Task 10.

- [ ] **Step 6: Run focused tests and commit**

Run:

```powershell
cargo test daemon_status --quiet
cargo test browser_list --quiet
cargo test browser_disconnect --quiet
cargo test cleanup_expired_sessions --quiet
cargo clippy --all -- -D warnings
git diff --check
```

Expected: all commands exit 0.

Commit:

```powershell
git add src/daemon/handler/browser.rs src/daemon/handler/daemon.rs src/daemon/handler/connect.rs src/daemon/handler/workspace.rs src/daemon/server.rs src/daemon/state.rs src/main.rs
git commit -m "Make runtime administration session-only"
```

### Task 8: Upgrade persistence to schema v3 with one-way v2 migration

**Files:**
- Create: `src/daemon/persist/migrate_v2.rs`
- Create: `src/daemon/persist/fixtures/state-v2-mixed.json`
- Modify: `src/daemon/persist.rs`
- Modify: `src/daemon/state.rs`
- Modify: `src/daemon/server.rs`
- Modify: `src/daemon/handler/daemon.rs`

**Interfaces:**
- Produces: `PersistedStateV3`, `PersistedSessionV3`, `PersistedSessionTabV3`, `MigrationReport`, and `LoadStateResult`.
- Produces: `migrate_v2_json(content) -> Result<(PersistedStateV3, MigrationReport), MigrationError>` and `load_state_from_path(path) -> Result<LoadStateResult, MigrationError>`.
- Removes workspace fields from every v3 write.

- [ ] **Step 1: Add the mixed v2 fixture and failing conversion test**

The fixture must contain: one existing session named `agent`, one isolated workspace, two attached workspaces on the default browser with one duplicate target, and one attached workspace on a conflicting host. Add:

```rust
#[test]
fn mixed_v2_state_migrates_deterministically() {
    let input = include_str!("fixtures/state-v2-mixed.json");
    let (state, report) = migrate_v2_json(input).unwrap();
    assert_eq!(state.version, 3);
    assert!(state.sessions.iter().any(|s| s.name == "agent"));
    assert!(state.sessions.iter().any(|s| s.name.starts_with("legacy-")));
    assert_eq!(report.isolated_workspaces_migrated, 1);
    assert_eq!(report.duplicate_targets_dropped, 1);
    assert_eq!(report.conflicting_hosts_dropped, 1);
    let value = serde_json::to_value(&state).unwrap();
    assert!(value.get("workspaces").is_none());
    assert!(value.get("default_ws").is_none());
}
```

- [ ] **Step 2: Run the migration test and confirm red**

Run: `cargo test mixed_v2_state_migrates_deterministically`

Expected: FAIL because the v3 and migration types do not exist.

- [ ] **Step 3: Implement isolated v2 structs and deterministic conversion**

Move `PersistedWorkspace` and v2 top-level layout into `migrate_v2.rs`; runtime files must not import `Workspace`. Define:

```rust
#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    #[error("invalid v2 state: {0}")]
    InvalidState(String),
    #[error("failed to back up v2 state: {0}")]
    Backup(std::io::Error),
    #[error("failed to write v3 state: {0}")]
    Write(std::io::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MigrationReport {
    pub source_version: u32,
    pub backup_path: Option<String>,
    pub existing_sessions_preserved: usize,
    pub isolated_workspaces_migrated: usize,
    pub attached_tabs_merged: usize,
    pub duplicate_targets_dropped: usize,
    pub conflicting_hosts_dropped: usize,
    pub warnings: Vec<String>,
}

pub fn migrate_v2_json(
    content: &str,
) -> Result<(PersistedStateV3, MigrationReport), MigrationError>;

pub fn load_state_from_path(path: &Path) -> Result<LoadStateResult, MigrationError>;
```

Existing sessions win name and target conflicts. Isolated workspaces use names formed from `legacy-` plus the first eight workspace-ID characters, with numeric suffixes on collision. Attached tabs merge only into the default session for the same host. A legacy tab with `managed: true` becomes `Owned`; `managed: false` becomes `Attached`. Every drop increments a counter and adds a stable warning.

- [ ] **Step 4: Write failing backup and corrupt-state tests**

```rust
#[test]
fn v2_load_creates_backup_before_v3_write() {
    let dir = tempfile::tempdir().unwrap();
    let state_path = dir.path().join("state.json");
    std::fs::write(&state_path, include_str!("fixtures/state-v2-mixed.json")).unwrap();
    let loaded = load_state_from_path(&state_path).unwrap();
    assert_eq!(loaded.state.version, 3);
    assert!(dir.path().join("state.v2.backup.json").exists());
}

#[test]
fn corrupt_state_is_preserved_and_disables_persistence() {
    let dir = tempfile::tempdir().unwrap();
    let state_path = dir.path().join("state.json");
    std::fs::write(&state_path, "{not-json").unwrap();
    let loaded = load_state_from_path(&state_path).unwrap();
    assert!(loaded.persist_disabled);
    assert_eq!(std::fs::read_to_string(state_path).unwrap(), "{not-json");
}
```

- [ ] **Step 5: Implement v3 load, write, and restore**

Define:

```rust
pub struct LoadStateResult {
    pub state: PersistedStateV3,
    pub persist_disabled: bool,
    pub migration_report: Option<MigrationReport>,
}
```

Load behavior is: parse top-level version first; v3 loads directly; v2 is backed up then converted and atomically written as v3; newer versions return empty runtime state with persistence disabled; malformed files remain untouched and persistence is disabled. V3 persistence serializes browsers, sessions, tab ownership, and optional migration report only. Restore starts one target watcher per restored browser, reattaches known targets, recreates console/dialog subscriptions, and marks sessions disconnected when their browser or BrowserContext cannot be restored. Store the report in `DaemonState` and include it in `daemon.status` under `migration`.

- [ ] **Step 6: Prove v3 contains no workspace fields**

Add:

```rust
#[test]
fn persisted_v3_has_no_workspace_fields() {
    let state = DaemonState::new();
    let json = serde_json::to_value(build_persisted_state(&state)).unwrap();
    assert_eq!(json["version"], 3);
    assert!(json.get("workspaces").is_none());
    assert!(json.get("default_ws").is_none());
}
```

- [ ] **Step 7: Run persistence tests and commit**

Run:

```powershell
cargo test daemon::persist --quiet
cargo test mixed_v2_state_migrates_deterministically
cargo test persisted_v3_has_no_workspace_fields
cargo clippy --all -- -D warnings
git diff --check
```

Expected: all commands exit 0.

Commit:

```powershell
git add src/daemon/persist.rs src/daemon/persist/migrate_v2.rs src/daemon/persist/fixtures/state-v2-mixed.json src/daemon/state.rs src/daemon/server.rs src/daemon/handler/daemon.rs
git commit -m "Migrate persisted state to session-only v3"
```

### Task 9: Collapse CLI and daemon dispatch to canonical routes

**Files:**
- Modify: `src/main.rs`
- Modify: `src/daemon/handler/mod.rs`
- Modify: `src/daemon/protocol.rs`

**Interfaces:**
- Consumes: all canonical handlers from Tasks 3 through 8.
- Produces: the final supported CLI and daemon route sets.
- Removes: workspace resolution and every compatibility alias.

- [ ] **Step 1: Add failing CLI removal tests**

```rust
#[test]
fn cli_rejects_all_removed_workspace_surfaces() {
    for args in [
        &["bk", "ws", "list"][..],
        &["bk", "tab", "list"][..],
        &["bk", "fetch", "https://example.com"][..],
        &["bk", "storage", "export"][..],
        &["bk", "debug", "monitor"][..],
        &["bk", "debug", "har", "https://example.com"][..],
        &["bk", "debug", "events"][..],
        &["bk", "--ws", "abc", "snapshot"][..],
    ] {
        assert!(try_parse(args).is_err(), "removed command parsed: {args:?}");
    }
}
```

- [ ] **Step 2: Add failing daemon route removal tests**

```rust
#[tokio::test]
async fn removed_route_families_are_unknown() {
    let state = Arc::new(DaemonState::new());
    for cmd in [
        "v2.connect", "v2.open", "v2.snapshot", "v2.act", "v2.navigate",
        "ws.list", "tab.list", "nav.goto", "page.html", "page.pdf",
        "storage.local.get", "network.monitor", "network.har", "cdp.events",
    ] {
        let req = Request { cmd: cmd.into(), params: json!({}), token: None };
        let value = serde_json::to_value(handle_request(&req, &state, &test_context()).await).unwrap();
        assert_eq!(value["error"], format!("unknown command: {cmd}"));
    }
}
```

- [ ] **Step 3: Run removal tests and confirm red**

Run:

```powershell
cargo test cli_rejects_all_removed_workspace_surfaces
cargo test removed_route_families_are_unknown
```

Expected: FAIL because legacy commands and aliases still parse or dispatch.

- [ ] **Step 4: Remove legacy CLI variants and request builders**

Delete `WsAction`, `TabAction`, top-level `Ws`, `Tab`, `Fetch`, and `Storage`, the legacy status aggregator, `resolve_workspace`, one-shot workspace PDF/fetch helpers, `BK_WS` handling, and all dispatch arms that send workspace routes. Keep canonical primary, session, dialog, browser, daemon, and developer commands. Update grouped help so it contains no Legacy section.

- [ ] **Step 5: Reduce the daemon route table to canonical commands**

Keep exactly the canonical primary, inspect, session storage, dialog, browser, daemon, and debug routes. Remove `v2.*` alternatives and all old route families. Ensure the unknown-command response remains JSON and contains the exact rejected command.

- [ ] **Step 6: Verify CLI/help and commit**

Run:

```powershell
cargo test --bin bk cli --quiet
cargo test removed_route_families_are_unknown
cargo run --quiet -- --help
cargo run --quiet -- session --help
cargo run --quiet -- debug --help
git diff --check
```

Expected: tests pass; help contains no `Legacy`, `ws`, `tab`, `fetch`, `monitor`, `har`, or `events` command entries.

Commit:

```powershell
git add src/main.rs src/daemon/handler/mod.rs src/daemon/protocol.rs
git commit -m "Remove legacy CLI and daemon routes"
```

### Task 10: Delete the workspace runtime and obsolete configuration

**Files:**
- Delete: `src/workspace/mod.rs`
- Delete: `src/daemon/auto_attach.rs`
- Delete: `src/daemon/handler/workspace.rs`
- Delete: `src/daemon/handler/tab.rs`
- Delete: `src/daemon/handler/nav.rs`
- Delete: `src/daemon/handler/page.rs`
- Modify: `src/lib.rs`
- Modify: `src/daemon/mod.rs`
- Modify: `src/daemon/state.rs`
- Modify: `src/daemon/server.rs`
- Modify: `src/daemon/handler/browser.rs`
- Modify: `src/daemon/handler/daemon.rs`
- Modify: `src/daemon/persist.rs`
- Modify: `src/config.rs`
- Modify: `src/error.rs`
- Modify: `src/page/mod.rs`

**Interfaces:**
- Consumes: all session-native replacements from Tasks 1 through 9.
- Produces: no compiled workspace type, field, route, config, or error.

- [ ] **Step 1: Establish the removal scan before deletion**

Run:

```powershell
rg -n '\bWorkspace\b|WorkspaceMode|workspaces|default_wid|resolve_wid|BK_WS|\bwid\b|max_workspaces|max_tabs_per_workspace|workspace_timeout_minutes|auto_attach_tasks' src Cargo.toml
```

Expected: matches remain in the files listed for deletion and cleanup; save the output in the task notes so each surviving match is accounted for.

- [ ] **Step 2: Remove workspace fields and configuration**

Delete `DaemonState.workspaces`, `default_wid`, and `auto_attach_tasks`; retain `target_watchers`. Remove `Wid`, `resolve_wid`, workspace timeout cleanup, `daemon.workspace_timeout_minutes`, `limits.max_workspaces`, `limits.max_tabs_per_workspace`, and workspace-related `BkError`/`ErrorCode` variants. Update defaults and config serialization tests so unknown historical workspace keys are ignored by serde rather than represented in runtime config.

- [ ] **Step 3: Delete obsolete modules and exports**

Delete the six listed files and remove their `mod`/`pub mod` declarations. Remove the legacy `Tab` runtime struct from `src/page/mod.rs` after moving any still-used `ConsoleEntry` definition and console-log constructor to `src/daemon/console.rs`. Keep page-layer capture, interaction, navigation, state, find, diff, and wait modules.

- [ ] **Step 4: Make all cleanup ownership-aware and session-only**

Search every use of `Target.closeTarget`, `Target.detachFromTarget`, and BrowserContext disposal. Each must derive its action from `SessionTab.ownership` and `Session.mode`. Explicit browser disconnect, daemon stop, session close, idle expiration, and target destruction must share the same close helper rather than duplicate policy.

- [ ] **Step 5: Run the zero-workspace scan**

Run:

```powershell
rg -n '\bWorkspace\b|WorkspaceMode|workspaces|default_wid|resolve_wid|BK_WS|\bwid\b|max_workspaces|max_tabs_per_workspace|workspace_timeout_minutes|auto_attach_tasks|"(ws|tab|nav|page)\.' src Cargo.toml -g '!daemon/persist/migrate_v2.rs' -g '!daemon/persist/fixtures/**'
```

Expected: no matches. The isolated migration file may contain serialized field names `workspaces`, `default_ws`, and `wid`; verify those separately with:

```powershell
rg -n 'workspaces|default_ws|\bwid\b' src/daemon/persist/migrate_v2.rs src/daemon/persist/fixtures
```

Expected: matches only in v2 migration structs and fixtures.

- [ ] **Step 6: Run full Rust verification and commit**

Run:

```powershell
cargo fmt --all -- --check
cargo test --quiet
cargo clippy --all -- -D warnings
git diff --check
```

Expected: all commands exit 0.

Commit:

```powershell
git add src/lib.rs src/daemon src/config.rs src/error.rs src/page/mod.rs src/workspace
git commit -m "Delete the workspace runtime"
```

### Task 11: Align documentation and run release-grade acceptance checks

**Files:**
- Modify: `README.md`
- Modify: `AGENTS.md`
- Modify: `docs/REDESIGN.md`
- Modify: `docs/ROADMAP.md`
- Modify: `docs/project-analysis.md`
- Modify: `docs/connect-existing-chrome.md`
- Modify: `docs/v1-legacy-retention-report.md`
- Modify: `docs/bk-browser/SKILL.md`
- Modify: `docs/bk-browser/references/commands.md`
- Modify: `docs/bk-browser.zip`

**Interfaces:**
- Consumes: the final session-only CLI and runtime.
- Produces: one current product definition and a reproducible real-Chrome acceptance record.

- [ ] **Step 1: Update all current documentation sources**

Document browserkit as a persistent browser runtime above cdpkit-rs. List the canonical agent, session storage, admin, and developer commands. Add the breaking migration note: workspace commands, variables, fields, and daemon routes are removed; v2 state is backed up and migrated to v3; partial drops appear in `bk status`. Mark the old retention report as historical and resolved rather than leaving recommendations in present tense.

- [ ] **Step 2: Update the bundled skill archive**

Rebuild `docs/bk-browser.zip` from `docs/bk-browser/` after command references and skill instructions are updated. Verify archive contents:

```powershell
tar -tf docs/bk-browser.zip
```

Expected: archive contains `bk-browser/SKILL.md` and `bk-browser/references/commands.md` with no duplicate top-level directory.

- [ ] **Step 3: Run repository-wide legacy and boundary scans**

Run:

```powershell
rg -n 'Browser automation CLI|BK_WS|--ws|\bws\.|\btab\.|\bnav\.|\bpage\.|v2\.|default workspace|workspace-first' README.md AGENTS.md docs src
rg -n 'click\(|snapshot|browser automation|Workspace' D:/Program/cdp/cdpkit-rs/cdpkit/src D:/Program/cdp/cdpkit-rs/README.md
```

Expected: browserkit matches exist only in the approved design, implementation plan, historical archive/retention context, and v2 migration fixture/code. cdpkit-rs contains protocol/session concepts but no browserkit runtime API additions.

- [ ] **Step 4: Run automated verification**

Run:

```powershell
cargo fmt --all -- --check
cargo test --quiet
cargo clippy --all -- -D warnings
git diff --check
cargo run --quiet -- --help
```

Expected: all commands exit 0; top-level help presents only the approved session runtime plus admin/developer groups.

- [ ] **Step 5: Run real Chrome acceptance scenarios**

With a Chrome instance exposing CDP and at least one user-opened tab whose URL or title is unique, execute this PowerShell flow and record the JSON assertions:

```powershell
$null = cargo run --quiet -- connect | ConvertFrom-Json

$pattern = Read-Host 'Unique URL or title substring for an existing user tab'
$attached = cargo run --quiet -- attach $pattern | ConvertFrom-Json
$attachedTarget = $attached.data.target
$null = cargo run --quiet -- --target $attachedTarget close | ConvertFrom-Json
$reattached = cargo run --quiet -- attach $pattern | ConvertFrom-Json
if ($reattached.data.target -ne $attachedTarget) { throw 'attached target was closed' }
$null = cargo run --quiet -- --target $attachedTarget close | ConvertFrom-Json

$owned = cargo run --quiet -- open https://example.com | ConvertFrom-Json
$ownedTarget = $owned.data.target
$null = cargo run --quiet -- --target $ownedTarget close | ConvertFrom-Json
$tabsAfterClose = cargo run --quiet -- tabs | ConvertFrom-Json
if ($tabsAfterClose.data.tabs.target -contains $ownedTarget) { throw 'owned target survived close' }

$newTabPage = 'data:text/html,<a href="https://example.com" target="_blank">open</a>'
$opener = cargo run --quiet -- open $newTabPage | ConvertFrom-Json
$snapshot = cargo run --quiet -- --target $opener.data.target snapshot --full | ConvertFrom-Json
$linkRef = ($snapshot.data.elements | Where-Object { $_.tag -eq 'a' } | Select-Object -First 1).ref
$click = cargo run --quiet -- --target $opener.data.target act click --ref $linkRef | ConvertFrom-Json
if (-not $click.data.new_tab) { throw 'click did not report new_tab' }

$null = cargo run --quiet -- navigate https://example.com | ConvertFrom-Json
$cookieFile = Join-Path $env:TEMP 'bk-cookie-isolation.json'
'[{"name":"bk_scope_probe","value":"default","domain":"example.com","path":"/"}]' | Set-Content -LiteralPath $cookieFile
$null = cargo run --quiet -- session cookies set --file $cookieFile | ConvertFrom-Json
$null = cargo run --quiet -- --session isolated-check connect | ConvertFrom-Json
$null = cargo run --quiet -- --session isolated-check open https://example.com | ConvertFrom-Json
$isolatedCookies = cargo run --quiet -- --session isolated-check session cookies get | ConvertFrom-Json
if ($isolatedCookies.data.cookies.name -contains 'bk_scope_probe') { throw 'default cookie leaked into isolated session' }
Remove-Item -LiteralPath $cookieFile

$status = cargo run --quiet -- status | ConvertFrom-Json
if ($status.data.PSObject.Properties.Name -contains 'workspaces') { throw 'status still exposes workspaces' }
```

Success criteria: the script reaches the end without throwing; attached close leaves the user target visible in Chrome; owned close removes its target; click reports `new_tab`; isolated cookies do not leak from default; status contains sessions and migration metadata but no workspace fields. Do not commit machine-specific outputs or profile data.

- [ ] **Step 6: Commit documentation and close the roadmap phase**

Mark browserkit roadmap Phases 1 through 3 complete only if all automated and real-browser checks pass. Record any deferred feature as a new Phase 5 item rather than retaining dead compatibility code.

Commit:

```powershell
git add README.md AGENTS.md docs
git commit -m "Document the session-only browser runtime"
```

The implementation is complete only when the zero-workspace runtime scan passes, schema v3 writes no workspace fields, attached targets survive cleanup, all retained capabilities are session-native, automated checks pass, and the real Chrome acceptance scenarios meet their success criteria.
