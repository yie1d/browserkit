# browserkit

A browser automation toolkit built on [cdpkit](https://crates.io/crates/cdpkit) — a typed Rust CDP client. Provides a CLI (`bk`) and a background daemon for browser automation.

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
│              Chrome / Chromium                      │
└─────────────────────────────────────────────────────┘
```

The daemon runs as a background process, maintains persistent browser connections, and serves all clients over a local TCP socket. State is persisted to `~/.bk/` and restored on restart.

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
# Open a URL (auto-starts daemon, creates workspace)
bk open https://example.com

# Get page state (interactive elements)
bk page state

# Click element by index
bk click --index 3

# Type into element
bk type --index 5 "search query"

# Take a screenshot
bk shot --output page.png

# One-shot fetch (no persistent workspace)
bk fetch https://example.com > page.html

# Stop the daemon
bk daemon stop
```

## Workspaces

Each workspace is an isolated browser context (separate cookies, storage, tabs). Multiple workspaces can share a single Chrome instance.

```sh
bk ws new --label "my session"   # create workspace
bk ws list                        # list all workspaces
bk ws use <wid>                   # set default workspace
bk ws info                        # show current workspace details
bk ws close <wid>                 # close workspace

# Aliases
bk new                            # alias for ws new
bk ls                             # alias for ws list
bk rm <wid>                       # alias for ws close
```

Workspace IDs are 16-char hex strings. Most commands accept a prefix (e.g. `a3f2` instead of `a3f2e1b09c7d4a68`).

## Navigation

```sh
bk goto https://example.com       # navigate (alias: bk nav goto)
bk reload                         # reload page
bk nav back                       # go back
bk nav forward                    # go forward
bk nav url                        # get current URL
bk nav title                      # get page title
bk nav wait                       # wait for page load
```

## Page Interaction

```sh
bk page state                     # list interactive elements with indices
bk page state --screenshot        # include viewport screenshot
bk page search "text"             # find text on page

bk click --index 3                # click element
bk click --x 100 --y 200          # click by coordinates
bk type --index 5 "hello"         # type text
bk scroll down                    # scroll (up/down)
bk hover --index 2                # hover over element
bk focus --index 4                # focus element
bk select --index 6 "option-val"  # select dropdown option
```

## Capture

```sh
bk shot                           # viewport screenshot (base64 to stdout)
bk shot --output page.png         # save to file
bk shot --full-page               # full scrollable page
bk shot --selector "#logo"        # element screenshot
bk shot https://example.com -o out.png  # one-shot (no persistent workspace)

bk pdf --output page.pdf          # generate PDF
bk pdf https://example.com -o out.pdf  # one-shot PDF

bk html                           # get full page HTML
bk html --selector "article"      # get element HTML
```

## JavaScript

```sh
bk eval "document.title"          # evaluate JS expression
bk js eval "1 + 1"                # same, non-awaited
bk js file script.js              # run JS file
```

## Tabs

```sh
bk tab new                        # new tab (becomes active)
bk tab new https://example.com    # new tab with URL
bk tab list                       # list tabs
bk tab switch <tid>               # switch active tab
bk tab close <tid>                # close tab
```

## Browser Management

```sh
bk browser connect localhost:9222  # connect to existing Chrome
bk browser list                    # list connected browsers
bk browser disconnect localhost:9222
```

## Storage

```sh
bk storage cookies get             # get all cookies
bk storage cookies set '[{"name":"k","value":"v","domain":"example.com"}]'
bk storage cookies clear
bk storage local get <key>         # localStorage get
bk storage local set <key> <val>   # localStorage set
bk storage export                  # export all storage state
bk storage import state.json       # import storage state
```

## Debug / Network

```sh
bk debug monitor                   # stream network events
bk debug har https://example.com   # navigate and record HAR (stub: entries always empty)
bk debug block "*.ads.com/*"       # block URL pattern
bk debug unblock
bk debug cdp Page.captureScreenshot '{"format":"png"}'  # raw CDP command
bk debug events --filter "Network" # stream CDP events
```

## Daemon

```sh
bk daemon start                    # start daemon in foreground
bk daemon status                   # show status (port, pid, uptime, workspaces)
bk daemon stop                     # graceful shutdown
```

The daemon auto-starts when any command needs it. Port is stored in `~/.bk/daemon.port`.

## Workspace Resolution

Commands that need a workspace resolve it in this order:

1. `--ws <wid>` flag (or `BK_WS` env var)
2. Daemon default workspace (`bk ws use <wid>`)
3. Auto-detect when exactly one workspace exists
4. Error with a helpful message

## Output Formats

```sh
bk --format text  ws list    # human-readable (default)
bk --format json  ws list    # pretty JSON
bk --format tsv   ws list    # tab-separated (pipe-friendly)
```

## Configuration

Optional config file at `~/.bk/config.toml`:

```toml
[daemon]
workspace_timeout_minutes = 30   # auto-cleanup idle workspaces (0 = disabled)
cleanup_interval_seconds = 60    # how often to check for expired workspaces
chrome_path = "/usr/bin/chromium" # override Chrome auto-discovery
disable_security = true          # pass --ignore-certificate-errors to Chrome
headless = true                  # set to false to show the browser window

[limits]
max_workspaces = 0               # 0 = unlimited
max_tabs_per_workspace = 0       # 0 = unlimited
js_timeout_seconds = 0           # 0 = no timeout
```

## Shell Completions

```sh
bk completions bash >> ~/.bashrc
bk completions zsh  >> ~/.zshrc
bk completions fish > ~/.config/fish/completions/bk.fish
```

## Project Structure

```
src/
├── main.rs              # CLI entry point (clap)
├── lib.rs               # library root
├── client.rs            # TCP client + auto-start logic
├── config.rs            # ~/.bk/config.toml loading
├── error.rs             # unified BkError type
├── browser/
│   ├── finder.rs        # Chrome executable discovery
│   ├── launcher.rs      # Chrome process management
│   └── mod.rs           # CDP connection helpers
├── daemon/
│   ├── mod.rs           # daemon lifecycle (start/stop/port file)
│   ├── state.rs         # DaemonState (Arc<DaemonState>, DashMap)
│   ├── server.rs        # TCP server + workspace cleanup
│   ├── persist.rs       # async debounced state persistence
│   ├── protocol.rs      # newline-delimited JSON protocol
│   └── handler/         # one file per command group
│       ├── workspace.rs, tab.rs, nav.rs, page.rs
│       ├── action.rs, js.rs, storage.rs
│       ├── browser.rs, network.rs, debug.rs
│       └── daemon.rs, common.rs
├── page/
│   ├── navigation.rs    # goto, reload, back, forward, wait
│   ├── interaction.rs   # click, type, scroll, hover, focus, select
│   ├── capture.rs       # screenshot, PDF, HTML
│   └── state.rs         # page element extraction
├── workspace/mod.rs     # Workspace + Tab types
```

## State Persistence

The daemon persists browser connections and workspace metadata to `~/.bk/`:

- `browsers.json` — connected browser hosts
- `workspaces.json` — workspace + tab metadata
- `default_ws` — default workspace ID
- `daemon.port` — current daemon port

On restart, the daemon reconnects to persisted browsers and re-attaches CDP sessions for each tab. Workspaces whose browser is no longer reachable are skipped.

Writes are atomic (write to `.tmp` then rename) and debounced (500ms quiet window) to avoid blocking request handlers.
