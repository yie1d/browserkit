# Browserkit Clean-Slate Runtime Cleanup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (- [ ]) syntax for tracking.

**Goal:** Replace the unreleased migration and managed-browser architecture with one canonical Session runtime, schema v1 persistence, and current-only APIs.

**Architecture:** The daemon connects to an already-running CDP endpoint and stores only disconnected-restorable Session metadata. Persistence accepts exactly schema v1 and fails closed for malformed or unsupported files without conversion. Browser connection deduplication remains, while process ownership, compatibility inputs, versioned names, and migration reports are removed.

**Tech Stack:** Rust 2021, Tokio, DashMap, Serde/serde_json, clap, cdpkit 0.5.0, cargo test.

## Global Constraints

- Work only in D:/Program/cdp/browserkit/.worktrees/fix-runtime-hardening on branch codex/fix-runtime-hardening.
- Do not connect to, launch, close, or terminate the user's normal Chrome or Edge.
- Do not add dependencies or change cdpkit 0.5.0.
- State schema version 1 contains only version and sessions.
- Do not add backup, conversion, migration-report, deprecated-input, or managed-browser compatibility.
- Browserkit connects to an already-running CDP endpoint and never owns the browser process.
- Keep browser connection creation serialized with browser_connect_lock; do not hold DashMap guards across await.
- Preserve atomic writes, 500 ms debounce, transient write retries, future-version fail-closed behavior, Session tab ownership, and structured cleanup errors.
- Stage only files named by the current task. Existing Cargo.toml and Cargo.lock line-ending noise is not part of this work.

## File Structure

- src/daemon/persist.rs owns the only current state schema, strict loading, atomic writing, debounce, and disconnected Session restore.
- src/daemon/state.rs owns live daemon state, connection locks, Browser, and persistence health.
- src/browser/mod.rs owns CDP connection establishment and reuse without process metadata.
- src/daemon/handler/browser.rs and daemon.rs own public browser and daemon JSON.
- src/main.rs owns canonical clap command variants and request mapping.
- src/page/element_ref.rs and interaction.rs own current element-ref-only targeting.
- src/daemon/handler/act.rs, debug.rs, network.rs, and mod.rs own canonical routes without compatibility wrappers.
- README.md, AGENTS.md, CHANGELOG.md, and docs describe only the current runtime.

---

### Task 1: Replace migration persistence with strict schema v1

**Files:**
- Modify: src/daemon/persist.rs
- Modify: src/daemon/state.rs
- Modify: src/daemon/handler/daemon.rs
- Modify: src/daemon/mod.rs
- Delete: src/daemon/persist/migrate_v2.rs
- Delete: src/daemon/persist/fixtures/state-v2-mixed.json

**Interfaces:**
- Produces: PersistedStateV1 { version: u32, sessions: Vec<PersistedSessionV1> }.
- Produces: load_state_from_path(path: &Path) -> LoadStateResult.
- Produces: prepare_restore_into_state(state: &Arc<DaemonState>) with no restore plan or network work.
- Removes: MigrationReport, migration_report, RestorePlan, execute_restore_plan, restore_into_state, and restore_state.

- [ ] **Step 1: Add failing schema-v1 and strict-loader tests**

Replace migration-oriented tests with:

~~~rust
#[test]
fn persisted_state_is_schema_v1_and_session_only() {
    let state = DaemonState::new();
    state.sessions.insert(
        "default".into(),
        Session::new_default("127.0.0.1:9222".into()),
    );
    let json = serde_json::to_value(build_persisted_state(&state)).unwrap();
    assert_eq!(json["version"], 1);
    assert!(json.get("sessions").is_some());
    assert!(json.get("browsers").is_none());
    assert!(json.get("migration").is_none());
}

#[test]
fn unsupported_state_version_is_disabled_without_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let original = r#"{"version":2,"sessions":[]}"#;
    std::fs::write(&path, original).unwrap();
    let loaded = load_state_from_path(&path);
    assert!(loaded.persist_disabled);
    assert!(loaded.persist_disabled_reason.unwrap().contains("unsupported state version 2"));
    assert_eq!(std::fs::read_to_string(path).unwrap(), original);
}

#[test]
fn schema_v1_rejects_removed_browser_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    std::fs::write(&path, r#"{"version":1,"sessions":[],"browsers":[]}"#).unwrap();
    let loaded = load_state_from_path(&path);
    assert!(loaded.persist_disabled);
    assert!(loaded.persist_disabled_reason.unwrap().contains("schema v1"));
}
~~~

Replace daemon_status_exposes_migration_report with:

~~~rust
#[tokio::test]
async fn daemon_status_has_no_migration_surface() {
    let state = Arc::new(DaemonState::new());
    let value = serde_json::to_value(handle_daemon_status(&state, &test_context()).await).unwrap();
    assert!(value["data"].get("migration").is_none());
    assert!(value["data"].get("persistence").is_some());
}
~~~

- [ ] **Step 2: Run new tests and verify red**

~~~powershell
cargo test persisted_state_is_schema_v1_and_session_only
cargo test unsupported_state_version_is_disabled_without_rewrite
cargo test schema_v1_rejects_removed_browser_metadata
cargo test daemon_status_has_no_migration_surface
~~~

Expected: FAIL because current code writes version 3 and exposes migration.

- [ ] **Step 3: Implement strict schema v1 in persist.rs**

Use current-only types:

~~~rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PersistedStateV1 {
    pub version: u32,
    pub sessions: Vec<PersistedSessionV1>,
}

impl PersistedStateV1 {
    pub const CURRENT_VERSION: u32 = 1;
    pub fn empty() -> Self {
        Self { version: Self::CURRENT_VERSION, sessions: Vec::new() }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoadStateResult {
    pub state: PersistedStateV1,
    pub persist_disabled: bool,
    pub persist_disabled_reason: Option<String>,
}
~~~

Implement load_state_from_path so NotFound returns empty enabled state; malformed JSON, missing/non-numeric version, versions other than 1, and schema-v1 deserialization errors return an empty disabled result. It must never write or rename a file.

Rename PersistedSessionV3 and PersistedSessionTabV3 to V1. Add serde(deny_unknown_fields) to persisted state/session/tab structs. Keep Session conversion and deterministic sorting. build_persisted_state becomes:

~~~rust
pub fn build_persisted_state(state: &DaemonState) -> PersistedStateV1 {
    let mut sessions: Vec<PersistedSessionV1> = state.sessions.iter()
        .map(|entry| PersistedSessionV1::from_session(entry.value()))
        .collect();
    sessions.sort_by(|left, right| left.name.cmp(&right.name));
    PersistedStateV1 { version: PersistedStateV1::CURRENT_VERSION, sessions }
}
~~~

prepare_restore_into_state loads Sessions, calls mark_disconnected on every Session, and inserts them before readiness. Delete managed-browser restore, stale profile cleanup, and all migration functions.

- [ ] **Step 4: Remove migration state and startup background restore**

Remove MigrationReport import/field/init from state.rs and migration JSON from daemon.rs. Replace the plan/background sequence in daemon/mod.rs with:

~~~rust
persist::prepare_restore_into_state(&state);
~~~

Delete migrate_v2.rs, its fixture, and migration-only tests.

- [ ] **Step 5: Run focused tests**

~~~powershell
cargo test daemon::persist --lib
cargo test daemon::handler::daemon --lib
~~~

Expected: PASS; no test creates backups or reports migration.

- [ ] **Step 6: Commit**

~~~powershell
git add src/daemon/persist.rs src/daemon/state.rs src/daemon/handler/daemon.rs src/daemon/mod.rs src/daemon/persist/migrate_v2.rs src/daemon/persist/fixtures/state-v2-mixed.json
git commit -m "refactor: define clean-slate state schema"
~~~

### Task 2: Remove the managed-browser process model

**Files:**
- Modify: src/daemon/state.rs
- Modify: src/browser/mod.rs
- Modify: src/daemon/handler/browser.rs
- Modify: src/daemon/handler/connect.rs
- Modify: src/daemon/mod.rs

**Interfaces:**
- Produces: Browser { host: String, cdp: Arc<CDP> }.
- Produces: get_or_connect_browser_with_url(self: &Arc<Self>, key: &str, connect_target: Option<&str>) -> Result<Arc<CDP>, BkError>.
- Produces: get_or_connect_browser(self: &Arc<Self>, host: &str) -> Result<Arc<CDP>, BkError>.
- Produces: browser_summary(host: &str, sessions: usize) -> serde_json::Value for the stable list response shape.
- Removes: managed, pid, child, Browser Drop, and management-merging helpers.

- [ ] **Step 1: Add failing output/model tests**

Add to browser handler tests:

~~~rust
#[test]
fn browser_summary_omits_process_metadata() {
    let value = browser_summary("127.0.0.1:9222", 2);
    assert_eq!(value, serde_json::json!({
        "host": "127.0.0.1:9222",
        "sessions": 2,
    }));
    assert!(value.get("managed").is_none());
    assert!(value.get("pid").is_none());
}
~~~

Update the nearest test that constructs Browser with a mock CDP to use only host and cdp. Do not connect to a live browser.

- [ ] **Step 2: Run focused tests and verify red**

~~~powershell
cargo test daemon::handler::browser --lib
cargo test browser::tests --lib
~~~

Expected: compilation/test failure while Browser and responses retain process metadata.

- [ ] **Step 3: Simplify Browser and connection signatures**

~~~rust
pub struct Browser {
    pub host: String,
    pub cdp: Arc<CDP>,
}
~~~

Delete Drop. Rename browser_launch_lock to browser_connect_lock. Remove management helpers and parameters. Keep the two-stage lookup around the lock:

~~~rust
let _connect_guard = self.browser_connect_lock.lock().await;
if let Some(browser) = self.browsers.get(key) {
    let cdp = Arc::clone(&browser.cdp);
    drop(browser);
    ensure_target_watcher(self, key, Arc::clone(&cdp));
    return Ok(cdp);
}
~~~

Insert Browser { host: key.to_string(), cdp: Arc::clone(&cdp) }. Update connect.rs/browser.rs call sites to pass only key and optional target.

- [ ] **Step 4: Remove process metadata from JSON and comments**

Connect/discover success data retains host, browser_status, status, session, tabs, and ws_path where applicable. Browser list entries contain host and sessions only. Disconnect/shutdown comments state that Browser holds only CDP state and cannot terminate the external process.

Use one pure projection helper so the shape has a direct unit test:

~~~rust
fn browser_summary(host: &str, sessions: usize) -> serde_json::Value {
    json!({ "host": host, "sessions": sessions })
}
~~~

- [ ] **Step 5: Verify and commit**

~~~powershell
cargo test daemon::handler::browser --lib
cargo test browser::tests --lib
cargo test --all-targets --locked --no-run
git add src/daemon/state.rs src/browser/mod.rs src/daemon/handler/browser.rs src/daemon/handler/connect.rs src/daemon/mod.rs
git commit -m "refactor: remove managed browser model"
~~~

Expected: PASS and no managed/pid/child/browser_launch_lock compile references.

### Task 3: Make current names canonical

**Files:**
- Modify: src/main.rs
- Modify: src/config.rs
- Modify: src/browser/finder.rs
- Modify: src/error.rs
- Modify: src/daemon/session.rs
- Modify: src/daemon/handler/connect.rs
- Modify: src/daemon/handler/evaluate.rs
- Modify: src/daemon/handler/open.rs
- Modify: src/daemon/handler/screenshot.rs
- Modify: src/daemon/handler/session.rs
- Modify: src/daemon/handler/snapshot.rs
- Modify: src/daemon/handler/tabs.rs
- Modify: src/daemon/handler/wait.rs

**Interfaces:**
- Produces Command variants Open, Close, Screenshot, Wait, Status.
- Does not change clap spellings, daemon routes, ErrorCode values, or JSON behavior.

- [ ] **Step 1: Rename parser tests first**

~~~rust
#[test]
fn parse_open_command() {
    let cli = try_parse(["bk", "open", "https://example.com"]).unwrap();
    assert!(matches!(cli.command, Command::Open { .. }));
}

#[test]
fn parse_status_command() {
    let cli = try_parse(["bk", "status"]).unwrap();
    assert!(matches!(cli.command, Command::Status));
}
~~~

Rename parse_v2_limits tests to parse_limits tests without changing behavior.

- [ ] **Step 2: Run and verify red**

~~~powershell
cargo test parse_open_command
cargo test parse_status_command
cargo test config::tests --lib
~~~

Expected: CLI tests fail to compile until variants are renamed.

- [ ] **Step 3: Rename current code without aliases**

Use:

~~~rust
Open { url: String },
Close,
Screenshot { path: Option<String>, full_page: bool },
Wait { condition: String, timeout: Option<u64> },
Status,
~~~

Rename every match and test occurrence. Do not add duplicate variants or clap aliases. Remove V1/V2/V3, legacy-version, and workspace-replacement wording from current handler headers, config, finder, errors, and Session comments while preserving behavior.

- [ ] **Step 4: Verify and commit**

~~~powershell
cargo test --bin bk
cargo test config::tests --lib
rg -n -i "OpenV2|CloseV2|ScreenshotV2|WaitV2|StatusV2|v2 session|v2 limits|v2 structured" src
git add src/main.rs src/config.rs src/browser/finder.rs src/error.rs src/daemon/session.rs src/daemon/handler/connect.rs src/daemon/handler/evaluate.rs src/daemon/handler/open.rs src/daemon/handler/screenshot.rs src/daemon/handler/session.rs src/daemon/handler/snapshot.rs src/daemon/handler/tabs.rs src/daemon/handler/wait.rs
git commit -m "refactor: use canonical runtime names"
~~~

Expected: tests PASS and search has no unintended matches.

### Task 4: Remove compatibility-only element and route paths

**Files:**
- Modify: src/page/element_ref.rs
- Modify: src/page/interaction.rs
- Modify: src/page/mod.rs
- Modify: src/page/state.rs
- Modify: src/main.rs
- Modify: src/daemon/handler/act.rs
- Modify: src/daemon/handler/debug.rs
- Modify: src/daemon/handler/network.rs
- Modify: src/daemon/handler/mod.rs

**Interfaces:**
- ElementTarget supports Ref(i64) and Selector(String), not Index.
- Batch fill accepts ref targets only.
- Current ElementInfo JSON emits ref/type/id/aria_label explicitly, using null when unavailable.
- Canonical route handlers are called directly without wrappers.

- [ ] **Step 1: Add failing current-only tests**

~~~rust
#[test]
fn parse_element_target_does_not_accept_index() {
    assert!(parse_element_target(&serde_json::json!({"index": 3})).is_none());
}

#[test]
fn parse_element_target_accepts_ref() {
    assert!(matches!(
        parse_element_target(&serde_json::json!({"ref": 42})),
        Some(ElementTarget::Ref(42))
    ));
}
~~~

Change fill tests so numeric index syntax returns InvalidArgument and ref syntax succeeds. Add:

~~~rust
#[test]
fn element_info_emits_current_optional_fields() {
    let element = ElementInfo {
        index: 0,
        tag: "button".into(),
        text: "Save".into(),
        x: 10.0,
        y: 20.0,
        width: 80.0,
        height: 30.0,
        href: None,
        placeholder: None,
        backend_node_id: None,
        element_type: None,
        id: None,
        aria_label: None,
        ancestors: None,
        ax_role: None,
        ax_name: None,
    };
    let value = serde_json::to_value(element).unwrap();
    for key in ["ref", "type", "id", "aria_label"] {
        assert!(value.get(key).is_some(), "missing current field {key}");
    }
}
~~~

- [ ] **Step 2: Run and verify red**

~~~powershell
cargo test page::element_ref --lib
cargo test page::interaction --lib
cargo test element_info_emits_current_optional_fields --lib
~~~

Expected: FAIL because index targets parse and optional fields are omitted.

- [ ] **Step 3: Remove index resolution and fill compatibility**

~~~rust
pub enum ElementTarget {
    Ref(i64),
    Selector(String),
}
~~~

Delete resolve_by_index, resolve_by_index_js, helpers used only by them, Index match arms, index target parsing, and numeric fill parsing. Update main.rs and act.rs to use normal InvalidArgument errors, with no workspace/migration guidance.

- [ ] **Step 4: Make page-state JSON current**

Remove skip_serializing_if from ref, type, id, and aria_label so output includes values or null. Keep Option because absence is valid runtime state. Delete tests whose only purpose is accepting older missing-field JSON; retain populated/null tests. Ensure JavaScript objects parsed by get_page_state produce every current field. Do not add a compatibility deserializer.

- [ ] **Step 5: Delete compatibility wrappers**

Delete debug handle_cdp if only handle_debug_cdp is routed. Delete handle_network_block/unblock wrappers and allow(deprecated) attributes; route and tests use handle_debug_block/unblock and handle_network_watch directly. Remove unsupported_legacy_act_fields and its test. Replace historical route catalogs with canonical dispatch-boundary tests.

- [ ] **Step 6: Verify and commit**

~~~powershell
cargo test page::element_ref --lib
cargo test page::interaction --lib
cargo test page::state --lib
cargo test daemon::handler::act --lib
cargo test daemon::handler::network --lib
cargo test daemon::handler::mod --lib
git add src/page/element_ref.rs src/page/interaction.rs src/page/mod.rs src/page/state.rs src/main.rs src/daemon/handler/act.rs src/daemon/handler/debug.rs src/daemon/handler/network.rs src/daemon/handler/mod.rs
git commit -m "refactor: remove compatibility-only APIs"
~~~

Expected: PASS; no numeric ElementTarget or compatibility wrapper remains.

### Task 5: Align active documentation

**Files:**
- Modify: README.md
- Modify: AGENTS.md
- Modify: CHANGELOG.md
- Modify: docs/REDESIGN.md
- Modify: docs/ROADMAP.md
- Modify: docs/connect-existing-chrome.md
- Modify: docs/bk-browser/references/commands.md
- Modify: docs/architecture-tour.html
- Review: docs/bk-browser/SKILL.md
- Review: docs/superpowers/specs/2026-08-04-clean-slate-runtime-design.md
- Review: docs/superpowers/plans/2026-08-04-clean-slate-runtime-cleanup.md
- Delete obsolete pre-release specs/plans only when they present removed behavior as current guidance.

**Interfaces:**
- Produces one narrative: attach existing browser, Session runtime, schema v1, explicit reconnect after daemon restart.
- Removes migration instructions, managed-browser restoration, workspace history, versioned command naming, and deprecated command catalogs from active docs.

- [ ] **Step 1: Rewrite lifecycle documentation**

Use this canonical wording:

~~~text
browserkit connects to an already-running Chrome or Edge CDP endpoint. It does
not launch or manage the browser process. The daemon persists schema v1 Session
metadata in ~/.bk/state.json. After daemon restart, restored Sessions are
disconnected until bk connect binds them to a live browser again.
~~~

Document only version and sessions. Remove migration status and managed/pid response examples.

- [ ] **Step 2: Remove pre-release history from current guidance**

Remove Breaking Migration sections, old-command replacement tables, workspace-era config notes, versioned roadmap claims, and compatibility-restoration statements. CHANGELOG describes resulting current behavior, not migration instructions. AGENTS states there is no migration layer and no managed browser process. Keep current command catalog, ownership, errors, persistence health, and Chrome safety.

- [ ] **Step 3: Update architecture tour**

Change schema v3 to schema v1; remove migration panels and managed-browser debt; show restart as state.json to disconnected Session to explicit bk connect. Remove migrate_v2.rs source links. Parse inline JavaScript with Node and verify internal anchors. If file URL policy prevents browser rendering, report that visual-QA limitation.

- [ ] **Step 4: Scan and commit**

~~~powershell
rg -n -i "managed.browser|managed Chrome|browser_launch_lock|state\.v2|schema v[234]|migrate_v2|migration report|workspace runtime|OpenV2|StatusV2" README.md AGENTS.md CHANGELOG.md docs src
rg -n '"managed"|"pid"' README.md docs src/daemon/handler src/browser src/daemon/state.rs
git add README.md AGENTS.md CHANGELOG.md docs
git commit -m "docs: describe clean-slate session runtime"
~~~

Expected: no unintended current-contract matches. Review remaining matches manually; development specs may describe absent behavior, but active product docs and code may not implement or recommend it.

### Task 6: Full verification and read-only review

**Files:**
- Review only: all files changed by Tasks 1-5
- Modify only when a verification failure identifies a scoped defect

**Interfaces:**
- Consumes the complete clean-slate implementation.
- Produces evidence and a review verdict; no push or merge.

- [ ] **Step 1: Format and inspect**

~~~powershell
cargo fmt --all -- --check
git diff --check
git status --short
git diff main...HEAD --stat
~~~

Expected: checks PASS; only intended changes plus pre-existing Cargo line-ending noise appear.

- [ ] **Step 2: Run complete locked suite**

~~~powershell
cargo test --all-targets --locked
~~~

Expected: all tests PASS. Report counts printed by this run, not prior counts.

- [ ] **Step 3: Run clean-slate scans**

~~~powershell
rg -n -i "migrate_v2|MigrationReport|migration_report|PersistedStateV3|PersistedSessionV3|browser_launch_lock|\.managed|\.pid|\.child|OpenV2|CloseV2|ScreenshotV2|WaitV2|StatusV2|ElementTarget::Index|unsupported_legacy_act_fields" src
rg -n -i "schema v[234]|state\.v2\.backup|managed.browser metadata|workspace migration" README.md AGENTS.md CHANGELOG.md docs
~~~

Expected: no matches except approved development documents explaining absence.

- [ ] **Step 4: Review invariants**

Verify no guard crosses unrelated await; double-check remains under browser_connect_lock; shutdown cannot terminate a browser process; unsupported state cannot be overwritten; restored Sessions are visible and disconnected before readiness; ownership still controls detach versus close; removed inputs are not silently accepted; docs match JSON and restart behavior.

Record blocking, should-fix, and optional findings. Fix blocking/should-fix items with targeted tests.

- [ ] **Step 5: Commit fixes only when needed**

~~~powershell
git commit -m "fix: resolve clean-slate review findings"
~~~

Stage exact files first. Do not create an empty commit.

- [ ] **Step 6: Present evidence**

Report commits, commands and exact counts, scan results, deleted files, justified remaining historical terms, and unchanged Chrome safety. State that the branch is local and not pushed or merged.
