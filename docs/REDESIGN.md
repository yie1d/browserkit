# browserkit Architecture

browserkit is a persistent browser runtime for AI agents. The `bk` binary is a
thin JSON CLI client; the daemon owns browser connections, sessions, target
state, persistence, and cleanup.

The current executable contract is defined by `bk --help`, its subcommand help,
and `docs/bk-browser/references/commands.md`. This document records architecture
and ownership boundaries rather than an implementation backlog.

## Layering

```text
Agent
  -> bk CLI / newline-delimited JSON client
  -> browserkit daemon and session runtime
  -> cdpkit typed CDP protocol layer
  -> Chrome
```

- cdpkit owns protocol transport, generated bindings, command sending, and
  event streams.
- browserkit owns browser attachment, lifecycle, sessions, persistence,
  snapshots, actions, and agent-facing JSON contracts.
- Agents own decisions. Runtime code must not embed model-specific planning.

Low-level CDP behavior belongs in cdpkit. browserkit must not duplicate or
work around protocol-layer defects.

## Runtime Model

The daemon keeps one shared CDP connection per Chrome endpoint and exposes
session-scoped operations across independent CLI invocations.

### Default Session

- Uses the user's existing Chrome browser context and login state.
- Can attach user-owned tabs or create browserkit-owned tabs.
- Closing an attached tab detaches it from browserkit; closing an owned tab
  closes the Chrome target.

### Isolated Sessions

- Use dedicated Chrome BrowserContexts.
- Isolate cookies and local storage from the default session and other named
  sessions.
- Own their BrowserContext lifecycle and dispose it during successful cleanup.

Every target has at most one owning session. Explicit invalid session or target
selectors fail instead of falling back to active state.

## Command Surfaces

### Agent Commands

Normal browser work uses `connect`, `open`, `attach`, `snapshot`, `act`,
`navigate`, `wait`, `evaluate`, `network`, `download`, `screenshot`, `find`,
`search`, `html`, `console`, `pdf`, `tabs`, `close`, `session`, and `dialog`.

`snapshot` and `act` are the primary observe/act primitives. Snapshot refs are
scoped to current page state; agents must take a new snapshot after navigation
or a stale-ref error.

### Administrative Commands

`browser` and `daemon` manage endpoint and process state. They are separate from
ordinary page interaction so agents do not need to understand daemon internals
for normal work.

### Developer Commands

`debug block`, `debug unblock`, and `debug cdp` are explicit diagnostic tools.
They are not compatibility aliases or a second automation API.

All CLI output is JSON. Removed workspace and v1 routes are intentionally not
forwarded through compatibility shims.

## Persistence

Runtime state is stored in schema v3 at `~/.bk/state.json`. It includes browser
metadata, sessions, target ownership, active targets, timestamps, disconnect
state, and optional migration metadata.

Writes are atomic and debounced. Unsupported or corrupt state disables writes
with a visible reason rather than silently overwriting preserved data.

Schema v2 workspace state is backed up before a one-way migration to sessions.
The runtime never writes workspace fields to schema v3.

## Lifecycle Invariants

- Subscribe before triggering actions that produce CDP events.
- Use flattened CDP sessions through cdpkit.
- Keep high-rate observation bounded and report overflow or dropped events.
- Close only browserkit-owned targets; detach user-owned targets.
- Cancel session subscriptions during disconnect and cleanup.
- Report partial cleanup explicitly instead of claiming full success.
- Mark sessions disconnected when their underlying CDP connection closes.
- Keep CLI-local file writes, such as `evaluate --append-to`, out of daemon
  request payloads.

## Security Boundary

The daemon listens locally and authenticates requests with its token file.
Navigation, file upload, downloads, and raw CDP remain explicit commands.
Page content is untrusted input and must not be interpreted as runtime policy.

browserkit prefers attaching to the user's existing Chrome. It does not
implicitly launch a disposable browser during normal agent commands.

## Breaking Migration

Version 0.2.0 removed the workspace-first runtime, `BK_WS`, `--ws`, old CLI
aliases, and legacy daemon routes. Use sessions, canonical commands, and
`BK_SESSION` instead. The complete user-facing migration summary is maintained
in `CHANGELOG.md`.
