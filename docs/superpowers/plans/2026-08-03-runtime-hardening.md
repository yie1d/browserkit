# Browserkit Runtime Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the remaining target-lifecycle races, make transient persistence failures recoverable, and constrain log retention without changing the approved URL or CLI surface.

**Architecture:** All session mutations share one asynchronous per-session read/write lifecycle gate. Ordinary commands and watcher events take read access; destructive cleanup and binding take write access. Target ownership mutations additionally share the existing synchronous registration lock, with the fixed order session bind then lifecycle then registration then DashMap entry. Persistence keeps permanent schema protection separate from transient runtime health.

**Tech Stack:** Rust 1.75, Tokio, DashMap, parking_lot, cdpkit 0.5.0, Cargo unit tests.

## Global Constraints

- Keep `open` and `navigate` as the only URL entry points; do not add `open-file`.
- Preserve HTTP/HTTPS localhost, loopback, private-network, `file:`, UNC file URL, and `about:blank` support.
- Never hold a DashMap guard across `.await`.
- Lock order is session bind, session lifecycle, target registration, then DashMap entry.
- Do not connect to, launch, close, or terminate the user's normal Chrome.
- Do not stage or commit files unless the user explicitly requests it.

---

### Task 1: Make target ownership mutations atomic

**Files:**
- Modify: `src/daemon/target_lifecycle.rs`

**Interfaces:**
- Produces: `remove_session_tab` with the same public signature, but serialized by `target_registration_lock`.
- Preserves: `register_reserved_session_tab` and `register_initialized_session_tab` return contracts.

- [ ] **Step 1: Write a failing deterministic ownership race test**

Add a test-only hook/helper that pauses registration after owner lookup, run removal concurrently, and assert registration cannot return `AlreadyTracked` after removal has committed. The assertion must express the invariant: after any completed operation, either the target is registered or the caller receives a non-success outcome; stale `AlreadyTracked` is forbidden.

- [ ] **Step 2: Run the targeted test and verify RED**

Run: `cargo test --locked daemon::target_lifecycle::tests::ownership_removal_cannot_invalidate_already_tracked_result -- --exact --nocapture`

Expected: FAIL because removal currently bypasses `target_registration_lock`.

- [ ] **Step 3: Serialize removal with the registration lock**

Move the current removal body behind one internal function that assumes the registration guard is held, and make the public function acquire the guard before owner lookup and removal. Keep event emission, subscription cancellation, activity touch, and persist request after the state mutation.

- [ ] **Step 4: Run target lifecycle tests and verify GREEN**

Run: `cargo test --locked daemon::target_lifecycle::tests -- --nocapture`

Expected: all target lifecycle tests pass.

---

### Task 2: Put watcher events behind the session lifecycle gate

**Files:**
- Modify: `src/daemon/target_lifecycle.rs`
- Modify: `src/daemon/server.rs`
- Modify: `src/daemon/handler/session.rs`
- Modify: `src/daemon/handler/browser.rs`

**Interfaces:**
- Produces: asynchronous watcher handlers for created, destroyed, and info-changed events.
- Consumes: `DaemonState::session_lifecycle_lock(&str)`.
- Preserves: one watcher per browser host and existing lifecycle event payloads.

- [ ] **Step 1: Write failing watcher-vs-cleanup tests**

Add deterministic tests that hold a session lifecycle guard, start a watcher mutation task, and assert the mutation does not alter `tabs` until the guard is released. Add a cleanup regression that changes `last_active`/registers a target after the initial expired scan and asserts the session is not disposed from a stale plan.

- [ ] **Step 2: Verify the new lifecycle tests fail**

Run: `cargo test --locked watcher_mutation_waits_for_session_lifecycle -- --nocapture`

Run: `cargo test --locked cleanup_revalidates_plan_after_lifecycle_lock -- --nocapture`

Expected: FAIL because watcher mutation currently bypasses the lifecycle gate and cleanup uses its pre-lock target snapshot.

- [ ] **Step 3: Implement lifecycle-aware watcher mutation**

For created events, resolve a tentative session, acquire its lifecycle lock, then revalidate session existence, host, context, and target ownership before attach/register. For destroyed and info-changed events, resolve a tentative owner, acquire that owner's lifecycle lock, revalidate ownership, then mutate. If created-target initialization completed but registration is no longer valid, detach the initialized CDP session.

- [ ] **Step 4: Rebuild cleanup and close plans under the lifecycle lock**

In idle cleanup, keep only candidate session names in the outer scan. After acquiring each lifecycle lock, re-read `last_active` and rebuild the target/context plan. Apply the same plan-after-lock rule to session close. Browser disconnect must cancel its watcher first, then process each matching session under that session's lifecycle lock without holding multiple lifecycle locks simultaneously.

- [ ] **Step 5: Run lifecycle, server, session, and browser handler tests**

Run: `cargo test --locked daemon::target_lifecycle::tests -- --nocapture`

Run: `cargo test --locked daemon::server::tests -- --nocapture`

Run: `cargo test --locked daemon::handler::session::tests -- --nocapture`

Run: `cargo test --locked daemon::handler::browser::tests -- --nocapture`

Expected: all selected tests pass and no test hangs.

---

### Task 3: Separate permanent persistence disablement from transient errors

**Files:**
- Modify: `src/daemon/state.rs`
- Modify: `src/daemon/persist.rs`
- Modify: `src/daemon/handler/daemon.rs`

**Interfaces:**
- Preserves: `persist_disabled` and `persist_disabled_reason` for future-schema fail-closed state.
- Produces: `persist_last_error: Mutex<Option<String>>` for recoverable runtime failures.
- Status contract: `persistence.enabled` means writes are not permanently disabled; `persistence.last_error` exposes transient degradation.

- [ ] **Step 1: Write failing persistence state-transition tests**

Add tests proving: a runtime write error sets `last_error` without setting `persist_disabled`; a later success clears `last_error`; a future-schema load still sets permanent disablement and remains non-retryable.

- [ ] **Step 2: Verify RED**

Run: `cargo test --locked runtime_failure_remains_retryable -- --nocapture`

Run: `cargo test --locked successful_retry_clears_runtime_error -- --nocapture`

Run: `cargo test --locked daemon_status_distinguishes_transient_persistence_error -- --nocapture`

Expected: FAIL because runtime failures currently call permanent `disable_persistence`.

- [ ] **Step 3: Implement transient persistence health**

Add `persist_last_error` to `DaemonState`. Keep the early return for permanent disablement. On a runtime write/worker error, store `last_error`, warn, return the error, and leave future requests enabled. On success, clear `last_error`. Extend daemon status without removing existing fields.

- [ ] **Step 4: Run persistence and daemon status tests**

Run: `cargo test --locked daemon::persist::tests -- --nocapture`

Run: `cargo test --locked daemon::handler::daemon::tests -- --nocapture`

Expected: all selected tests pass.

---

### Task 4: Restrict daemon log pruning to appender files

**Files:**
- Modify: `src/daemon_logging.rs`

**Interfaces:**
- Produces: a private filename predicate accepting only `daemon.log.YYYY-MM-DD`.

- [ ] **Step 1: Extend the pruning test and verify RED**

Create `daemon.log.keep`, `daemon.log.notes`, a directory named like a rotated log, valid rotated files, and `unrelated.log`. Run pruning and assert only excess valid regular rotated files are deleted.

Run: `cargo test --locked daemon_logging::tests::pruning_preserves_similarly_prefixed_and_non_file_entries -- --exact --nocapture`

Expected: FAIL because the current predicate uses `starts_with("daemon.log")`.

- [ ] **Step 2: Implement the exact predicate**

Accept the literal prefix plus a ten-character ASCII date in `YYYY-MM-DD` positions and require `metadata().is_file()`. Do not add a regex dependency.

- [ ] **Step 3: Run logging tests and verify GREEN**

Run: `cargo test --locked daemon_logging::tests -- --nocapture`

Expected: all logging tests pass.

---

### Task 5: Final verification and delivery hygiene

**Files:**
- Inspect: all modified files
- Normalize only if needed: `Cargo.toml`, `Cargo.lock`

- [ ] **Step 1: Run formatting and static checks**

Run: `cargo fmt --check`

Run: `cargo clippy --all-targets --locked -- -D warnings`

Expected: both exit 0 with no diagnostics.

- [ ] **Step 2: Run complete tests and build**

Run: `cargo test --all-targets --locked`

Run: `cargo build --locked`

Expected: all tests pass and build exits 0.

- [ ] **Step 3: Inspect scope and line-ending state**

Run: `git diff --check`

Run: `git status --short`

Run: `git diff -- Cargo.toml Cargo.lock`

If Cargo files still appear modified with identical HEAD/worktree object hashes and no diff, report the Windows line-ending/index condition rather than staging them. Do not use a destructive checkout or silently stage files.

- [ ] **Step 4: Perform a final read-only concurrency review**

Trace lock acquisition at every lifecycle and ownership mutation call site. Confirm no reverse `target registration -> session lifecycle` acquisition and no DashMap guard across await. Report any remaining real Chrome E2E gap explicitly.
