# Clean-Slate Follow-up Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the remaining clean-slate gaps by rejecting semantically invalid state and non-canonical request fields, removing obsolete architecture artifacts, and aligning terminology and regression coverage.

**Architecture:** Persistence keeps schema v1 but adds a pure semantic validator before any state reaches `DaemonState`; failures use the existing fail-closed path. Request dispatch adds one canonical command-field contract before handlers run, while `act` keeps its action-specific validation. Documentation removes artifacts that claim the old runtime is current and consistently describes browser connections rather than browser-process management.

**Tech Stack:** Rust 1.75, Serde JSON, Tokio, Clap, cargo test/clippy/fmt.

## Global Constraints

- Do not add migration, conversion, deprecated-input, or compatibility behavior.
- Do not launch, connect to, close, or terminate Chrome during validation.
- Keep `cdpkit = "0.5.0"` and add no dependencies.
- Keep changes uncommitted unless the user explicitly requests another commit.

---

### Task 1: Reject semantically invalid schema v1 state

**Files:**
- Modify: `src/daemon/persist.rs`

**Interfaces:**
- Produces: `validate_persisted_state(state: &PersistedStateV1) -> Result<(), String>` used by `load_state_from_path`.

- [x] Add failing tests for duplicate Session names, duplicate tab IDs, invalid default/named mode combinations, missing isolated context, attached tabs in isolated Sessions, missing active targets, and empty identifiers/host.
- [x] Run `cargo test daemon::persist::tests::semantic_ --locked` and confirm the new tests fail because invalid state is accepted.
- [x] Implement `validate_persisted_state` and route every failure through `disabled_empty_result("state.json is not valid schema v1: ...")`.
- [x] Re-run the targeted persistence tests and confirm they pass.

### Task 2: Enforce canonical request fields

**Files:**
- Modify: `src/daemon/handler/mod.rs`
- Modify only if required for exact current contracts: handlers under `src/daemon/handler/`

**Interfaces:**
- Produces: `validate_request_fields(req: &Request) -> Result<(), Response>` called before session activity and dispatch.
- Contract: `params` must be an object; each canonical command accepts only its current fields; `act` delegates action-specific validation but still rejects non-object params.

- [x] Add failing table-driven tests proving representative old/unknown fields (`wid`, `tid`, `index`, misspellings) return `INVALID_ARGUMENT`, and valid payloads for every canonical command pass field validation.
- [x] Run `cargo test daemon::handler::tests::canonical_request_fields --locked` and confirm rejection tests fail.
- [x] Implement an exhaustive command-to-field allowlist covering every arm in `dispatch_request`; return an error naming the unexpected field and command.
- [x] Re-run targeted handler and CLI tests, fixing only omissions in the canonical allowlist.

### Task 3: Restore command-boundary regression tests

**Files:**
- Modify: `src/daemon/handler/mod.rs`

**Interfaces:**
- Tests the existing `dispatch_request` unknown-command boundary without adding compatibility code.

- [x] Add a table-driven test named in current-contract terms that rejects non-canonical route families such as `v2.open`, `ws.list`, `tab.list`, `nav.goto`, `page.wait`, and old storage/debug routes.
- [x] Run the targeted test and confirm it passes against the current dispatcher; this is characterization coverage and requires no production change.

### Task 4: Remove obsolete docs and align terminology

**Files:**
- Delete: `docs/superpowers/specs/2026-08-03-architecture-tour-design.md`
- Delete: `docs/superpowers/plans/2026-08-03-architecture-tour.md`
- Modify: `README.md`
- Modify: `src/main.rs`
- Modify: `src/browser/mod.rs`
- Modify: `src/daemon/handler/browser.rs`

**Interfaces:**
- User-facing wording consistently says browser connection/runtime, never browser-process manager.

- [x] Delete the two artifacts that describe schema v3/v2 migration and managed-browser compatibility as current behavior.
- [x] Replace remaining `browser manager/management` wording with `browser connections/connection management`.
- [x] Run clean-slate repository searches and ensure remaining migration words only describe the explicit absence/removal in the current clean-slate spec/plan or changelog.

### Task 5: Full verification

**Files:**
- Verify all modified files.

- [x] Run `cargo test --all-targets --locked`.
- [x] Run `cargo clippy --all-targets --locked -- -D warnings`.
- [x] Run `cargo fmt --all -- --check`.
- [x] Run `git diff --check`.
- [x] Run canonical clean-slate searches and inspect `git status --short`; do not stage or commit.
