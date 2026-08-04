# browserkit Roadmap

## Current State

- browserkit is the persistent browser runtime and agent-facing JSON API.
- cdpkit-rs is the typed CDP protocol layer.
- The default session attaches to the user's browser context.
- Named sessions use isolated BrowserContexts.
- browserkit connects only to already-running Chrome or Edge CDP endpoints and
  never manages the browser process.
- Schema v1 persists sessions and target ownership. Restored sessions remain
  disconnected until an explicit `bk connect`.
- Only the current command, configuration, and persisted-state contracts are
  supported; there is no migration or compatibility layer.
- Network observation, downloads, append-to-file evaluation, and deterministic
  snapshot budgets are available through canonical session commands.
- CI, Rust 1.75 checks, release validation, and cross-platform artifacts are in
  place.
- Daemon requests use a per-daemon token, navigation rejects active-content and
  browser-internal schemes without blocking localhost or canonical `file:`
  URLs, and lifecycle cleanup is serialized against active session work.

## Maintenance Priorities

1. Keep README, CLI help, the bundled skill source, CHANGELOG, and this roadmap
   aligned with each release.
2. Add protocol capabilities to cdpkit first, then consume the released crate
   from browserkit.
3. Preserve session ownership, bounded observation, structured errors, and
   cleanup reporting when adding commands.
4. Add new transports or SDKs only when they reuse the same daemon/runtime
   contract rather than creating a parallel automation model.

Completed implementation records under `docs/superpowers/` are historical
evidence; they are not maintained as the current command or behavior contract.
