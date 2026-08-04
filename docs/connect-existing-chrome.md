# Connecting to an Existing Chrome Instance

browserkit is a persistent browser runtime for AI agents. Its default client,
`bk`, connects to the user's already-running Chrome, reuses the user's logged-in
browser context, and keeps session/tab state in the local daemon. browserkit
does not launch, manage, or terminate the browser process.

## Prerequisites

One-time Chrome setup persists across restarts:

1. Open `chrome://inspect/#remote-debugging` in Chrome.
2. Enable remote debugging.
3. Leave Chrome running.

Chrome writes a `DevToolsActivePort` file to the profile directory containing
the dynamic debug port and, on newer Chrome versions, the browser WebSocket
path. Do not hardcode port 9222.

## File Locations

- Windows: `%LOCALAPPDATA%\Google\Chrome\User Data\DevToolsActivePort`
- macOS: `~/Library/Application Support/Google/Chrome/DevToolsActivePort`
- Linux: `~/.config/google-chrome/DevToolsActivePort`

## Connect

Use the high-level command first:

```bash
bk connect
```

For diagnostics or non-default profiles, use the admin commands:

```bash
bk browser discover
bk browser discover --path /path/to/DevToolsActivePort
bk browser connect "ws://127.0.0.1:<port>/devtools/browser/<guid>"
```

`bk connect` and `bk browser discover` use the dynamic endpoint exposed by the
user's browser. They should be preferred over fixed ports.

All three connection commands bind the selected session. Add
`--session <name>` before the command to create or reconnect an isolated
session on that endpoint.

## Attach an Existing User Tab

`bk attach` adopts an existing page target into the default session. It never
creates a new target and it must resolve to one unambiguous tab.

```bash
bk attach "github.com"
bk attach "Issue 123"
bk --target <targetId> attach
```

The match string is a URL, title, or target ID substring. Avoid broad patterns
that could match multiple user tabs. If you need a fresh tab that browserkit
owns, use `bk open <url>` instead.

User-opened tabs belong to Chrome's default BrowserContext, so isolated
sessions do not accept `bk attach`; use `bk --session <name> open <url>` there.

## Ownership and Close Semantics

browserkit tracks tab ownership per session tab:

- `Owned`: created by `bk open`; `bk close` closes the Chrome target.
- `Attached`: adopted by `bk attach`; `bk close` only detaches browserkit from
  the target and leaves the user's tab open in Chrome.

The same rule applies to `bk session close`, idle cleanup, `bk daemon stop`, and
browser disconnect cleanup. browserkit must not close a user-owned target as a
side effect of cleanup.

## Common Session Workflow

```bash
bk connect
bk attach "unique title or URL fragment"
bk snapshot --full
bk act click --ref 42
bk close
```

For a new browserkit-owned tab:

```bash
bk connect
bk open https://example.com
bk snapshot
bk close
```

For isolated work:

```bash
bk --session agent-a connect
bk --session agent-a open https://example.com
bk --session agent-a session cookies get
bk --session agent-a session close
```

## State and Restart

The daemon stores strict schema v1 session state in `~/.bk/state.json`. The file
contains session metadata, session tabs, ownership, active targets, timestamps,
and disconnect state. It does not contain browser process metadata. After a
daemon restart, restored sessions are visible but disconnected; run `bk connect`
to discover the already-running browser and bind the selected session again.
Unknown fields, corrupt content, or any non-v1 version disable persistence
writes so the original file is not overwritten.

## Chrome 136+ and `/json` Endpoint Restrictions

Starting with Chrome 136, when remote debugging is enabled via the
`chrome://inspect` toggle instead of the `--remote-debugging-port` flag, Chrome
may disable the HTTP `/json/*` discovery endpoints.

browserkit handles this by reading `DevToolsActivePort`. When the WebSocket path
is present, it connects directly to the browser WebSocket URL instead of
requiring `/json/version`. If explicit `bk browser connect localhost:<port>`
fails, prefer `bk browser discover` or pass the full WebSocket URL.

Chrome may also wait for an in-browser remote-debugging authorization. If the
debug port accepts TCP connections but the WebSocket upgrade times out, bring
the Chrome window to the foreground, open `chrome://inspect/#remote-debugging`,
and confirm the **Allow remote debugging** prompt. Then retry `bk connect`. A
stale `DevToolsActivePort` file can produce similar symptoms after Chrome exits;
restart Chrome or disable and re-enable remote debugging before retrying.

## Security Considerations

- Attaching gives browserkit control over authenticated browser sessions,
  cookies, localStorage, and page content.
- The daemon listens on loopback only and authenticates each JSON request with
  the per-daemon token in `~/.bk/daemon.token`. The standard `bk` client reads
  and sends this token automatically.
- Treat the daemon as privileged local automation. Do not expose its port to a
  network.
