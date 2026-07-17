# bk command reference

> Current contract from `bk --help`, `bk session --help`, and `bk debug --help`.
> All command output is JSON. `bk` is the thin CLI client for the local
> browserkit daemon.

## Global options

| Option | Meaning |
|---|---|
| `--session <NAME>` | Target session, or set `BK_SESSION` |
| `--target <ID>` | Target tab ID |
| `--timeout <MS>` | Timeout in milliseconds |
| `--no-state-diff` | Skip `state_diff` in act responses |
| `--focus` | Bring the target tab to the foreground |
| `--help` | Print help |
| `--version` | Print version |

## Agent commands

Use these for normal browser work.

| Command | Purpose |
|---|---|
| `bk setup` | One-time Chrome remote debugging setup |
| `bk connect` | Connect to the user's Chrome, idempotent |
| `bk open <URL>` | Open a browserkit-owned tab |
| `bk attach [PATTERN]` | Attach an existing user tab to the default session |
| `bk snapshot [--full] [--no-page-text] [--wait dom-stable|networkidle|none] [--max-tokens <16..100000>]` | Get elements, page text, viewport, and truncation metadata |
| `bk find <SELECTOR> [--attributes <NAMES>] [--include-text] [--max <N>]` | Find elements by CSS selector |
| `bk search <TEXT> [--regex] [--scope <SCOPE>] [--context <N>] [--max <N>]` | Search text in the page |
| `bk act click --ref <N>` | Click by snapshot ref |
| `bk act click --x <X> --y <Y>` | Click by coordinates |
| `bk act type --ref <N> --text <TEXT> [--append]` | Type text, replacing by default |
| `bk act fill --set ref:<N>=<VALUE>` | Fill one or more fields |
| `bk act press --keys <KEYS>...` | Press keys such as `Enter` or `Control+a` |
| `bk act scroll [--direction <DIR>] [--amount <PX>] [--ref <N>] [--selector <CSS>]` | Scroll page or element |
| `bk act hover --ref <N>` | Hover an element |
| `bk act focus --ref <N>` | Focus an element |
| `bk act select --ref <N> --value <VALUE>` | Select an option |
| `bk act options --ref <N>` | Inspect select options |
| `bk act upload --ref <N> <FILES...>` | Upload files |
| `bk act upload --selector <CSS> <FILES...>` | Upload with a CSS selector |
| `bk act drag --from-ref <N> --to-ref <N>` | Drag between refs |
| `bk act drag --from-selector <CSS> --to-selector <CSS>` | Drag between selectors |
| `bk navigate <URL>` | Navigate current target |
| `bk navigate --back` | Go back |
| `bk navigate --forward` | Go forward |
| `bk navigate --reload` | Reload |
| `bk wait --selector <CSS>` | Wait for an element |
| `bk wait --text <TEXT>` | Wait for text |
| `bk wait --text-gone <TEXT>` | Wait for text to disappear |
| `bk wait --url <PATTERN>` | Wait for URL match |
| `bk wait --idle` | Wait for network idle |
| `bk wait --fn <EXPR>` | Wait for JavaScript truthy |
| `bk wait --time <MS>` | Fixed wait |
| `bk evaluate <EXPR>` | Evaluate JavaScript |
| `bk evaluate --file <PATH>` | Evaluate JavaScript from a file |
| `bk evaluate <EXPR> --append-to <FILE>` | Append an exact string result locally without echoing it |
| `bk network watch --pattern <SUBSTRING> [--count <1..100>]` | Observe bounded XHR/fetch response metadata without bodies |
| `bk download --ref <N> --output-dir <DIR>` | Click and track one download to terminal state |
| `bk html [--selector <CSS>]` | Get page or element HTML |
| `bk console [--level <LEVEL>] [--limit <N>]` | Show console buffer |
| `bk pdf [-o <FILE>]` | Generate PDF of current target |
| `bk screenshot [--output <FILE>] [--full-page] [--selector <CSS>] [--labels]` | Capture screenshot |
| `bk tabs` | List tabs tracked by the current session |
| `bk close` | Close owned tab or detach attached tab |
| `bk status` | Show daemon/browser/session status |
| `bk dialog list` | List pending dialogs |
| `bk dialog accept` | Accept a pending dialog |
| `bk dialog dismiss` | Dismiss a pending dialog |
| `bk dialog policy [manual|accept|dismiss]` | View or set dialog policy |

`bk act click` reports `new_tab` when the action opens a new target. Use the
reported target ID or run `bk tabs` before operating on the new page.

`snapshot --max-tokens` uses `ceil(serialized UTF-8 JSON bytes / 4)` for the
`elements + page_text` scope. Read `token_budget` and `truncation` in the
response; this is deterministic but not a model-specific tokenizer. Omitting
the flag preserves compact/`--full` content limits.

`evaluate --append-to` is CLI-local and accepts only string results. It appends
exact UTF-8 bytes with no implicit newline. `network watch` stops at count or
timeout and reports `stop_reason`; it is not a stream. Network bodies are never
read: `body` is `null` with omission metadata. Its three event streams and
out-of-order terminal buffer each have capacity 256; inspect `event_streams`
and `terminal_buffer` for overflow, close, and dropped-event metadata. Reaching
the terminal-buffer capacity stops immediately with
`stop_reason="terminal_buffer_overflow"`.
`download` requires an existing output directory, validates the final path,
and restores Browser download behavior after the lifecycle.

## Session storage commands

| Command | Purpose |
|---|---|
| `bk session close` | Close current session |
| `bk session list` | List sessions |
| `bk session cookies get` | Get cookies |
| `bk session cookies set --file <FILE>` | Set cookies from JSON file |
| `bk session cookies clear` | Clear cookies |
| `bk session storage local get <KEY>` | Get a localStorage value |
| `bk session storage local set <KEY> <VALUE>` | Set a localStorage value |
| `bk session storage export` | Export all storage state |
| `bk session storage import <FILE>` | Import storage state |

Default session shares the user's Chrome context. Named sessions use isolated
BrowserContext storage:

```bash
bk --session agent-a connect
bk --session agent-a open https://example.com
bk --session agent-a session cookies get
```

## Admin commands

Use these to manage browser and daemon connections.

| Command | Purpose |
|---|---|
| `bk browser discover [--path <DevToolsActivePort>]` | Discover Chrome and bind the selected session |
| `bk browser connect <HOST_OR_WS_URL>` | Connect an endpoint and bind the selected session |
| `bk browser list` | List connected browsers |
| `bk browser disconnect <HOST>` | Disconnect a browser |
| `bk daemon start` | Start daemon |
| `bk daemon status` | Show daemon status |
| `bk daemon stop` | Stop daemon gracefully |

Prefer `bk connect` for normal use. Do not assume port 9222; Chrome's
`DevToolsActivePort` file is the source of truth for user-browser endpoints.

## Developer commands

Use these only for diagnostics or controlled debugging.

| Command | Purpose |
|---|---|
| `bk debug block <PATTERN>` | Block matching requests |
| `bk debug unblock` | Remove request blocking |
| `bk debug cdp <METHOD> [PARAMS]` | Send a raw CDP command |

## Breaking migration notes

The workspace runtime was removed in a breaking migration. Do not use or
document compatibility aliases for these surfaces:

- CLI: `ws`, `tab`, `fetch`, old top-level action/navigation aliases, and
  top-level `storage`;
- environment and flags: `BK_WS`, `--ws`;
- daemon routes: `ws.*`, `tab.*`, `nav.*`, `page.*`, old `storage.*`, and
  `v2.*` aliases;
- debug streams: `debug monitor`, `debug har`, and `debug events`.

Schema v2 state is backed up before migration to schema v3. `bk status` reports
migration metadata. If writes are disabled, `persistence.enabled` is false and
`persistence.disabled_reason` explains the preserved-state error. Cleanup commands return structured `cleanup_errors` when
cleanup is partial. Schema v3 state contains browser metadata, sessions, tab
ownership, and optional migration metadata, but no workspace fields.
