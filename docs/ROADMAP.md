# browserkit Roadmap

## Current State

The v2 session-runtime migration is complete as of browserkit 0.2.0.

- browserkit is the persistent browser runtime and agent-facing JSON API.
- cdpkit-rs is the typed CDP protocol layer.
- The default session attaches to the user's browser context.
- Named sessions use isolated BrowserContexts.
- Schema v3 persists sessions and target ownership.
- Schema v2 workspace state is migrated once with a backup and visible report.
- Workspace commands, v1 aliases, and legacy daemon routes are removed.
- Network observation, downloads, append-to-file evaluation, and deterministic
  snapshot budgets are available through canonical session commands.
- CI, Rust 1.75 checks, release validation, and cross-platform artifacts are in
  place.

## Maintenance Priorities

1. Keep README, CLI help, the bundled skill source, CHANGELOG, and this roadmap
   aligned with each release.
2. Add protocol capabilities to cdpkit first, then consume the released crate
   from browserkit.
3. Preserve session ownership, bounded observation, structured errors, and
   cleanup reporting when adding commands.
4. Add new transports or SDKs only when they reuse the same daemon/runtime
   contract rather than creating a parallel automation model.

Completed implementation checklists remain available in Git history; they are
not maintained as current documentation.
