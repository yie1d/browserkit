# Changelog

## [Unreleased]

### Changed

- Reduced current documentation to maintained architecture, roadmap, Chrome
  connection, and agent command references; completed implementation plans
  remain available in Git history.
- Release archives now include the browserkit license and a generated
  third-party license report.

## [0.2.0] - 2026-07-20

### Breaking Changes

- Replaced the legacy workspace-first runtime and v1 command aliases with the
  session-only v2 command surface.
- Removed `BK_WS`, `--ws`, and legacy `ws.*`, `tab.*`, `nav.*`, `page.*`, and
  `act.*` daemon routes. Use `BK_SESSION`, `--session`, and the canonical v2
  commands instead.
- Migrated persisted schema v2 state one way into session-only schema v3.

### Added

- Persistent default and isolated sessions with target ownership, restoration,
  idle cleanup, resource limits, and structured disconnect errors.
- Session-native browser attachment, target lifecycle tracking, inspection,
  storage, dialogs, network operations, and developer commands.
- Bounded `network watch` observation for XHR/fetch responses.
- Download lifecycle handling through `bk download`.
- CLI-local `evaluate --append-to <file>` for long string extraction.
- Deterministic `snapshot --max-tokens` budgets with truncation metadata.

### Changed

- Positioned browserkit as a persistent browser runtime for AI agents built on
  the pure-protocol cdpkit-rs layer.
- Upgraded the protocol layer to cdpkit 0.5.0, including explicit WebSocket
  connection handling and durable connection shutdown semantics.
- Kept CLI output JSON-only and made invalid explicit session/target selectors
  fail instead of falling back to active state.

### Migration

- Replace legacy workspace and v1 command invocations with the canonical
  session-oriented commands documented by `bk --help`.
- Replace `BK_WS` and `--ws` with `BK_SESSION` and `--session`.
- Existing schema v2 state is migrated automatically to schema v3 on first
  startup; the migration is intentionally one way.
