# browserkit

A persistent browser runtime for AI agents that connects to an already-running Chrome or Edge CDP endpoint, built on [cdpkit](https://crates.io/crates/cdpkit).

browserkit connects agents to Chrome or Edge through a long-running local daemon. It keeps browser connections, tabs, isolated sessions, and page state available across CLI invocations, so agents can observe and act without re-authenticating the browser. It does not launch, manage, or terminate the browser process.

The `bk` CLI is the default client. Under the hood, it talks to the daemon over newline-delimited JSON on an ephemeral IPv4 loopback port. The daemon has no authentication layer or per-user transport isolation: any local process that can reach the port is trusted. It is intended for a single-user workstation and must never be exposed or forwarded to a network.

## Architecture

```text
┌─────────────────────────────────────────────────────┐
│ Clients                                             │
│                                                     │
│   bk CLI  /  any local TCP client                   │
└──────────────────────┬──────────────────────────────┘
                       │ newline-delimited JSON (TCP)
┌──────────────────────▼──────────────────────────────┐
│ browserkit runtime                                  │
│                                                     │
│   daemon      sessions      tabs      persistence   │
│   observe     act           browser connections     │
└──────────────────────┬──────────────────────────────┘
                       │ typed CDP commands/events
┌──────────────────────▼──────────────────────────────┐
│ cdpkit-rs                                           │
│                                                     │
│   type-safe Chrome DevTools Protocol client         │
└──────────────────────┬──────────────────────────────┘
                       │ CDP WebSocket
┌──────────────────────▼──────────────────────────────┐
│          Chrome / Edge / Chromium                    │
└─────────────────────────────────────────────────────┘
```

The daemon is the runtime boundary: it owns persistent browser connections, session state, tab tracking, and debounced state persistence. The CLI is intentionally thin.

The CLI verifies every daemon connection with a `ping` and reuses that verified
connection for the command. Daemon startup is bounded to 30 seconds; TCP connect
and handshake each use 2-second deadlines. Requests use at least 30 seconds,
include 5 seconds of client grace beyond a command timeout, and are capped at
600 seconds. The server closes a client connection after 60 seconds without a
new request.

## Why browserkit

browserkit is designed for agents that need to work in a real browser over multiple tool calls.

- **Attach to the user's Chrome**: use the browser and login state the user already has, instead of launching a disposable automation browser.
- **Persistent runtime**: the daemon keeps browser connections and session state alive across commands and agent turns.
- **Observe / Act API**: agents get compact page snapshots, then interact through stable element refs or coordinates.
- **Session isolation**: named sessions use isolated browser contexts for parallel agents, while the default session can share the user's logged-in context.
- **Local JSON protocol**: `bk` is a CLI client over a simple local TCP protocol, so other clients can be added without changing the runtime model.

## Layering

browserkit intentionally sits above cdpkit-rs.

- `cdpkit-rs` is the protocol layer: typed CDP commands, sessions, events, and senders.
- `browserkit` is the runtime layer: daemon lifecycle, browser attachment, sessions, tabs, persistence, snapshots, and actions.
- The agent is the decision layer: it observes page state and decides the next action.

Low-level CDP support belongs in cdpkit-rs. browserkit composes those capabilities into agent-friendly browser operations.

## Requirements

- Rust 1.75+
- Chrome or Chromium with remote debugging enabled

## Build

```sh
git clone https://github.com/yie1d/browserkit
cd browserkit
cargo build --release
# binary: target/release/bk
```

## Documentation

- [Architecture](docs/REDESIGN.md)
- [Interactive architecture tour](docs/architecture-tour.html)
- [Roadmap](docs/ROADMAP.md)
- [Connect to an existing Chrome or Edge](docs/connect-existing-chrome.md)
- [Agent skill and command reference](docs/bk-browser/)

## Quick Start

```sh
# First time: interactive guide to enable Chrome remote debugging
bk setup

# Connect to the user's running Chrome
bk connect

# Open a new tab (inherits the user's login state)
bk open https://example.com

# Get page state (elements + text + viewport)
bk snapshot

# Interact with elements (ref comes from snapshot output)
bk act click --ref 67
bk act type --ref 42 --text "search query"

# Close the session
bk session close
```

## Sessions

A session is a logical connection to the user's Chrome. The default session shares the user's browser context (cookies, login state, tabs).

```sh
# Single agent — operate on user's logged-in sites (default session)
bk connect
bk open https://taobao.com
bk snapshot
bk session close

# Multi-agent parallel — isolated cookies per session
BK_SESSION=agent-a bk connect
BK_SESSION=agent-a bk open https://shop.com
BK_SESSION=agent-a bk snapshot

BK_SESSION=agent-b bk connect
BK_SESSION=agent-b bk open https://shop.com
```

Session management:

```sh
bk session list                     # List all sessions
bk session close                    # Close current session
bk session cookies get              # Get cookies for the current session
```

## Command Reference

### Agent Commands

| Command | Description |
|---------|-------------|
| `setup` | One-time Chrome remote debugging setup (interactive) |
| `connect` | Connect to browser (idempotent) |
| `open` | Open URL in a new tab |
| `attach` | Attach an existing user tab to the default session |
| `snapshot` | Get page state: elements + text + viewport info |
| `find` | Find elements by CSS selector |
| `search` | Search page text |
| `act` | Execute interaction (click, type, fill, press, scroll, hover, focus, select, options, upload, drag) |
| `navigate` | Navigate to URL or back/forward/reload |
| `wait` | Wait for a page condition |
| `evaluate` | Execute JavaScript |
| `network watch` | Observe bounded XHR/fetch response metadata without bodies |
| `download` | Click an element and track its download lifecycle |
| `html` | Get page HTML |
| `console` | Show the console log buffer |
| `pdf` | Generate a PDF of the current target |
| `screenshot` | Take a screenshot |
| `tabs` | List tabs in the session |
| `close` | Close or detach the current tab |
| `status` | Connection status |
| `dialog` | List, accept, dismiss, or configure dialogs for the current session |

### Session Storage Commands

| Command | Description |
|---------|-------------|
| `session close` | Close the current session |
| `session list` | List all sessions |
| `session cookies get` | Get cookies for the current session |
| `session cookies set --file <FILE>` | Set cookies from a JSON file |
| `session cookies clear` | Clear cookies for the current session |
| `session storage local get <KEY>` | Get a localStorage value |
| `session storage local set <KEY> <VALUE>` | Set a localStorage value |
| `session storage export` | Export cookies and localStorage |
| `session storage import <FILE>` | Import storage state |

### Admin Commands

| Command | Description |
|---------|-------------|
| `browser discover` | Discover Chrome and bind the selected session |
| `browser connect` | Connect an endpoint and bind the selected session |
| `browser list` | List connected browsers |
| `browser disconnect` | Disconnect a browser |
| `daemon start` | Start the local daemon |
| `daemon status` | Show daemon status |
| `daemon stop` | Stop the daemon gracefully |

### Developer Commands

| Command | Description |
|---------|-------------|
| `debug block` | Block requests matching a pattern |
| `debug unblock` | Remove request blocking |
| `debug cdp` | Send a raw CDP command |

### act

Execute interactions. The `--ref` value comes from the `ref` field in `bk snapshot` output.

```sh
# Click
bk act click --ref 67
bk act click --x 100 --y 200       # By coordinates

# Type (replaces field content by default)
bk act type --ref 42 --text "hello world"
bk act type --ref 42 --text "append this" --append

# Batch fill stable refs
bk act fill --set ref:42=alpha --set ref:55=beta

# Press keys
bk act press --keys Enter
bk act press --keys Control+a
bk act press --keys Tab Tab Tab

# Scroll page or bring an element into view
bk act scroll --direction down
bk act scroll --direction top
bk act scroll --amount 250
bk act scroll --ref 5
bk act scroll --selector "#main"

# Hover and focus
bk act hover --ref 42
bk act focus --ref 42

# Select dropdown values and inspect options
bk act select --ref 77 --value "option-value"
bk act options --ref 77

# Upload files and drag between elements
bk act upload --ref 3 /path/to/file.pdf
bk act upload --selector "input[type=file]" /path/to/a.pdf /path/to/b.pdf
bk act drag --from-ref 10 --to-ref 20
bk act drag --from-selector "#card-a" --to-selector "#drop-zone"
```

`bk act fill`, `bk act select`, and `bk act options` accept only stable element refs from `bk snapshot`.
`bk act click` returns the action result plus `state_diff`; when a click opens a new tab, the response reports `new_tab`.

| Action | Command |
|--------|---------|
| keys | `bk act press --keys Enter`, `bk act press --keys Control+a` |
| dialog | `bk dialog accept`, `bk dialog dismiss`, `bk dialog policy accept` |

### navigate

```sh
bk navigate https://example.com     # Go to URL
bk navigate --back                  # Go back
bk navigate --forward               # Go forward
bk navigate --reload                # Reload
```

### snapshot

```sh
bk snapshot                         # Elements + page text + viewport
bk snapshot --no-page-text          # Exclude page text
bk snapshot --full                  # Remove the compact element cap
bk snapshot --wait networkidle      # Wait strategy: dom-stable (default), networkidle, none
bk snapshot --max-tokens 512        # Deterministic elements + page_text budget
```

`--max-tokens` accepts `16..100000`. It uses the deterministic estimate
`ceil(serialized UTF-8 JSON bytes / 4)` for the `elements + page_text` content
scope; it is not a model-specific tokenizer. Responses include the
`truncated` field, `token_budget`, and per-field `truncation` metadata.
Without `--max-tokens`, compact and `--full` limits behave as before.

### wait

```sh
bk wait --idle                      # Wait for network idle
bk wait --selector "#login-form"    # Wait for element
bk wait --text "Welcome back"       # Wait for text to appear
bk wait --text-gone "Loading..."    # Wait for text to disappear
bk wait --url "/dashboard"          # Wait for URL to match
bk wait --fn "document.querySelectorAll('li').length > 5"
bk wait --time 2000                 # Fixed delay (ms)
```

### evaluate

```sh
bk evaluate "document.title"
bk evaluate "await fetch('/api').then(r => r.json())"
bk evaluate --file script.js
bk evaluate "extractLongText()" --append-to results.txt
```

`--append-to` is CLI-local: the daemon returns `data.result`, then the CLI
requires that result to be a string and appends its exact UTF-8 bytes to the
file. It does not add a newline and does not echo the long result. Directory
targets, symbolic links, missing parents, and write failures return structured
JSON errors.

### network watch

```sh
bk network watch --pattern "/api/orders" --count 3 --timeout 10000
```

`network watch` observes only XHR/fetch responses whose URL contains the
pattern. It returns JSON after `count` matching responses complete or the
timeout expires, with `stop_reason` and `timed_out`; it is not an infinite
stream. Responses are metadata-only: status, headers, MIME type, encoded size,
and failure metadata are returned, while `body` is always `null` with
`body_omitted=true` and `body_omission_reason="metadata_only"`. The operation
is bounded by `count` and `timeout`; its three CDP event streams are unbounded,
while the out-of-order terminal-event buffer has capacity 256. Terminal-buffer
overflow or event-stream closure stops observation with structured
`stop_reason`, `event_streams`, and `terminal_buffer` metadata.

### download

```sh
mkdir downloads
bk download --ref 42 --output-dir ./downloads --timeout 30000
```

`download` subscribes to Browser download events before clicking the ref,
correlates the main frame and download GUID, and waits for completed or canceled
state. The CLI resolves an existing output directory to an absolute path; the
daemon verifies any reported final path remains inside it. Timeout attempts to
cancel the download, and Chrome download behavior is restored on every handled
exit. `path_verified` is false when Chrome reports completion before a final
filesystem path can be confirmed.

### screenshot

```sh
bk screenshot                       # Viewport screenshot (base64 JSON)
bk screenshot --output page.png     # Save to file
bk screenshot --full-page           # Full scrollable page
```

### pdf

```sh
bk pdf                              # PDF as base64 JSON
bk pdf --output page.pdf             # Save to file
bk pdf --landscape --background      # Landscape with backgrounds
```

For `screenshot` and `pdf`, an output path may be relative to the CLI working
directory, but its parent directory must already exist and its extension must
match the artifact. The CLI sends a canonical absolute path to the daemon. A
saved response contains `file` and `size` without duplicating the base64 data;
an inline response contains `data`, `encoding: "base64"`, and `format`.

### open / attach / close / tabs

```sh
bk open https://example.com         # Open URL in new tab
bk attach github.com                # Attach an existing user tab by URL/title/target substring
bk close                            # Close active tab
bk close --target <targetId>        # Close specific tab
bk tabs                             # List all tabs in session
```

`bk attach` is limited to the default session because user-opened tabs belong
to the browser's default context. Use `bk --session <name> open <url>` for an
isolated session.

`open` and URL navigation accept `http:` and `https:` for every host, including
localhost, loopback addresses, and private networks. They also accept canonical
local and UNC file URLs such as `file:///C:/reports/result.html` and
`file://server/share/result.html`, plus `about:blank`. Active-content, browser-
internal, and unknown schemes such as `javascript:`, `data:`, `chrome:`,
`chrome-extension:`, and `devtools:` are rejected. This policy controls which
URLs browserkit sends to Chrome; Chrome's own file-access and cross-origin
restrictions still apply.

## Global Options

| Option | Description |
|--------|-------------|
| `--session <NAME>` | Session for commands that bind or operate on a session (or `BK_SESSION` env var) |
| `--target <ID>` | Tab for commands that operate on one target (targetId) |
| `--timeout <MS>` | Timeout for `snapshot`, `act`, `navigate`, `evaluate`, `wait`, `network watch`, and `download` |
| `--no-state-diff` | Skip `state_diff` in `act` responses; valid only with `act` |
| `-h, --help` | Print help |
| `--version` | Print version |

Supplying one of these options to a command that does not consume it is an
error; the CLI does not silently ignore unsupported option-command combinations.

## Environment Variables

| Variable | Description |
|----------|-------------|
| `BK_SESSION` | Default session name (equivalent to `--session`) |

## Configuration

Optional config at `~/.bk/config.toml`:

```toml
[daemon]
cleanup_interval_seconds = 60    # how often to check for expired sessions

[limits]
max_sessions = 10                # isolated sessions; default session does not count
max_tabs_per_session = 5         # tabs per session, including default
session_timeout_hours = 72       # idle session timeout
js_timeout_seconds = 0           # 0 = no timeout
```

Defaults are used when the file is absent and for omitted fields in an otherwise
valid file. An existing unreadable or invalid file fails daemon startup; unknown
keys are rejected rather than ignored. Valid ranges are
`cleanup_interval_seconds = 1..3600`, `js_timeout_seconds = 0..3600`,
`max_sessions = 0..1000`, `max_tabs_per_session = 0..1000`, and
`session_timeout_hours = 0..8760`. Zero disables the corresponding JavaScript,
session-count, tab-count, or idle-timeout limit; it is not valid for the cleanup
interval. Browser discovery and connection never launch Chrome or Edge.

## State Persistence

All persistent daemon state is stored in a single schema v1 `~/.bk/state.json` file:

- session metadata: mode, browser host, BrowserContext ID, tabs, active target, timestamps, disconnected flag;
- per-tab ownership (`Owned` or `Attached`).

After a daemon restart, restored sessions are visible but disconnected until an explicit `bk connect`. Attached user tabs are detached from browserkit on close; browserkit-owned tabs are closed in Chrome or Edge. Browser process state and process identifiers are never persisted.

Additional runtime files in `~/.bk/`:
- `daemon.port` — current daemon TCP port
- `daemon.lock` — singleton lock (prevents multiple daemons)
- `daemon.log.YYYY-MM-DD` — daily daemon logs; the seven newest files are retained

Writes are atomic (tmp + rename) and debounced (500ms quiet window) to avoid blocking request handlers. Recoverable write failures remain retryable and are reported through `daemon.status.persistence.last_error`; corrupt or future-schema state disables writes to prevent destructive overwrite.

## Shell Completions

Generate completions for your shell:

```sh
bk completions bash > ~/.local/share/bash-completion/completions/bk
bk completions zsh > ~/.zfunc/_bk
bk completions fish > ~/.config/fish/completions/bk.fish
```

## Acknowledgements

- [cdpkit-rs](https://github.com/yie1d/cdpkit-rs) — the typed Rust CDP client that powers all Chrome communication in browserkit
- [browser-use](https://github.com/browser-use/browser-use) — inspiration for element discovery heuristics, AX tree enrichment, and LLM-friendly page state design
- [openclaw](https://github.com/openclaw/openclaw) — inspiration for aria snapshot approach, role-ref element addressing, and attached browser (user Chrome takeover) patterns
