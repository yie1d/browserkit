# browserkit Workspace Runtime Removal Design

**Date:** 2026-07-15

**Status:** Approved for implementation planning

## 1. Context

browserkit is a persistent browser runtime for AI agents. Its public model is
session-based, but the daemon still maintains a second workspace-based runtime.
The two models have separate tab maps, lifecycle handling, persistence, and
command surfaces.

The v2 primary commands already use sessions, while several useful capabilities
still resolve a legacy workspace before reaching a page:

- page inspection: find, search, HTML, console, and PDF;
- storage: localStorage and storage export/import;
- dialogs;
- network request blocking;
- raw CDP access;
- attaching an existing user tab.

Keeping both state trees makes browser disconnects, target lifecycle, daemon
shutdown, persistence, and documentation harder to reason about. This design
removes the workspace runtime in one breaking migration and rebuilds the useful
capabilities directly on sessions.

## 2. Goals

1. Make `Session` the only browser activity and persistence boundary.
2. Remove all public and internal workspace command paths.
3. Preserve useful capabilities without preserving workspace compatibility.
4. Track whether a tab is owned by browserkit or attached from the user.
5. Centralize target lifecycle, console, and dialog subscriptions.
6. Migrate version 2 state to a version 3 session-only schema once, with an
   atomic backup and explicit warnings.
7. Keep browserkit above cdpkit-rs: browserkit owns runtime policy, while
   cdpkit-rs remains the typed CDP protocol layer.

## 3. Non-goals

- Maintaining `ws.*`, `tab.*`, `nav.*`, `page.*`, legacy `storage.*`, or
  `v2.*` daemon aliases.
- Keeping `BK_WS`, `--ws`, workspace IDs, tab aliases, or default-workspace
  resolution.
- Preserving non-functional streaming commands as placeholders.
- Adding a generic compatibility context that supports both Workspace and
  Session.
- Moving browser automation policy into cdpkit-rs.
- Adding unrelated browser features before the session-only runtime is stable.

## 4. Target Architecture

### 4.1 Daemon state

`DaemonState` will contain:

- connected browsers;
- sessions;
- browser-scoped target watcher tasks;
- console and dialog state keyed by session and target;
- persistence and startup migration status;
- runtime limits and request accounting.

It will no longer contain:

- `workspaces`;
- `default_wid`;
- workspace cleanup tasks;
- workspace-scoped auto-attach tasks;
- workspace-based dialog or console keys.

Browser discovery currently implemented in the workspace handler will move to
the browser handler. Target matching and duplicate-detection helpers currently
owned by the tab handler will move to the session target lifecycle layer.

### 4.2 Session and tab ownership

`Session` remains the browser-context boundary. A session has one browser host,
an optional isolated BrowserContext ID, tracked tabs, one active target, and
connection/lifecycle timestamps.

`SessionTab` gains an ownership field:

- `Owned`: created by browserkit. Closing it closes the Chrome target.
- `Attached`: adopted from the user's existing browser. Closing it only removes
  it from browserkit and detaches browserkit's CDP session.

The ownership distinction applies to explicit close, session close, idle
cleanup, daemon stop, and browser disconnect handling. User-owned targets must
never be closed as a side effect of browserkit cleanup.

### 4.3 Unified session target resolver

All page-facing handlers use one resolver. It:

1. resolves the requested session, defaulting only when no session was given;
2. rejects disconnected sessions;
3. resolves the explicit target or the session's active target;
4. checks that the target belongs to that session;
5. resolves the browser and the target CDP session;
6. exposes the BrowserContext ID for context-scoped operations;
7. updates session and target activity after successful use.

An explicitly supplied missing session or target never falls back to another
session or target.

### 4.4 Browser target watcher

One watcher per browser connection consumes target lifecycle events and updates
session state. It is responsible for:

- tracking targets created by browserkit operations;
- detecting tabs opened by an action and reporting `new_tab`;
- removing destroyed targets from sessions;
- refreshing target URL and title metadata;
- starting and stopping console and dialog subscriptions for tracked targets;
- avoiding duplicate ownership across sessions;
- marking affected sessions disconnected when the browser connection closes.

The watcher is runtime infrastructure. It does not expose CDP protocol details
as agent-facing concepts.

## 5. External API

### 5.1 Agent-facing commands

The supported agent-facing commands are:

```text
connect
open
attach
snapshot
find
search
act
navigate
wait
evaluate
html
console
pdf
screenshot
tabs
close
session
status
dialog
```

`attach` adopts an existing user target into the default session after explicit
target or pattern selection. Isolated sessions create targets with `open`
inside their own BrowserContext. A target already tracked by another session is
rejected.

`find` and `search` remain because they provide capabilities not equivalent to
snapshot: arbitrary CSS queries, selected attributes, regular expressions,
scoped search, context snippets, and result limits.

`html`, `console`, and `pdf` become session-native commands rather than legacy
page commands.

### 5.2 Session storage

Storage is exposed under the session boundary:

```text
session cookies get|set|clear
session storage local get|set
session storage export|import
```

Cookie operations for isolated sessions always use their BrowserContext ID.
They must not fall back to browser-wide cookie operations when the session has
no active tab.

### 5.3 Management and developer commands

These are supported but are not the agent's primary workflow:

```text
browser connect|discover|list|disconnect
daemon start|status|stop
debug cdp
debug block|unblock
```

Browser and daemon commands are formal administration APIs. Raw CDP and request
blocking are developer escape hatches.

### 5.4 Removed commands and routes

The following are removed without forwarding aliases:

- CLI: `ws`, `tab`, and `fetch`;
- environment and flags: `BK_WS` and `--ws`;
- daemon routes: `ws.*`, `tab.*`, `nav.*`, `page.*`, old `storage.*`, and all
  `v2.*` aliases;
- debug commands: `monitor`, `har`, and `events`.

The removed streaming commands are not working capabilities in the current
daemon: they acknowledge setup but never stream events, and HAR always returns
an empty entry list. Their removal does not require a compatibility replacement
in this migration.

Every supported capability has one canonical daemon command. CLI and daemon
responses remain JSON-only.

## 6. Request and Lifecycle Flows

### 6.1 Opening a tab

1. Resolve the session and browser context.
2. Enforce the session tab limit.
3. Create the target in the session's BrowserContext.
4. Register it as `Owned`.
5. Start lifecycle subscriptions through the browser watcher.
6. Make it the session's active target and persist.

### 6.2 Attaching a user tab

1. Resolve the default session and browser.
2. Discover page targets and resolve an exact target or an unambiguous pattern.
3. Reject targets already tracked by another session.
4. Attach with flattened CDP sessions.
5. Register the target as `Attached`.
6. Start console and dialog subscriptions.
7. Persist only browserkit metadata; browserkit never claims target ownership.

### 6.3 Closing tabs and sessions

For `Owned` tabs, close the target and remove subscriptions. For `Attached`
tabs, detach the CDP session and remove subscriptions without closing the
target.

Closing an isolated session also disposes its BrowserContext after owned tabs
are closed. Closing the default session clears tracked tabs but leaves the
user's browser and attached targets running.

### 6.4 Browser disconnect

The disconnect monitor removes the browser connection, cancels its watcher,
marks every affected session disconnected, clears live subscription handles,
and persists the disconnected state. Subsequent commands return a structured
`CHROME_DISCONNECTED` error until the session is restored or reconnected.

## 7. Persistence and One-way Migration

### 7.1 Version 3 schema

Version 3 stores only:

- schema version;
- restorable browser metadata;
- sessions and their tabs;
- migration report metadata when a version 2 migration occurred.

It does not store workspaces or a default workspace.

### 7.2 Version 2 reader

The version 2 reader is isolated in a migration module and is not part of the
runtime model. On first successful parse it:

1. atomically creates a version 2 backup;
2. keeps existing version 2 sessions as authoritative;
3. converts each isolated workspace to an isolated session named
   `legacy-<wid-prefix>`, disambiguating names deterministically;
4. attempts to merge attached workspace tabs into the default session for the
   same browser;
5. drops conflicting hosts, duplicate targets, and non-restorable tabs with a
   structured warning;
6. writes version 3 atomically only after conversion completes;
7. exposes migrated and dropped counts, warnings, and the backup path through
   `status`.

The migration is best effort. Partial tab loss does not prevent daemon startup,
but it is never silent.

If the old state is corrupt, browserkit preserves it, disables persistence for
that daemon run, and reports the failure instead of replacing it with empty
state. If the file has a schema newer than the binary supports, the existing
forward-compatibility behavior remains: do not overwrite it.

## 8. Error Contract

All failures use the existing structured JSON error envelope. The migration
adds or standardizes these conditions:

- session not found;
- session has no target;
- target not found in the requested session;
- target already attached to another session;
- browser disconnected;
- browser or BrowserContext unavailable;
- ambiguous attach pattern;
- persisted state unreadable or newer than supported.

A partially successful migration is reported in `status` as structured
migration metadata. It does not turn unrelated runtime requests into failures.

Removed commands are rejected as unknown commands. They are not translated to
new commands, because compatibility forwarding would retain the legacy surface
and its ambiguous workspace semantics.

## 9. Implementation Shape

Although this is one breaking migration, implementation should use reviewable
TDD commits while keeping the branch's final state free of a dual runtime:

1. add SessionTab ownership and the unified resolver;
2. build the browser target watcher and subscription lifecycle;
3. migrate retained page, storage, dialog, network, and CDP capabilities;
4. migrate CLI consumers and status/admin behavior;
5. implement and test the version 2 to version 3 converter;
6. remove all workspace runtime, routes, configuration, errors, tests, and docs;
7. run full static, unit, CLI, integration, and real-browser verification.

No transitional `WorkspaceOrSession` abstraction is introduced. Intermediate
commits may prepare dependencies, but the migration is not complete until the
workspace state tree and all consumers are gone.

## 10. Test Strategy

Development follows test-driven development. Each behavior starts with a
focused failing test.

### 10.1 Model and resolver tests

- Owned versus Attached serialization and close behavior.
- Explicit missing session or target never falls back.
- Disconnected sessions return `CHROME_DISCONNECTED`.
- Context-scoped operations receive the correct BrowserContext ID.
- A target cannot belong to more than one session.

### 10.2 Migration fixture tests

- version 2 state with sessions only;
- isolated workspace conversion;
- attached workspace merge;
- session name collisions;
- duplicate targets and conflicting browser hosts;
- non-restorable tabs with warnings;
- backup creation and atomic version 3 write;
- corrupt and newer-version state protection;
- version 3 round-trip with no workspace fields.

### 10.3 Handler and lifecycle tests

- session-native find, search, HTML, console, PDF, storage, dialog, network
  blocking, and raw CDP routing;
- target created/destroyed metadata updates;
- action-triggered `new_tab` reporting;
- console and dialog subscription startup and cleanup;
- attached tab close leaves the Chrome target alive;
- isolated cookie operations never become browser-wide;
- browser disconnect marks all matching sessions and no others.

### 10.4 CLI and removal tests

- canonical help and parser coverage for every supported command;
- `ws`, `tab`, `fetch`, `BK_WS`, `--ws`, and removed debug commands rejected;
- removed daemon routes return unknown-command JSON;
- output remains JSON-only;
- repository scans find no runtime workspace types, routes, config, or primary
  documentation wording.

### 10.5 Real Chrome verification

- attach to an existing user tab;
- closing an attached tab does not close it in Chrome;
- opening and closing an owned tab does close it;
- default and isolated sessions remain separated;
- isolated cookies remain context-scoped;
- click-triggered new tabs are detected;
- console and dialog state is delivered for session targets;
- daemon restart restores or explicitly disconnects persisted sessions.

Final verification includes:

```text
cargo test
cargo clippy --all -- -D warnings
git diff --check
```

Public help, README, REDESIGN, ROADMAP, command references, and migration notes
must describe the session-only runtime. `.codex/` remains untracked and is not
part of any commit.

## 11. Completion Criteria

The migration is complete only when:

- no supported CLI command resolves a workspace;
- no daemon handler accepts `wid`;
- `DaemonState` has no workspace map or default workspace;
- version 3 persistence writes no workspace fields;
- useful legacy-only capabilities operate through sessions;
- attached targets are never closed by browserkit cleanup;
- fake streaming commands are gone;
- all removed commands and routes have rejection tests;
- the repository's current documentation presents only the session runtime;
- automated verification and the real Chrome acceptance checks pass.
