# browserkit v2 Act Parity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move all ordinary page interactions onto session-native `bk act <kind>`, then remove the matching legacy top-level commands and `act.*` daemon routes.

**Architecture:** Extend the existing `Command::Act` request builder and `daemon::handler::act` parser/dispatcher while reusing `page::interaction` for CDP work. Each batch first establishes v2 parsing and session error behavior, then removes only the legacy CLI variants and daemon routes covered by that batch. The action migration does not add workspace dependencies; click new-tab reporting remains absent from v2 and is documented as requiring session-native target lifecycle tracking.

**Tech Stack:** Rust 1.75, clap, serde_json, tokio, cdpkit 0.3.0, cargo test, cargo clippy

## Global Constraints

- `cdpkit-rs` remains the protocol layer; browser automation behavior stays in browserkit.
- Every migrated action uses `cmd = "act"` and resolves `session` plus `target` through v2 session state.
- Do not accept `index`, `wid`, or workspace aliases in the v2 handler.
- Use snapshot refs, coordinates, and selectors exactly as defined by the approved spec.
- Return structured `Response::error_detail` failures and omit legacy `wid`, `tid`, and `status` fields.
- Reuse helpers in `src/page/interaction.rs`; do not duplicate CDP command sequences in handlers.
- Every production behavior change follows red-green TDD.
- Do not stage or commit `.codex/`.

## File Map

- `src/main.rs`: clap shape, v2 JSON request construction, rejection tests for removed commands.
- `src/daemon/handler/act.rs`: v2 action parameter model, validation, execution dispatch, response data.
- `src/daemon/handler/action.rs`: legacy workspace action implementations; delete handlers after v2 parity.
- `src/daemon/handler/mod.rs`: daemon route table and unknown-command regression tests.
- `src/page/interaction.rs`: shared low-level helpers; expected to remain unchanged unless a helper cannot express the approved ref/selector shape.
- `README.md`: public action examples and explicit click `new_tab` limitation.
- `docs/bk-browser/references/commands.md`: complete v2 command reference and removed-command mappings.
- `docs/ROADMAP.md`: migration ledger updated after each batch.

---

### Task 1: Migrate scroll, hover, and focus

**Files:**
- Modify: `src/main.rs`
- Modify: `src/daemon/handler/act.rs`
- Modify: `src/daemon/handler/action.rs`
- Modify: `src/daemon/handler/mod.rs`
- Modify: `README.md`
- Modify: `docs/bk-browser/references/commands.md`
- Modify: `docs/ROADMAP.md`

**Interfaces:**
- Consumes: `scroll_page`, `scroll_to_element_by_target`, `scroll_to_element_by_selector`, `hover_by_target`, and `focus_by_target` from `page::interaction`.
- Produces: `ActKind::{Scroll, Hover, Focus}` and `ActParams::{direction, amount, selector}`; `Command::Act` forwards these fields to `cmd = "act"`.

- [ ] **Step 1: Write failing CLI and parser tests**

Add these cases to the existing test modules:

```rust
#[test]
fn cli_parses_act_scroll_hover_and_focus() {
    let scroll = try_parse(&[
        "bk", "act", "scroll", "--direction", "down", "--amount", "250",
    ]).unwrap();
    assert!(matches!(
        scroll.command,
        Command::Act { ref kind, ref direction, amount: Some(250.0), .. }
            if kind.as_deref() == Some("scroll") && direction.as_deref() == Some("down")
    ));

    let hover = try_parse(&["bk", "act", "hover", "--ref", "42"]).unwrap();
    assert!(matches!(
        hover.command,
        Command::Act { ref kind, element_ref: Some(42), .. }
            if kind.as_deref() == Some("hover")
    ));

    let focus = try_parse(&["bk", "act", "focus", "--ref", "43"]).unwrap();
    assert!(matches!(
        focus.command,
        Command::Act { ref kind, element_ref: Some(43), .. }
            if kind.as_deref() == Some("focus")
    ));
}

#[test]
fn parse_act_scroll_hover_and_focus() {
    let scroll = parse_act_params(&json!({"kind": "scroll", "direction": "down", "amount": 250.0})).unwrap();
    assert_eq!(scroll.kind, ActKind::Scroll);
    assert_eq!(scroll.direction.as_deref(), Some("down"));
    assert_eq!(scroll.amount, Some(250.0));

    let hover = parse_act_params(&json!({"kind": "hover", "ref": 42})).unwrap();
    assert_eq!(hover.kind, ActKind::Hover);

    let focus = parse_act_params(&json!({"kind": "focus", "ref": 43})).unwrap();
    assert_eq!(focus.kind, ActKind::Focus);
}

#[test]
fn parse_act_hover_and_focus_require_ref() {
    for kind in ["hover", "focus"] {
        let response = parse_act_params(&json!({"kind": kind})).unwrap_err();
        let value = serde_json::to_value(response).unwrap();
        assert_eq!(value["error"]["code"], "INVALID_ARGUMENT");
    }
}

#[test]
fn parse_act_rejects_workspace_fields() {
    for legacy_field in ["wid", "tid", "index"] {
        let mut params = json!({"kind": "click", "ref": 42});
        params[legacy_field] = json!(1);
        let value = serde_json::to_value(parse_act_params(&params).unwrap_err()).unwrap();
        assert_eq!(value["error"]["code"], "INVALID_ARGUMENT");
    }
}

#[tokio::test]
async fn handle_new_simple_actions_use_session_resolution() {
    let state = Arc::new(DaemonState::new());
    for params in [
        json!({"kind": "scroll", "direction": "down"}),
        json!({"kind": "hover", "ref": 42}),
        json!({"kind": "focus", "ref": 42}),
    ] {
        let req = Request { cmd: "act".into(), params, token: None };
        let value = serde_json::to_value(handle_act(&req, &state).await).unwrap();
        assert_eq!(value["error"]["code"], "SESSION_NOT_FOUND");
    }
}
```

- [ ] **Step 2: Run the focused tests and confirm red**

Run:

```powershell
cargo test cli_parses_act_scroll_hover_and_focus
cargo test parse_act_scroll_hover_and_focus
cargo test parse_act_hover_and_focus_require_ref
cargo test parse_act_rejects_workspace_fields
cargo test handle_new_simple_actions_use_session_resolution
```

Expected: FAIL because the CLI fields and `ActKind` variants do not exist.

- [ ] **Step 3: Implement v2 CLI fields, validation, and execution**

Add these fields to `Command::Act` and its JSON request builder:

```rust
#[arg(long)]
direction: Option<String>,
#[arg(long)]
amount: Option<f64>,
#[arg(long)]
selector: Option<String>,
```

Extend the handler model with:

```rust
pub enum ActKind {
    Click,
    Type,
    Press,
    Scroll,
    Hover,
    Focus,
}

// ActParams additions
direction: Option<String>,
amount: Option<f64>,
selector: Option<String>,
```

Before kind-specific parsing, reject requests containing `wid`, `tid`, or `index` with `INVALID_ARGUMENT`. Parse `direction`, `amount`, and `selector`; default direction to `down` only when scroll has no ref or selector. Reject unknown directions outside `up/down/left/right/top/bottom`, reject non-positive amounts, and require a ref for hover/focus. Dispatch with these complete functions:

```rust
async fn execute_scroll(
    cdp: &Arc<cdpkit::CDP>,
    session_id: &str,
    params: &ActParams,
) -> Result<ActionSuccess, Response> {
    use crate::page::element_ref::ElementTarget;
    use crate::page::interaction::{
        scroll_page, scroll_to_element_by_selector, scroll_to_element_by_target,
    };

    let result = if let Some(selector) = params.selector.as_deref() {
        scroll_to_element_by_selector(cdp, session_id, selector).await
    } else if let Some(ref_id) = params.ref_id {
        scroll_to_element_by_target(cdp, session_id, &ElementTarget::Ref(ref_id)).await
    } else {
        scroll_page(
            cdp,
            session_id,
            params.direction.as_deref().unwrap_or("down"),
            params.amount,
        ).await
    };

    result.map(|()| ActionSuccess { action: "scroll".into(), ref_id: params.ref_id })
        .map_err(|e| action_error("scroll", e))
}

async fn execute_ref_action(
    action: &'static str,
    cdp: &Arc<cdpkit::CDP>,
    session_id: &str,
    ref_id: i64,
) -> Result<ActionSuccess, Response> {
    use crate::page::element_ref::ElementTarget;
    let target = ElementTarget::Ref(ref_id);
    let result = match action {
        "hover" => crate::page::interaction::hover_by_target(cdp, session_id, &target).await,
        "focus" => crate::page::interaction::focus_by_target(cdp, session_id, &target).await,
        _ => unreachable!("validated ref action"),
    };
    result.map(|()| ActionSuccess { action: action.into(), ref_id: Some(ref_id) })
        .map_err(|e| action_error(action, e))
}
```

Define `action_error(action, error)` once in `act.rs`, mapping element-not-found errors to `REF_NOT_FOUND` and all other helper failures to `JS_ERROR`.

- [ ] **Step 4: Run focused tests and handler regression tests**

Run: `cargo test act --quiet`

Expected: PASS.

- [ ] **Step 5: Remove covered legacy surfaces and prove rejection**

Delete `Command::{Scroll, Hover, Focus}`, their dispatch arms, the three legacy handler functions in `action.rs`, and routes `act.scroll`, `act.hover`, `act.focus`. Add:

```rust
fn assert_cli_commands_removed(cases: &[&[&str]]) {
    for args in cases {
        assert!(try_parse(args).is_err(), "{args:?} should be removed");
    }
}

#[test]
fn cli_rejects_removed_scroll_hover_focus_commands() {
    assert_cli_commands_removed(&[
        &["bk", "scroll", "down"][..],
        &["bk", "hover", "--ref", "42"][..],
        &["bk", "focus", "--ref", "42"][..],
    ]);
}

async fn assert_routes_removed(commands: &[&str]) {
    let state = Arc::new(DaemonState::new());
    for cmd in commands {
        let req = Request { cmd: (*cmd).into(), params: json!({}), token: None };
        let value = serde_json::to_value(handle_request(&req, &state, &test_context()).await).unwrap();
        assert_eq!(value["error"], format!("unknown command: {cmd}"));
    }
}

#[tokio::test]
async fn dispatch_removed_scroll_hover_focus_routes_are_unknown() {
    assert_routes_removed(&["act.scroll", "act.hover", "act.focus"]).await;
}
```

- [ ] **Step 6: Update docs and commit Batch A**

Document `bk act scroll`, `bk act hover`, and `bk act focus`; move the old commands into the removed-alias table; mark Batch A complete in the roadmap.

Run: `cargo test --quiet; cargo clippy --all -- -D warnings; git diff --check`

Expected: all commands exit 0.

Commit:

```powershell
git add src/main.rs src/daemon/handler/act.rs src/daemon/handler/action.rs src/daemon/handler/mod.rs README.md docs/bk-browser/references/commands.md docs/ROADMAP.md
git commit -m "Migrate scroll hover and focus to v2 act"
```

### Task 2: Migrate select and dropdown options

**Files:**
- Modify: `src/main.rs`
- Modify: `src/daemon/handler/act.rs`
- Modify: `src/daemon/handler/action.rs`
- Modify: `src/daemon/handler/mod.rs`
- Modify: `README.md`
- Modify: `docs/bk-browser/references/commands.md`
- Modify: `docs/ROADMAP.md`

**Interfaces:**
- Consumes: `select_by_target` and `dropdown_options_by_target`.
- Produces: `ActKind::{Select, Options}`, `ActParams::value`, and action-specific response data through `ActionSuccess::data`.

- [ ] **Step 1: Add failing CLI, parser, and response tests**

```rust
#[test]
fn cli_parses_act_select_and_options() {
    let select = try_parse(&["bk", "act", "select", "--ref", "42", "--value", "green"]).unwrap();
    assert!(matches!(select.command, Command::Act { ref kind, ref value, .. }
        if kind.as_deref() == Some("select") && value.as_deref() == Some("green")));
    let options = try_parse(&["bk", "act", "options", "--ref", "42"]).unwrap();
    assert!(matches!(options.command, Command::Act { ref kind, element_ref: Some(42), .. }
        if kind.as_deref() == Some("options")));
}

#[test]
fn parse_act_select_and_options_validate_fields() {
    assert!(parse_act_params(&json!({"kind": "select", "ref": 42, "value": "green"})).is_ok());
    assert!(parse_act_params(&json!({"kind": "select", "ref": 42})).is_err());
    assert!(parse_act_params(&json!({"kind": "options", "ref": 42})).is_ok());
    assert!(parse_act_params(&json!({"kind": "options"})).is_err());
}

#[tokio::test]
async fn handle_select_and_options_use_session_resolution() {
    let state = Arc::new(DaemonState::new());
    for params in [
        json!({"kind": "select", "ref": 42, "value": "green"}),
        json!({"kind": "options", "ref": 42}),
    ] {
        let req = Request { cmd: "act".into(), params, token: None };
        let value = serde_json::to_value(handle_act(&req, &state).await).unwrap();
        assert_eq!(value["error"]["code"], "SESSION_NOT_FOUND");
    }
}
```

- [ ] **Step 2: Run tests and confirm red**

Run:

```powershell
cargo test cli_parses_act_select_and_options
cargo test parse_act_select_and_options_validate_fields
cargo test handle_select_and_options_use_session_resolution
```

Expected: FAIL because `value`, `Select`, and `Options` are absent.

- [ ] **Step 3: Implement selection and option inspection**

Add `value: Option<String>` to `Command::Act` and `ActParams`. Add `Select` and `Options` variants. Extend successful execution data:

```rust
struct ActionSuccess {
    action: String,
    ref_id: Option<i64>,
    data: serde_json::Map<String, serde_json::Value>,
}

impl ActionSuccess {
    fn completed(action: &str, ref_id: Option<i64>) -> Self {
        Self {
            action: action.into(),
            ref_id,
            data: serde_json::Map::new(),
        }
    }

    fn insert(&mut self, key: &str, value: serde_json::Value) {
        self.data.insert(key.into(), value);
    }
}
```

Replace every existing `ActionSuccess` struct literal with `ActionSuccess::completed(action, ref_id)`. `execute_select` calls `select_by_target` with `ElementTarget::Ref(ref_id)`, then inserts both `value` and returned `detail`. `execute_options` calls `dropdown_options_by_target` and inserts its `options` array. Change `build_act_response` to accept `data: serde_json::Map<String, serde_json::Value>` and merge each entry into the response object before returning.

- [ ] **Step 4: Run v2 act tests**

Run: `cargo test act --quiet`

Expected: PASS, including response assertions for `data.value`, `data.detail`, and `data.options`.

- [ ] **Step 5: Delete legacy select/options paths and add rejection tests**

Delete `Command::{Select, Options}`, dispatch arms, handlers `handle_act_select` and `handle_act_dropdown_options`, and routes `act.select` plus `act.dropdown_options`. Add:

```rust
#[test]
fn cli_rejects_removed_select_and_options_commands() {
    assert_cli_commands_removed(&[
        &["bk", "select", "--ref", "42", "green"][..],
        &["bk", "options", "--ref", "42"][..],
    ]);
}

#[tokio::test]
async fn dispatch_removed_select_and_options_routes_are_unknown() {
    assert_routes_removed(&["act.select", "act.dropdown_options"]).await;
}
```

- [ ] **Step 6: Update docs, verify, and commit Batch B**

Run: `cargo test --quiet; cargo clippy --all -- -D warnings; git diff --check`

Expected: all commands exit 0.

Commit:

```powershell
git add src/main.rs src/daemon/handler/act.rs src/daemon/handler/action.rs src/daemon/handler/mod.rs README.md docs/bk-browser/references/commands.md docs/ROADMAP.md
git commit -m "Migrate select and options to v2 act"
```

### Task 3: Migrate batch fill

**Files:**
- Modify: `src/main.rs`
- Modify: `src/daemon/handler/act.rs`
- Modify: `src/daemon/handler/action.rs`
- Modify: `src/daemon/handler/mod.rs`
- Modify: `README.md`
- Modify: `docs/bk-browser/references/commands.md`
- Modify: `docs/ROADMAP.md`

**Interfaces:**
- Consumes: `FillFieldTarget` and `fill_fields_by_target`.
- Produces: `ActKind::Fill`, `ActParams::fields: Vec<ActFillField>`, and `data.results`.

- [ ] **Step 1: Add failing ref-only fill tests**

```rust
#[test]
fn cli_parses_act_fill_sets() {
    let cli = try_parse(&[
        "bk", "act", "fill", "--set", "ref:42=alpha", "--set", "ref:55=beta",
    ]).unwrap();
    assert!(matches!(cli.command, Command::Act { ref kind, ref set, .. }
        if kind.as_deref() == Some("fill") && set.len() == 2));
}

#[test]
fn parse_act_fill_accepts_refs_and_rejects_indexes() {
    let parsed = parse_act_params(&json!({
        "kind": "fill",
        "fields": [{"ref": 42, "value": "alpha"}]
    })).unwrap();
    assert_eq!(parsed.fields, vec![ActFillField { ref_id: 42, value: "alpha".into() }]);
    assert!(parse_act_params(&json!({
        "kind": "fill",
        "fields": [{"index": 0, "value": "alpha"}]
    })).is_err());
}

#[tokio::test]
async fn handle_fill_uses_session_resolution() {
    let state = Arc::new(DaemonState::new());
    let req = Request {
        cmd: "act".into(),
        params: json!({"kind": "fill", "fields": [{"ref": 42, "value": "alpha"}]}),
        token: None,
    };
    let value = serde_json::to_value(handle_act(&req, &state).await).unwrap();
    assert_eq!(value["error"]["code"], "SESSION_NOT_FOUND");
}
```

- [ ] **Step 2: Run tests and confirm red**

Run:

```powershell
cargo test cli_parses_act_fill_sets
cargo test parse_act_fill_accepts_refs_and_rejects_indexes
cargo test handle_fill_uses_session_resolution
```

Expected: FAIL because v2 fill fields do not exist.

- [ ] **Step 3: Implement ref-only fill parsing and execution**

Add repeatable `#[arg(long = "set")] set: Vec<String>` to `Command::Act`. Parse each CLI value with `parse_fill_set_target`, reject `Index` and `Selector`, and emit `{ "ref": n, "value": value }`. Define:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
struct ActFillField {
    ref_id: i64,
    value: String,
}
```

The handler requires a non-empty `fields` array, requires numeric `ref` plus string `value` for every item, converts each item to `FillFieldTarget { target: ElementTarget::Ref(field.ref_id), value }`, calls `fill_fields_by_target`, and stores the serialized results in `ActionSuccess::data["results"]`.

- [ ] **Step 4: Remove legacy fill and verify rejection**

Delete `Command::Fill`, its dispatch arm, `handle_act_fill`, and route `act.fill`. Add:

```rust
#[test]
fn cli_rejects_removed_fill_command() {
    assert_cli_commands_removed(&[&["bk", "fill", "--set", "ref:42=value"]]);
}

#[tokio::test]
async fn dispatch_removed_fill_route_is_unknown() {
    assert_routes_removed(&["act.fill"]).await;
}
```

- [ ] **Step 5: Update docs, verify, and commit Batch C**

Run: `cargo test --quiet; cargo clippy --all -- -D warnings; git diff --check`

Expected: all commands exit 0.

Commit:

```powershell
git add src/main.rs src/daemon/handler/act.rs src/daemon/handler/action.rs src/daemon/handler/mod.rs README.md docs/bk-browser/references/commands.md docs/ROADMAP.md
git commit -m "Migrate batch fill to v2 act"
```

### Task 4: Migrate upload and drag

**Files:**
- Modify: `src/main.rs`
- Modify: `src/daemon/handler/act.rs`
- Modify: `src/daemon/handler/action.rs`
- Modify: `src/daemon/handler/mod.rs`
- Modify: `README.md`
- Modify: `docs/bk-browser/references/commands.md`
- Modify: `docs/ROADMAP.md`

**Interfaces:**
- Consumes: `upload_files_by_target`, `upload_files_by_selector`, and `drag_by_target`.
- Produces: `ActKind::{Upload, Drag}` and `ActParams::{files, from_ref, from_selector, to_ref, to_selector}`.

- [ ] **Step 1: Add failing CLI and validation tests**

```rust
#[test]
fn cli_parses_act_upload_and_drag() {
    let upload = try_parse(&["bk", "act", "upload", "--ref", "42", "a.txt", "b.txt"]).unwrap();
    assert!(matches!(upload.command, Command::Act { ref kind, ref files, .. }
        if kind.as_deref() == Some("upload") && files == &["a.txt", "b.txt"]));
    let drag = try_parse(&[
        "bk", "act", "drag", "--from-ref", "10", "--to-selector", "#drop",
    ]).unwrap();
    assert!(matches!(drag.command, Command::Act { ref kind, from_ref: Some(10), ref to_selector, .. }
        if kind.as_deref() == Some("drag") && to_selector.as_deref() == Some("#drop")));
}

#[test]
fn parse_act_upload_and_drag_require_complete_targets() {
    assert!(parse_act_params(&json!({"kind": "upload", "ref": 42, "files": ["a.txt"]})).is_ok());
    assert!(parse_act_params(&json!({"kind": "upload", "files": ["a.txt"]})).is_err());
    assert!(parse_act_params(&json!({"kind": "drag", "from_ref": 10, "to_selector": "#drop"})).is_ok());
    assert!(parse_act_params(&json!({"kind": "drag", "from_ref": 10})).is_err());
}

#[tokio::test]
async fn handle_upload_and_drag_use_session_resolution() {
    let state = Arc::new(DaemonState::new());
    for params in [
        json!({"kind": "upload", "ref": 42, "files": ["a.txt"]}),
        json!({"kind": "drag", "from_ref": 10, "to_selector": "#drop"}),
    ] {
        let req = Request { cmd: "act".into(), params, token: None };
        let value = serde_json::to_value(handle_act(&req, &state).await).unwrap();
        assert_eq!(value["error"]["code"], "SESSION_NOT_FOUND");
    }
}
```

- [ ] **Step 2: Run tests and confirm red**

Run:

```powershell
cargo test cli_parses_act_upload_and_drag
cargo test parse_act_upload_and_drag_require_complete_targets
cargo test handle_upload_and_drag_use_session_resolution
```

Expected: FAIL because the new CLI fields and kinds are absent.

- [ ] **Step 3: Implement upload and drag**

Add positional `files: Vec<String>` plus `from_ref`, `from_selector`, `to_ref`, and `to_selector` flags to `Command::Act`; forward non-empty values. In `parse_act_params`, require exactly one ref/selector source for upload, at least one file, and exactly one source plus one destination for drag. Build only `ElementTarget::Ref` or `ElementTarget::Selector`; no index fields exist. Store uploaded `files` in response data; drag returns the standard action result.

- [ ] **Step 4: Remove legacy upload/drag paths and add rejection tests**

Delete `Command::{Upload, Drag}`, their dispatch arms, `handle_act_upload`, `handle_act_drag`, and routes `act.upload`, `act.drag`. Add:

```rust
#[test]
fn cli_rejects_removed_upload_and_drag_commands() {
    assert_cli_commands_removed(&[
        &["bk", "upload", "--ref", "42", "a.txt"][..],
        &["bk", "drag", "--from-ref", "10", "--to-ref", "20"][..],
    ]);
}

#[tokio::test]
async fn dispatch_removed_upload_and_drag_routes_are_unknown() {
    assert_routes_removed(&["act.upload", "act.drag"]).await;
}
```

- [ ] **Step 5: Update docs, verify, and commit Batch D**

Run: `cargo test --quiet; cargo clippy --all -- -D warnings; git diff --check`

Expected: all commands exit 0.

Commit:

```powershell
git add src/main.rs src/daemon/handler/act.rs src/daemon/handler/action.rs src/daemon/handler/mod.rs README.md docs/bk-browser/references/commands.md docs/ROADMAP.md
git commit -m "Migrate upload and drag to v2 act"
```

### Task 5: Remove legacy keys in favor of press

**Files:**
- Modify: `src/main.rs`
- Modify: `src/daemon/handler/action.rs`
- Modify: `src/daemon/handler/mod.rs`
- Modify: `README.md`
- Modify: `docs/bk-browser/references/commands.md`
- Modify: `docs/ROADMAP.md`

**Interfaces:**
- Consumes: existing `ActKind::Press`, `Command::Act::keys`, and `dispatch_key_combo`.
- Produces: one keyboard public surface, `bk act press --keys ...`.

- [ ] **Step 1: Add failing removal tests**

```rust
#[test]
fn cli_rejects_removed_keys_command() {
    assert!(try_parse(&["bk", "keys", "Enter"]).is_err());
}

#[tokio::test]
async fn dispatch_removed_act_keys_route_is_unknown() {
    let state = Arc::new(DaemonState::new());
    let req = Request { cmd: "act.keys".into(), params: json!({"keys": ["Enter"]}), token: None };
    let value = serde_json::to_value(handle_request(&req, &state, &test_context()).await).unwrap();
    assert_eq!(value["error"], "unknown command: act.keys");
}
```

- [ ] **Step 2: Run tests and confirm red**

Run:

```powershell
cargo test cli_rejects_removed_keys_command
cargo test dispatch_removed_act_keys_route_is_unknown
```

Expected: FAIL because the old command and route still exist.

- [ ] **Step 3: Delete only the legacy wrapper**

Delete `Command::Keys`, its dispatch arm, `handle_act_keys`, and route `act.keys`. Keep `dispatch_key_combo` public within the handler module because v2 `execute_press` consumes it.

- [ ] **Step 4: Update docs, verify, and commit Batch E**

Map `bk keys Enter` to `bk act press --keys Enter` in the removed-command table.

Run: `cargo test --quiet; cargo clippy --all -- -D warnings; git diff --check`

Expected: all commands exit 0.

Commit:

```powershell
git add src/main.rs src/daemon/handler/action.rs src/daemon/handler/mod.rs README.md docs/bk-browser/references/commands.md docs/ROADMAP.md
git commit -m "Remove legacy keys action route"
```

### Task 6: Remove legacy click and type wrappers

**Files:**
- Modify: `src/main.rs`
- Modify: `src/daemon/handler/action.rs`
- Modify: `src/daemon/handler/mod.rs`
- Modify: `src/daemon/handler/act.rs`
- Modify: `README.md`
- Modify: `docs/bk-browser/references/commands.md`
- Modify: `docs/ROADMAP.md`

**Interfaces:**
- Consumes: existing v2 click/type parser and execution tests.
- Produces: no top-level `bk click`/`bk type` and no `act.click`/`act.type` routes.

- [ ] **Step 1: Lock existing v2 parity and add failing removal tests**

Retain tests for ref click, coordinate click, replacing type, append type, and structured session failures. Add:

```rust
#[test]
fn cli_rejects_removed_click_and_type_commands() {
    for args in [
        &["bk", "click", "--ref", "42"][..],
        &["bk", "type", "--ref", "42", "hello"][..],
    ] {
        assert!(try_parse(args).is_err(), "{args:?} should be removed");
    }
}

#[tokio::test]
async fn dispatch_removed_click_and_type_routes_are_unknown() {
    let state = Arc::new(DaemonState::new());
    for cmd in ["act.click", "act.type"] {
        let req = Request { cmd: cmd.into(), params: json!({}), token: None };
        let value = serde_json::to_value(handle_request(&req, &state, &test_context()).await).unwrap();
        assert_eq!(value["error"], format!("unknown command: {cmd}"));
    }
}
```

- [ ] **Step 2: Run tests and confirm red**

Run:

```powershell
cargo test cli_rejects_removed_click_and_type_commands
cargo test dispatch_removed_click_and_type_routes_are_unknown
```

Expected: FAIL because old click/type still parse and dispatch.

- [ ] **Step 3: Delete legacy click/type implementations**

Delete `Command::{Click, Type}`, their dispatch arms, `handle_click`, `handle_type`, click dialog-race types, workspace `detect_new_tab`, autocomplete-only legacy helpers, and routes `act.click`, `act.type`. Remove imports used only by those paths. Keep `dispatch_key_combo` until it can be moved into `act.rs` without broadening this commit.

- [ ] **Step 4: Record the deliberate click response difference**

README and command reference must say: v2 `bk act click` returns action result plus `state_diff`; it does not report `new_tab` until session-native target lifecycle tracking is implemented. Remove the unused `new_tab` parameter and its unit test from `build_act_response` so the v2 response contract matches reality.

- [ ] **Step 5: Verify and commit Batch F**

Run: `cargo test --quiet; cargo clippy --all -- -D warnings; git diff --check`

Expected: all commands exit 0.

Commit:

```powershell
git add src/main.rs src/daemon/handler/action.rs src/daemon/handler/mod.rs src/daemon/handler/act.rs README.md docs/bk-browser/references/commands.md docs/ROADMAP.md
git commit -m "Remove legacy click and type action routes"
```

### Task 7: Close the action migration and audit boundaries

**Files:**
- Modify: `src/daemon/handler/action.rs`
- Modify: `docs/ROADMAP.md`
- Test: repository-wide scans and Rust verification

**Interfaces:**
- Consumes: all prior action migration commits.
- Produces: `dispatch_key_combo` owned by `act.rs` and deletion of the legacy `action.rs` module.

- [ ] **Step 1: Scan for legacy action reachability**

Run:

```powershell
rg -n 'Command::(Click|Type|Fill|Select|Scroll|Hover|Focus|Upload|Drag|Keys|Options)|"act\.(click|type|fill|select|scroll|hover|focus|upload|drag|keys|dropdown_options)"' src
```

Expected: no matches.

- [ ] **Step 2: Move the remaining keyboard helper and delete action.rs**

Move `dispatch_key_combo` and its key-mapping tests into `act.rs`, update `execute_press` to call it directly, remove `mod action;`, and delete `src/daemon/handler/action.rs`. Confirm deletion with `rg -n 'action::|mod action' src`, which must return no matches.

- [ ] **Step 3: Verify the public CLI surface**

Run:

```powershell
cargo run --quiet -- act --help
cargo run --quiet -- --help
```

Expected: `act --help` lists all approved kinds and flags; top-level help contains no migrated action commands.

- [ ] **Step 4: Run final repository verification**

Run: `cargo test --quiet; cargo clippy --all -- -D warnings; git diff --check`

Expected: all commands exit 0.

- [ ] **Step 5: Commit cleanup only if files changed**

```powershell
git add src/daemon/handler/act.rs src/daemon/handler/action.rs src/daemon/handler/mod.rs docs/ROADMAP.md
git commit -m "Finish v2 action route cleanup"
```

The implementation is complete when the legacy route scan is empty, the public help exposes only `bk act`, and all verification commands pass.
