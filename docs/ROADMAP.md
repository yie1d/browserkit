# browserkit / cdpkit-rs Joint Roadmap

This roadmap tracks the shared direction for browserkit and cdpkit-rs.

Core boundary:

- cdpkit-rs is the pure CDP protocol layer: typed commands, sessions, events, sender traits, connection handling, and generated protocol bindings.
- browserkit is the persistent browser runtime for AI agents: daemon lifecycle, browser attachment, sessions, tabs, persistence, snapshots, actions, and agent-facing JSON commands.
- browserkit depends on cdpkit-rs. Breaking cdpkit-rs changes are allowed during active development, but browserkit must be updated in the same maintenance flow.

## Phase 0: Baseline And Boundaries

Status: in progress

- [ ] Record the current dirty worktree state in both repositories before large changes.
- [ ] Align README, AGENTS, REDESIGN, and CLI help with the protocol/runtime boundary.
- [ ] Keep browserkit on a local path dependency to cdpkit-rs during joint development.
- [ ] Establish verification commands for both projects.
- [ ] Fix obvious project metadata drift, such as stale versions and memory path notes.

## Phase 1: browserkit v2 Session Runtime Closure

Status: in progress

- [x] Make `connect --session <name>` create a real isolated BrowserContext.
- [x] Keep default session attached to the user's existing browser context.
- [ ] Route `open`, `snapshot`, `act`, `navigate`, `tabs`, `close`, `evaluate`, `screenshot`, and `session cookies` through the session model.
- [x] Move `wait` off the legacy workspace path and onto session targets.
- [x] Enforce `max_sessions` and `max_tabs_per_session`.
- [x] Implement session idle timeout and cleanup.
- [ ] Ensure Chrome disconnect handling marks affected sessions and returns structured errors.

## Phase 2: browserkit Session Persistence

Status: in progress

- [x] Extend `state.json` to persist sessions.
- [x] Persist session mode, browser host, BrowserContext id, tabs, active target, and timestamps.
- [x] Restore sessions after daemon restart, or mark them disconnected with explicit restore warnings.
- [x] Mark non-restorable sessions or tabs as disconnected instead of pretending they are usable.
- [ ] Add one-way migration from legacy workspace state into session state with explicit drop warnings for non-restorable tabs.
- [x] Document state schema versioning and forward-compatibility rules.

## Phase 3: browserkit Legacy Removal

Status: pending

- [ ] Deprecate or remove public `ws.*` commands.
- [ ] Deprecate or remove legacy `tab.*`, `nav.*`, `page.*`, and `act.*` handlers after v2 equivalents are complete.
- [ ] Remove `BK_WS`, `--ws`, and workspace-first documentation.
- [ ] Move workspace code out of the runtime path, keeping only migration code until it can be deleted.
- [ ] Remove legacy tests after equivalent v2 tests exist.
- [ ] Keep CLI output JSON-only.

## Phase 4: cdpkit-rs Protocol-Layer Improvements

Status: pending

- [ ] Fix the `flatten=false` footgun so generated APIs cannot silently create unsupported non-flatten sessions.
- [ ] Make generated `protocol.rs` stable across runs by removing volatile timestamps.
- [ ] Add codegen golden tests for command builders, event subscriptions, enum handling, refs, keyword renames, and flatten overrides.
- [ ] Evaluate bounded event streams or event stream policies for high-rate events.
- [ ] Evaluate `event_stream_result<T>()` for surfacing deserialization failures.
- [ ] Clarify or narrow HTTP discovery behavior.
- [ ] Keep browser automation concepts out of cdpkit-rs.

## Phase 5: browserkit Feature Expansion

Status: pending

- [ ] Add new features only after the v2 runtime path is stable.
- [ ] Candidate: `network watch` for structured XHR/fetch response observation.
- [ ] Candidate: download lifecycle handling.
- [ ] Candidate: `evaluate --append-to <file>` for long extraction workflows.
- [ ] Candidate: more precise snapshot token controls.
- [ ] Route missing low-level CDP capability work to cdpkit-rs first.

## Phase 6: CI, Release, And Synchronization

Status: pending

- [ ] Add or restore test and clippy CI for both repositories.
- [ ] Ensure browserkit release builds run tests and clippy before packaging.
- [ ] Keep CHANGELOG entries aligned with actual repository files and workflows.
- [ ] Publish cdpkit-rs first when browserkit depends on a new cdpkit API.
- [ ] Switch browserkit back from path dependency to crates.io once the required cdpkit-rs version is published.
- [ ] Document breaking changes and migration steps for both projects.

## Current First Batch

- [x] Fix browserkit clippy failures.
- [x] Write a focused browserkit v2 migration design.
- [x] Implement isolated session connect.
- [x] Persist and restore sessions.
- [x] Move `wait` to the session model.
- [x] Implement session idle timeout and cleanup.
- [x] Remove the legacy `page.wait` daemon route after v2 `wait` parity.
- [x] Remove the legacy `nav.wait` daemon route after v2 `wait --idle` parity.
- [x] Remove the legacy `page.state` and `page.info` daemon routes after v2 `snapshot` parity.
- [x] Move deprecated `eval` onto v2 `evaluate` and remove legacy `js.eval` / `js.await` daemon routes.
- [x] Move legacy `back` / `forward` / `reload` onto v2 `navigate` and remove matching daemon routes.
- [x] Move deprecated `shot` onto v2 `screenshot` and remove legacy `page.screenshot` daemon route.
- [x] Remove deprecated `goto`, `info`, `eval`, and `shot` CLI aliases after v2 parity.
- [x] Remove legacy `back`, `forward`, and `reload` CLI aliases after v2 `navigate` parity.
- [x] Remove legacy `new`, `ls`, and `rm` workspace CLI aliases.
- [x] Remove legacy `url` and `title` CLI aliases after v2 `evaluate` parity.
- [x] Complete v2 act parity Batch A by moving `scroll`, `hover`, and `focus` onto `bk act` and removing their legacy daemon routes.
- [x] Complete v2 act parity Batch B by moving `select` and `options` onto `bk act` and removing their legacy daemon routes.
- [x] Complete v2 act parity Batch C by moving `fill` onto `bk act` and removing the legacy `fill` command and `act.fill` daemon route.
- [x] Complete v2 act parity Batch D by moving `upload` and `drag` onto `bk act` and removing the legacy CLI commands plus `act.upload` / `act.drag` daemon routes.
- [x] Complete v2 act parity Batch E by moving legacy `keys` onto `bk act press --keys` and removing the `act.keys` daemon route.
- [ ] Start deleting legacy workspace-facing surfaces after v2 parity is verified.
