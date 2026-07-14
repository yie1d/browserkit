# Connecting to an Existing Chrome Instance

browserkit is a persistent browser runtime for AI agents. Its default
client, `bk`, can connect to your already-running Chrome browser, operate
in your visible window, and reuse your logged-in sessions.

This document describes the lower-level attached-browser and legacy
workspace commands. The current agent-facing API is session-oriented
(`bk connect`, `bk open`, `bk snapshot`, `bk act`).

## Prerequisites

**One-time Chrome setup** (persists across restarts):

1. Open `chrome://inspect/#remote-debugging` in your Chrome
2. Enable "Allow remote debugging"

Chrome will write a `DevToolsActivePort` file to your profile directory
containing the dynamic debug port. Do NOT hardcode port 9222.

## File locations

- Windows: `%LOCALAPPDATA%\Google\Chrome\User Data\DevToolsActivePort`
- macOS: `~/Library/Application Support/Google/Chrome/DevToolsActivePort`
- Linux: `~/.config/google-chrome/DevToolsActivePort`

## Usage

### Auto-discover and connect

```bash
# Discover Chrome via DevToolsActivePort (recommended)
bk browser discover

# With custom DevToolsActivePort path (non-default profile)
bk browser discover --path /path/to/DevToolsActivePort
```

### Connect with explicit port

```bash
# If you know the port (e.g. from DevToolsActivePort first line)
bk browser connect localhost:41753
```

### Create an attached workspace

Attached workspaces share the user's default browser context -- no cookie
isolation. They discover and attach to your existing tabs.

**Important:** attached mode requires a pre-existing browser connection.
It will never auto-launch Chrome. You must run `bk browser discover` or
`bk browser connect` first, otherwise the command will error out.
Attached workspaces should target user-connected (unmanaged) browsers --
if only bk-launched browsers exist, you must specify `--host` explicitly.

```bash
# Step 1: connect to your running Chrome (required before attached mode)
bk browser discover
# or: bk browser connect localhost:<port>

# Step 2: create attached workspace (discovers and attaches existing tabs)
bk ws new --attached                    # attach ALL open page tabs
bk ws new --attached --pattern github   # attach only tabs matching "github"

# Equivalent (ws attach is the same operation):
bk ws attach --pattern "github.com"
bk ws attach                            # attach all open page tabs
```

### Managing tabs in attached workspaces

```bash
# Create a NEW tab in user's visible Chrome window (managed by bk)
bk tab new https://example.com

# Attach an EXISTING user tab into the workspace (not managed by bk)
bk tab attach "github.com"
```

Key difference:
- `tab new` creates a tab that bk owns (managed=true). On close, the tab
  is actually closed via CloseTarget. This is intentional even in attached
  workspaces -- bk created it, so bk owns its lifecycle.
- `tab attach` attaches to an existing user tab (managed=false). On close,
  bk only detaches its CDP session -- the tab remains open in Chrome.

### Working with attached workspaces

All normal commands work: `bk goto`, `bk click`, `bk eval`, etc.

```bash
# Navigate in the user's visible tab
bk goto https://example.com

# Take screenshot of the user's actual browser state
bk shot --output current.png
```

### Close semantics (per-tab managed model)

Close behavior is determined per-tab by the `managed` flag, not by
workspace mode:

- **managed=true** tabs (created by `tab new` or isolated `ws new`):
  `CloseTarget` -- the tab is closed.
- **managed=false** tabs (attached via `ws attach`, `ws new --attached`,
  or `tab attach`): `DetachFromTarget` -- the tab stays open in Chrome.

Workspace-level close:
- `bk ws close <wid>` iterates all tabs: closes managed ones, detaches
  unmanaged ones. Isolated workspaces also dispose their BrowserContext.
- `bk tab close <tid>` does the same for a single tab.

Browser-level safety:
- Daemon shutdown (`bk daemon stop`) will not kill user-connected browsers.
  Only bk-launched browsers (managed=true, child process tracked) are killed.
- Workspace timeout expiry follows the same rules.

### Isolated vs Attached comparison

| Aspect            | Isolated (default)          | Attached (`--attached`)       |
|-------------------|-----------------------------|-----------------------------|
| BrowserContext    | Own (isolated cookies)      | Shared (user's real state)  |
| Cookie/session    | Fresh, empty                | User's logged-in sessions   |
| Tab visibility    | Only if `--no-headless`     | Always in user's window     |
| On ws close       | Closes tabs + disposes ctx  | Detach unmanaged, close managed |
| On tab close      | CloseTarget (always)        | Per-tab: managed closes, unmanaged detaches |
| Use case          | Scraping, testing           | RPA on logged-in sites      |

## Chrome 136+ and `/json` endpoint restrictions

Starting with Chrome 136, when remote debugging is enabled via the
`chrome://inspect` toggle (as opposed to the `--remote-debugging-port` flag),
Chrome **disables the HTTP `/json/*` discovery endpoints** (they return 404).

This means the traditional flow of querying `http://host:port/json/version`
to discover the WebSocket URL no longer works for toggle-enabled Chrome.

**How bk handles this:**

- `bk browser discover` reads the `DevToolsActivePort` file, which contains
  both the port (line 1) and the browser WebSocket path (line 2, e.g.
  `/devtools/browser/<guid>`).
- When the ws path is present, bk constructs a direct `ws://` URL and
  connects without hitting `/json/version`.
- When the ws path is absent (older Chrome, or port-only file), bk falls
  back to the traditional `/json/version` discovery via the host.

If you encounter connection failures with `bk browser connect localhost:<port>`,
use `bk browser discover` instead -- it will use the ws path automatically.
Alternatively, pass the full WebSocket URL directly:

```bash
bk browser connect "ws://localhost:<port>/devtools/browser/<guid>"
```

## Connection timeout

All CDP connection attempts (both `discover` and `connect`) are wrapped in
a 10-second timeout. If the endpoint is unreachable or the DevToolsActivePort
file is stale (Chrome exited without cleaning it up), bk will fail fast with
a clear error message instead of hanging indefinitely.

Common causes of timeout:
- Chrome has exited but `DevToolsActivePort` was not deleted
- The debug port is blocked by a firewall
- A different process is listening on that port

Resolution: restart Chrome, or delete the stale DevToolsActivePort file and
re-enable debugging via `chrome://inspect/#remote-debugging`.

## Security considerations

- Attached mode gives bk full control over the user's authenticated
  browser sessions (cookies, localStorage, page content). Treat the
  daemon's TCP socket as a privileged interface.
- The daemon listens on `127.0.0.1` only (localhost). No new network
  ports are exposed by this feature.
- No authentication is added to the daemon protocol -- this is unchanged
  from the existing architecture. Access is gated by local TCP only.
