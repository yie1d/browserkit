# browserkit v2 Full Migration Design

Date: 2026-07-13

## Purpose

browserkit should complete its transition from a workspace-centered browser automation CLI to a session-centered persistent browser runtime for AI agents.

The end state is:

- The agent-facing model is `session -> tabs -> observe/act`.
- `bk` is a thin JSON CLI client for the daemon/runtime.
- The daemon owns browser attachment, session lifecycle, tab tracking, persistence, and recovery.
- cdpkit-rs remains the typed CDP protocol layer below browserkit.

This design covers the migration to the v2 runtime model and the removal of legacy workspace-facing behavior after v2 parity exists.

## Current State

browserkit is currently in a mixed migration state:

- v2 commands exist for `connect`, `open`, `snapshot`, `act`, `navigate`, `tabs`, `close`, `evaluate`, `screenshot`, and `session`.
- `--session` and `BK_SESSION` are exposed as the agent-facing isolation controls.
- `DaemonState` has `sessions: DashMap<String, Session>`.
- v1 workspace handlers and commands still exist and remain heavily tested.
- `wait` still routes through the old workspace path.
- `connect --session <name>` records a named session, but currently creates a default-context session rather than a dedicated BrowserContext.
- persistence still stores browsers/workspaces/default workspace, not sessions.

## Goals

- Make named sessions real isolated BrowserContexts.
- Keep the default session attached to the user's existing browser context.
- Route all primary v2 commands through sessions.
- Persist and restore session metadata.
- Keep legacy workspace behavior only long enough to migrate state and preserve v2 parity.
- Remove workspace-first public surface after v2 commands cover the intended workflows.
- Keep all CLI output JSON-only.

## Non-Goals

- Do not move browser automation behavior into cdpkit-rs.
- Do not preserve long-term compatibility for v1 workspace commands.
- Do not add new browserkit product features before the v2 runtime path is stable.
- Do not make browserkit a general Playwright replacement.

## Runtime Model

### Sessions

`Session` is the unit agents address.

- `default` session:
  - Uses Chrome's default browser context.
  - Inherits the user's existing login state.
  - Tracks only browserkit-created tabs.
  - Closing the session closes browserkit-created tabs but keeps the default session record available for reuse.

- Named sessions:
  - Use `Target.createBrowserContext`.
  - Isolate cookies, storage, cache, and tabs from other sessions.
  - Are counted against `max_sessions`.
  - Closing the session closes its tabs and disposes its BrowserContext.

### Tabs

Each session owns a set of tabs:

- `target_id` identifies the Chrome target.
- `cdp_session_id` identifies the attached CDP session for page-level commands.
- `active_target` is the default target for commands that omit `--target`.
- Creating a tab makes it active.
- Closing the active tab moves active target to another tab or `None`.

### Commands

The v2 command path should use sessions end to end:

- `connect`: create or refresh the session and browser connection.
- `open`: create a target in the session's BrowserContext and attach flatten mode.
- `snapshot`: observe the active or specified session tab.
- `act`: interact with the active or specified session tab.
- `navigate`: navigate/back/forward/reload on the session tab.
- `tabs`: list only tabs owned by the session.
- `close`: close a session-owned tab.
- `wait`: wait on the session tab, not a workspace.
- `evaluate`: evaluate on the session tab.
- `screenshot`: capture the session tab.
- `session cookies`: operate in the correct BrowserContext, using a tab session where required.

## Persistence

`state.json` should become the source of truth for v2 runtime state.

The schema should include:

- `version`
- `browsers`
- `sessions`
- legacy `workspaces` only during migration
- default session metadata if needed

Persisted sessions should include:

- name
- mode
- browser host
- browser context id
- tabs
- active target
- created and last-active timestamps
- disconnected flag

`cdp_session_id` is transient and must not be trusted after daemon restart.

Restore rules:

- Reconnect to each persisted browser host first.
- For each persisted tab, call `Target.attachToTarget` with `flatten: true` and refresh `cdp_session_id`.
- If a target no longer exists, remove it from the session and record it in the restore report.
- If the active target is removed, select the newest remaining tab as active; if no tabs remain, set `active_target` to `None`.
- If a browser cannot be reconnected, keep the session record, clear transient CDP session IDs, mark the session `disconnected`, and return `CHROME_DISCONNECTED` for commands that need the browser.

Forward compatibility:

- If `state.json.version` is newer than supported, browserkit must not overwrite it.
- Migration from legacy workspace state is one-way. Migrated tabs must either become session tabs with refreshed CDP session IDs or be dropped with an explicit restore warning.

## Legacy Removal Strategy

Legacy workspace code should be removed only after v2 parity exists.

Order:

1. Complete v2 session runtime behavior.
2. Add session persistence and restore.
3. Port `wait` off workspace.
4. Hide or deprecate public workspace commands.
5. Remove workspace command dispatch and CLI options.
6. Keep migration-only workspace parsing until old state files are no longer supported.
7. Delete workspace runtime modules and tests once migration coverage replaces them.

The public docs, AGENTS instructions, and bk-browser skill should describe session/runtime concepts only.

## Error Handling

v2 commands should return structured errors:

```json
{
  "ok": false,
  "error": {
    "code": "SESSION_NOT_FOUND",
    "message": "...",
    "suggestion": "...",
    "recoverable": true
  }
}
```

Legacy string errors may remain internally while legacy commands exist, but every new v2 handler must return structured errors at the CLI boundary.

Required error cases:

- session not found
- no tab in session
- target not in session
- browser disconnected
- session limit exceeded
- tab limit exceeded
- invalid argument
- timeout
- JavaScript/CDP failure

## Security And Ownership

- The daemon remains local-only.
- Token authentication remains required for normal daemon operation.
- The default session must not expose or manipulate arbitrary user tabs by default.
- Actions should not focus tabs unless `--focus` is explicitly requested.
- URL scheme filtering should continue to reject dangerous schemes such as `javascript:` and unsafe HTML data URLs.
- `disable_security` should default to `false` before release. Existing config files that set it explicitly keep their value.

## Verification

Baseline verification:

```sh
cargo test
cargo clippy --all -- -D warnings
```

Runtime verification with Chrome available:

```sh
bk connect
bk open https://example.com
bk snapshot
bk act click --ref <ref>
bk tabs
bk close
bk session close
```

Session isolation verification:

```sh
bk connect --session agent-a
bk connect --session agent-b
bk open https://example.com --session agent-a
bk open https://example.org --session agent-b
bk tabs --session agent-a
bk tabs --session agent-b
```

Persistence verification:

1. Start daemon.
2. Connect and open tabs in default and named sessions.
3. Stop daemon.
4. Start daemon again.
5. Confirm sessions restore or explicitly report disconnected state.

## Risks

- Removing legacy workspace code too early may break still-unported behavior.
- BrowserContext restore semantics are limited by Chrome target availability after restart.
- Default session must balance user login reuse with protection from user tab manipulation.
- Path dependency on cdpkit-rs is correct during joint development but must be converted before release.
- Large cleanup diffs can obscure behavior changes; migration should be staged.

## Acceptance Criteria

- `cargo test` passes.
- `cargo clippy --all -- -D warnings` passes.
- All primary v2 commands use sessions, not workspaces.
- Named sessions create and use isolated BrowserContexts.
- Session state is persisted and restored or marked disconnected predictably.
- Public docs and CLI help no longer promote workspace as the primary model.
- Legacy workspace-facing commands are removed or explicitly deprecated after v2 parity is verified.
