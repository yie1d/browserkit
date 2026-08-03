# Changelog

## [Unreleased]

### Changed

- Reduced current documentation to maintained architecture, roadmap, Chrome
  connection, and agent command references; completed implementation plans
  remain available in Git history.
- Release archives now include the browserkit license and a generated
  third-party license report.
- Removed current managed-Chrome launch configuration and kept browser
  discovery/connection focused on an already-running user browser.
- Documented the canonical URL policy consistently across the README,
  architecture, bundled skill, and command reference.

### Security

- Authenticated loopback daemon requests with a per-daemon token.
- Restricted `open` and URL navigation to HTTP(S), canonical local/UNC `file:`
  URLs, and `about:blank`, while preserving localhost, loopback, and private
  network access.

### Fixed

- Serialized target ownership changes and destructive session lifecycle work
  without blocking watcher-driven new-tab registration during ordinary actions.
- Kept transient persistence failures retryable and exposed the latest error in
  daemon status.
- Limited daemon log pruning to dated `daemon.log.YYYY-MM-DD` files.

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
