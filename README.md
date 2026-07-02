# browserkit

Browser automation CLI for LLM agents, built on [cdpkit](https://crates.io/crates/cdpkit). Connects to the user's own Chrome through persistent CDP sessions. All output is JSON.

## Architecture

```
┌─────────────────────────────────────────────────────┐
│  bk CLI  /  any TCP client                          │
└──────────────────────┬──────────────────────────────┘
                       │  newline-delimited JSON (TCP)
┌──────────────────────▼──────────────────────────────┐
│                   bk daemon                         │
│  ┌─────────────┐  ┌──────────────┐  ┌───────────┐  │
│  │  sessions   │  │   browsers   │  │  persist  │  │
│  │  (DashMap)  │  │  (DashMap)   │  │  (async)  │  │
│  └─────────────┘  └──────────────┘  └───────────┘  │
└──────────────────────┬──────────────────────────────┘
                       │  CDP WebSocket
┌──────────────────────▼──────────────────────────────┐
│              Chrome / Chromium                       │
└─────────────────────────────────────────────────────┘
```

The daemon runs in the background, maintains persistent browser connections, and serves clients over a local TCP socket. State is persisted to `~/.bk/` and restored on restart.

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
| `act` | Execute interaction (click, type, press) |
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

# Press keys
bk act press --keys Enter
bk act press --keys Control+a
bk act press --keys Tab Tab Tab
```

Phase 2 actions (via legacy commands, migrating to `act` in Phase 3):

| Action | Command |
|--------|---------|
| fill | `bk fill --set ref:42=value --set ref:55=other` |
| select | `bk select --ref 77 "option-value"` |
| scroll | `bk scroll down`, `bk scroll top`, `bk scroll --ref 5` |
| hover | `bk hover --ref 42` |
| drag | `bk drag --from-ref 10 --to-ref 20` |
| upload | `bk upload --ref 3 /path/to/file.pdf` |
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
js_timeout_seconds = 0           # 0 = no timeout
```

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
