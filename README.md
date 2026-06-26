# browserkit

Browser automation CLI and daemon built on [cdpkit](https://crates.io/crates/cdpkit). Controls headless or visible Chrome instances through persistent CDP connections.

## Architecture

```
┌─────────────────────────────────────────────────────┐
│  bk CLI  /  any TCP client                          │
└──────────────────────┬──────────────────────────────┘
                       │  newline-delimited JSON (TCP)
┌──────────────────────▼──────────────────────────────┐
│                   bk daemon                         │
│  ┌─────────────┐  ┌──────────────┐  ┌───────────┐  │
│  │  workspaces │  │   browsers   │  │  persist  │  │
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
# Open a URL (auto-starts daemon, launches Chrome, creates workspace)
bk open https://example.com

# See interactive elements on the page
bk info

# Click an element by its index
bk click --index 3

# Type into a form field
bk type --index 2 "hello world"

# Take a screenshot
bk shot --output page.png

# One-shot fetch rendered HTML (ephemeral, no persistent workspace)
bk fetch https://example.com > page.html
```

## Connecting to Your Own Chrome

You can attach to an already-running Chrome (with remote debugging enabled) instead of launching a new instance.

```sh
# Auto-discover Chrome via DevToolsActivePort file
bk browser discover

# Or connect by host:port manually
bk browser connect localhost:9222

# Create a workspace that shares the user's browser context (no isolation)
bk ws new --attached

# Or attach only tabs matching a pattern
bk ws new --attached --pattern "github.com"

# Attach a specific tab into an existing workspace
bk tab attach "github.com"
```

Start Chrome with remote debugging:
```sh
chrome --remote-debugging-port=9222
```

## Command Reference

### Navigation

```sh
bk goto <URL>                       # Navigate to URL
bk goto https://example.com
bk goto file:///tmp/test.html

bk back                             # Go back in history
bk forward                          # Go forward in history
bk reload                           # Reload current page

bk wait                             # Wait for networkidle (default)
bk wait --selector "#login-form"    # Wait for element to appear
bk wait --text "Welcome back"       # Wait for text
bk wait --text-gone "Loading..."    # Wait for text to disappear
bk wait --url "/dashboard"          # Wait for URL match
bk wait --fn "document.querySelectorAll('li').length > 5"
bk wait --time 2000                 # Fixed delay (ms)
bk wait --load-state networkidle    # Explicit load state
```

### Interaction

```sh
# Click
bk click --index 3                  # By element index
bk click --ref 42                   # By backendNodeId (stable ref)
bk click --x 100 --y 200           # By coordinates

# Type
bk type --index 2 "hello world"
bk type --ref 55 --clear "new value"   # Clear first, then type
bk type --index 0 --autocomplete "react"  # Wait for autocomplete dropdown

# Fill (batch form fill)
bk fill --set 0=John --set 1=Doe --set 2=john@example.com
bk fill --set ref:42=hello --set ref:55=world

# Select dropdown
bk select --index 3 "United States"
bk select --ref 77 "option-value"

# Scroll
bk scroll down                      # Default direction
bk scroll up --amount 200           # Custom amount (px)
bk scroll top                       # Scroll to top
bk scroll bottom                    # Scroll to bottom
bk scroll --index 5                 # Scroll element into view
bk scroll --selector "#footer"      # Scroll to CSS selector

# Hover
bk hover --index 3
bk hover --ref 42

# Drag
bk drag --from-index 2 --to-index 5
bk drag --from-ref 10 --to-ref 20
bk drag --from-selector ".item" --to-selector ".dropzone"

# Focus
bk focus --index 2
bk focus --ref 42

# Upload files (absolute paths required)
bk upload --index 3 /path/to/file.pdf
bk upload --selector "input[type=file]" /tmp/a.png /tmp/b.png

# Keyboard
bk keys Enter
bk keys Tab Tab Tab
bk keys Control+a
bk keys Control+Shift+Enter
bk keys Escape
```

### Page State

```sh
# Interactive elements + page text + viewport info
bk info
bk info --no-text                   # Exclude page text
bk info --screenshot                # Include viewport screenshot
bk info --tree                      # Elements in tree format
bk info --ax                        # Include accessibility info

# Find elements by CSS selector
bk find "a[href]" --attributes href,class --include-text
bk find ".error" --max 10
bk find "input[type=text]"

# Search text in page content
bk search "error message"
bk search "\\d{3}-\\d{4}" --regex
bk search "price" --scope ".product-card" --max 5

# JavaScript evaluation
bk eval "document.title"
bk eval "await fetch('/api').then(r => r.json())"
bk eval --sync "1 + 1"
bk eval --file script.js

# Page HTML
bk html
bk html --selector "#main-content"

# URL and title
bk url
bk title

# Console log buffer
bk console
bk console --level error
bk console --level warn --limit 20

# List <select> options
bk options --index 3
bk options --ref 77
```

### Output

```sh
# Screenshot
bk shot                             # Viewport screenshot (base64)
bk shot --output page.png           # Save to file
bk shot --full-page                 # Full scrollable page
bk shot --selector ".hero"          # Element screenshot
bk shot --labels                    # Overlay index labels
bk shot https://example.com -o example.png  # One-shot mode

# PDF
bk pdf --output page.pdf
bk pdf https://example.com --output report.pdf
```

### One-shot

These commands create a temporary workspace, perform an action, then clean up.

```sh
# Open URL in new persistent workspace (becomes default)
bk open https://example.com
bk open https://app.local --no-headless

# Fetch rendered HTML (ephemeral workspace, auto-closed)
bk fetch https://example.com
bk fetch https://spa-app.com/page
```

### Workspaces

Each workspace is an isolated browser context with independent cookies, storage, and tabs. Multiple workspaces can share one Chrome instance.

```sh
bk ws new                           # Create workspace (launches Chrome if needed)
bk ws new --label "session-1"       # With label
bk ws new --no-headless             # Show browser window
bk ws new --attached                # Attached mode (share user's context)
bk ws new --attached --pattern "github"  # Only tabs matching pattern

bk ws attach                        # Attach existing tabs into new workspace
bk ws attach --pattern "google"     # Filter by URL/title pattern

bk ws list                          # List all workspaces
bk ws info                          # Show workspace details
bk ws use <wid>                     # Set default workspace
bk ws default                       # Show current default workspace
bk ws close <wid>                   # Close workspace

# Aliases
bk new                              # = bk ws new
bk ls                               # = bk ws list
bk rm <wid>                         # = bk ws close
```

Workspace IDs are 16-char hex strings. Most commands accept a prefix (e.g. `a3f2` instead of `a3f2e1b09c7d4a68`).

### Tabs

Each workspace has one or more tabs. Tabs have short aliases (`t1`, `t2`, ...) for quick reference.

```sh
bk tab new                          # New tab (about:blank)
bk tab new https://example.com      # New tab with URL
bk tab attach "github.com"          # Attach existing tab by URL/title match
bk tab list                         # List tabs in workspace
bk tab switch t2                    # Switch active tab (alias or tid)
bk tab close t1                     # Close a tab
```

### Browser Management

```sh
bk browser discover                 # Auto-discover Chrome (DevToolsActivePort)
bk browser discover --path /custom/path
bk browser connect localhost:9222   # Connect to existing Chrome
bk browser list                     # List connected browsers
bk browser disconnect localhost:9222
```

### Storage

```sh
bk storage cookies get              # Get all cookies
bk storage cookies set '[{"name":"k","value":"v","domain":"example.com"}]'
bk storage cookies clear

bk storage local get <key>          # Get localStorage value
bk storage local set <key> <val>    # Set localStorage value

bk storage export                   # Export all storage state
bk storage import state.json        # Import from file
```

### Dialogs

JavaScript dialog handling (alert, confirm, prompt).

```sh
bk dialog list                      # List pending dialogs
bk dialog accept                    # Accept (confirm) pending dialog
bk dialog dismiss                   # Dismiss (cancel) pending dialog

bk dialog policy                    # View current policy
bk dialog policy manual             # Manual handling (default)
bk dialog policy accept             # Auto-accept all dialogs
bk dialog policy dismiss            # Auto-dismiss all dialogs
```

### Debug

```sh
bk debug monitor                    # Stream network requests (live)
bk debug har https://example.com    # Navigate and record HAR
bk debug block "*.ads.com/*"        # Block URL pattern
bk debug unblock                    # Remove all blocks

bk debug cdp Page.captureScreenshot '{"format":"png"}'  # Raw CDP command
bk debug events                     # Stream all CDP events
bk debug events --filter "Network"  # Filter by domain
```

### Daemon

```sh
bk daemon start                     # Start in foreground
bk daemon stop                      # Graceful shutdown
bk daemon status                    # Show status
```

The daemon auto-starts when any command needs it.

### Status

```sh
bk status                           # Daemon + browser + workspace summary
bk status --format json
```

## Global Options

| Option | Description |
|--------|-------------|
| `-w, --ws <ID>` | Target workspace (or `BK_WS` env var) |
| `--format <FMT>` | Output format: `text` (default), `json`, `tsv` |
| `-h, --help` | Print help |
| `--version` | Print version |

## Workspace Resolution

Commands that need a workspace resolve it in this order:

1. `--ws <wid>` flag (or `BK_WS` env var)
2. Daemon default workspace (`bk ws use <wid>`)
3. Auto-detect when exactly one workspace exists
4. Error with a helpful message

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
- Workspace metadata (tabs, active tab, label, mode)
- Default workspace ID

Additional runtime files in `~/.bk/`:
- `daemon.port` — current daemon TCP port
- `daemon.lock` — singleton lock (prevents multiple daemons)

On restart, the daemon reconnects to persisted managed browsers and re-attaches CDP sessions for each tab. Unmanaged browsers (user-connected via `discover`/`connect`) are not persisted.

Writes are atomic (tmp + rename) and debounced (500ms quiet window) to avoid blocking request handlers.

## Project Structure

```
src/
├── main.rs                # CLI entry point (clap)
├── lib.rs                 # library root
├── client.rs              # TCP client + daemon auto-start
├── config.rs              # ~/.bk/config.toml loading
├── error.rs               # unified BkError type
├── browser/
│   ├── mod.rs             # CDP connection helpers
│   ├── finder.rs          # Chrome executable discovery
│   ├── launcher.rs        # Chrome process management
│   └── discover.rs        # DevToolsActivePort auto-discovery
├── daemon/
│   ├── mod.rs             # daemon lifecycle (start/stop/port file)
│   ├── state.rs           # DaemonState (Arc + DashMap)
│   ├── server.rs          # TCP server + workspace cleanup
│   ├── persist.rs         # async debounced state persistence
│   ├── protocol.rs        # newline-delimited JSON protocol
│   ├── auto_attach.rs     # auto-attach target tracking
│   ├── console.rs         # console log subscription
│   ├── dialog.rs          # dialog event subscription
│   └── handler/           # one file per command group
│       ├── mod.rs         # dispatcher
│       ├── workspace.rs   # ws new/attach/list/close/use/default
│       ├── tab.rs         # tab new/attach/list/switch/close
│       ├── nav.rs         # goto/back/forward/reload
│       ├── page.rs        # info/find/search/html/url/title/options
│       ├── action.rs      # click/type/fill/select/scroll/hover/drag/focus/upload/keys
│       ├── js.rs          # eval
│       ├── storage.rs     # cookies/local/export/import
│       ├── browser.rs     # connect/discover/list/disconnect
│       ├── network.rs     # monitor/har/block/unblock
│       ├── debug.rs       # cdp/events
│       ├── dialog.rs      # dialog list/accept/dismiss/policy
│       ├── daemon.rs      # start/stop/status
│       └── common.rs      # shared handler utilities
├── page/
│   ├── mod.rs             # page module root
│   ├── navigation.rs      # goto, reload, back, forward, wait
│   ├── interaction.rs     # click, type, scroll, hover, focus, select, drag
│   ├── capture.rs         # screenshot, PDF, HTML
│   ├── state.rs           # page element extraction
│   ├── find_elements.rs   # CSS selector queries
│   ├── element_ref.rs     # backendNodeId resolution
│   └── wait.rs            # wait conditions + networkidle
└── workspace/
    └── mod.rs             # Workspace + Tab types
```

## Acknowledgements

- [cdpkit-rs](https://github.com/yie1d/cdpkit-rs) — the typed Rust CDP client that powers all Chrome communication in browserkit
- [browser-use](https://github.com/browser-use/browser-use) — inspiration for element discovery heuristics, AX tree enrichment, and LLM-friendly page state design
- [openclaw](https://github.com/openclaw/openclaw) — inspiration for aria snapshot approach, role-ref element addressing, and attached browser (user Chrome takeover) patterns
