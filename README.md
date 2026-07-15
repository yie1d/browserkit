# browserkit

A persistent browser runtime for AI agents that attaches to the user's existing Chrome, built on [cdpkit](https://crates.io/crates/cdpkit).

browserkit connects agents to Chrome through a long-running local daemon. It keeps browser connections, tabs, isolated sessions, and page state available across CLI invocations, so agents can observe and act without relaunching or re-authenticating the browser.

The `bk` CLI is the default client. Under the hood, it talks to the daemon over newline-delimited JSON on a local TCP socket.

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
│   observe     act           browser manager         │
└──────────────────────┬──────────────────────────────┘
                       │ typed CDP commands/events
┌──────────────────────▼──────────────────────────────┐
│ cdpkit-rs                                           │
│                                                     │
│   type-safe Chrome DevTools Protocol client         │
└──────────────────────┬──────────────────────────────┘
                       │ CDP WebSocket
┌──────────────────────▼──────────────────────────────┐
│              Chrome / Chromium                       │
└─────────────────────────────────────────────────────┘
```

The daemon is the runtime boundary: it owns persistent browser connections, session state, tab tracking, and debounced state persistence. The CLI is intentionally thin.

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

- Rust 1.74+
- Chrome or Chromium (auto-discovered, or set `chrome_path` in config)

## Build

```sh
git clone https://github.com/yie1d/browserkit
cd browserkit
cargo build --release
# binary: target/release/bk
```

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
bk session cookies                  # Cookie operations
```

## Command Reference

### Primary Commands

| Command | Description |
|---------|-------------|
| `setup` | One-time Chrome remote debugging setup (interactive) |
| `connect` | Connect to browser (idempotent) |
| `snapshot` | Get page state: elements + text + viewport info |
| `act` | Execute interaction (click, type, fill, press, scroll, hover, focus, select, options, upload, drag) |
| `navigate` | Navigate to URL or back/forward/reload |
| `open` | Open URL in a new tab |
| `close` | Close the current tab |
| `tabs` | List tabs in the session |
| `wait` | Wait for a page condition |
| `evaluate` | Execute JavaScript |
| `screenshot` | Take a screenshot |
| `session` | Session management (close/list/cookies) |
| `status` | Connection status |

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
bk snapshot --full                  # No truncation
bk snapshot --wait networkidle      # Wait strategy: dom-stable (default), networkidle, none
```

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
```

### screenshot

```sh
bk screenshot                       # Viewport screenshot (base64 JSON)
bk screenshot --output page.png     # Save to file
bk screenshot --full-page           # Full scrollable page
```

### open / close / tabs

```sh
bk open https://example.com         # Open URL in new tab
bk close                            # Close active tab
bk close --target <targetId>        # Close specific tab
bk tabs                             # List all tabs in session
```

## Global Options

| Option | Description |
|--------|-------------|
| `--session <NAME>` | Target session (or `BK_SESSION` env var) |
| `--target <ID>` | Target tab (targetId) |
| `--timeout <MS>` | Timeout in milliseconds (default: 30000) |
| `--no-state-diff` | Skip state_diff in act responses |
| `--focus` | Bring tab to foreground |
| `-h, --help` | Print help |
| `--version` | Print version |

## Environment Variables

| Variable | Description |
|----------|-------------|
| `BK_SESSION` | Default session name (equivalent to `--session`) |

## Configuration

Optional config at `~/.bk/config.toml`:

```toml
[daemon]
workspace_timeout_minutes = 30   # auto-cleanup idle workspaces (0 = disabled)
cleanup_interval_seconds = 60    # how often to check for expired workspaces
chrome_path = "/usr/bin/chromium" # override Chrome auto-discovery
disable_security = true          # pass --ignore-certificate-errors to Chrome
headless = true                  # set to false to show browser window

[limits]
max_workspaces = 0               # 0 = unlimited
max_tabs_per_workspace = 0       # 0 = unlimited
max_sessions = 10                # isolated sessions; default session does not count
max_tabs_per_session = 5         # tabs per isolated session
session_timeout_hours = 72       # idle session timeout
js_timeout_seconds = 0           # 0 = no timeout
```

`session` is the agent-facing isolation model. Some config and persisted fields still use `workspace` for legacy/internal state while the v2 CLI migrates toward sessions.

## State Persistence

All daemon state is stored in a single `~/.bk/state.json` file:

- Browser connections (host, managed flag, PID)
- Session metadata (tabs, active tab, mode)
- Default session ID

Additional runtime files in `~/.bk/`:
- `daemon.port` — current daemon TCP port
- `daemon.lock` — singleton lock (prevents multiple daemons)

Writes are atomic (tmp + rename) and debounced (500ms quiet window) to avoid blocking request handlers.

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
