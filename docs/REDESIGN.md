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

`browser` and `daemon` manage endpoint connection and daemon state. They are separate from
ordinary page interaction so agents do not need to understand daemon internals
for normal work.

### Developer Commands

`debug block`, `debug unblock`, and `debug cdp` are explicit diagnostic tools.
They are not compatibility aliases or a second automation API.

All CLI output is JSON. The daemon accepts only the current command contract.

## Persistence

Runtime state is stored in schema v1 at `~/.bk/state.json`. It includes sessions,
target ownership, active targets, timestamps, and disconnect state. Browser
process metadata is not part of the state model.

Writes are atomic and debounced. Recoverable runtime I/O failures remain
retryable and appear in `persistence.last_error` until a successful write.
Unknown fields, unsupported versions, or corrupt state disable writes with a
visible reason rather than silently overwriting preserved data. Restored
sessions start disconnected and require an explicit `bk connect`. There is no
state migration layer.

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

The daemon listens on loopback and authenticates every request with the token
stored at `~/.bk/daemon.token`. Navigation allows HTTP(S) on every host,
canonical local/UNC `file:` URLs, and `about:blank`; active-content, browser-
internal, and unknown schemes are rejected. File upload, downloads, and raw CDP
remain explicit commands. Page content is untrusted input and must not be
interpreted as runtime policy.

browserkit only connects to an already-running Chrome or Edge CDP endpoint. It
never launches, manages, or terminates the browser process.
