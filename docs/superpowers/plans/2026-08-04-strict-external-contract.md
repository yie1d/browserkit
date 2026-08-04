# Strict External Contract Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reject every unsupported CLI option combination and every malformed canonical request field instead of silently ignoring or defaulting it.

**Architecture:** Add a pure CLI option-scope validator before daemon connection and enrich the central canonical request contract with JSON type metadata. Handlers continue to own required fields and semantic constraints, with `snapshot.wait` made explicitly strict.

**Tech Stack:** Rust 1.75, Clap, Serde JSON, Tokio, cargo test/clippy/fmt.

## Global Constraints

- Do not add migration, compatibility, deprecated-input, or normalization behavior.
- Do not launch, connect to, close, or terminate Chrome during validation.
- Keep `cdpkit = "0.5.0"` and add no dependencies.
- Keep changes uncommitted unless the user explicitly requests a commit.

---

### Task 1: Enforce CLI option scopes

**Files:**
- Modify: `src/main.rs`

**Interfaces:**
- Produces: `validate_cli_option_scope(cli: &Cli) -> Result<(), String>` called before daemon client creation.

- [x] Add tests showing unsupported `--target`, `--timeout`, and `--no-state-diff` combinations currently parse but must fail scope validation.
- [x] Run the targeted tests and confirm RED because the validator does not exist.
- [x] Implement command-scope predicates and return an error naming the option and command.
- [x] Change `dialog list` and `dialog policy` builders to send only `session`; test their exact request shapes.
- [x] Re-run targeted CLI tests and confirm GREEN.

### Task 2: Enforce canonical request field types

**Files:**
- Modify: `src/daemon/handler/mod.rs`

**Interfaces:**
- Produces: typed field specs consumed by `validate_request_fields(req: &Request) -> Result<(), Response>`.

- [x] Replace the current null-valued contract test with valid representative values for every canonical field.
- [x] Add table-driven tests for wrong string, bool, integer, object, array, and string-array shapes; confirm RED.
- [x] Implement the minimal typed schema while preserving unknown-command dispatch and `act` action-specific parsing.
- [x] Re-run canonical request tests and confirm GREEN.

### Task 3: Reject invalid enum-like values

**Files:**
- Modify: `src/daemon/handler/snapshot.rs`

**Interfaces:**
- Changes: `WaitStrategy::from_param` returns a validation result instead of defaulting unknown values.

- [x] Add a failing test that `snapshot.wait = "invalid"` returns `INVALID_ARGUMENT`.
- [x] Implement strict accepted values: `dom-stable`, `networkidle`, and `none` only.
- [x] Re-run snapshot tests and confirm GREEN.

### Task 4: Align public documentation

**Files:**
- Modify: `README.md`
- Modify: `docs/bk-browser/references/commands.md`

**Interfaces:**
- Documents exact command scopes for global options and strict request rejection.

- [x] Replace broad global-option wording with exact applicability and state that unsupported combinations are errors.
- [x] Check documentation searches for stale claims that global options apply universally.

### Task 5: Full verification

**Files:**
- Verify all modified files.

- [x] Run `cargo test --all-targets --locked`.
- [x] Run `cargo clippy --all-targets --locked -- -D warnings`.
- [x] Run `cargo fmt --all -- --check`.
- [x] Run `git diff --check` and inspect `git status --short`.
- [x] Confirm no Chrome process was launched or connected and leave all changes uncommitted.
