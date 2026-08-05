# Changelog

## [Unreleased]

### Changed

- Path-bound the unpublished runtime to cdpkit 0.6.0 for joint verification.
  Event watchers now choose an explicit buffer policy: long-lived watchers use
  `Unbounded`, while `wait networkidle` uses `Bounded(256)`. Watchers handle
  decode errors and stream closure explicitly; generated enums and root domain
  imports replace stringly or private generated APIs. This does not claim that
  cdpkit 0.6.0 is available from crates.io.

- Reduced current documentation to maintained architecture, roadmap, Chrome
  connection, and agent command references; completed implementation plans
  remain available in Git history.
- Release archives now include the browserkit license and a generated
  third-party license report.
- Browser connections now target already-running Chrome or Edge CDP endpoints;
  browserkit never launches, manages, or terminates the browser process.
- Defined the first formal persisted-state contract as strict schema v1 with no
  migration or compatibility layer. Restored sessions require `bk connect`.
- Documented the canonical URL policy consistently across the README,
  architecture, bundled skill, and command reference.
- Made existing invalid configuration fatal with explicit numeric bounds.
- Unified screenshot and PDF output paths and response payloads, exposed PDF
  landscape/background flags, and made JavaScript evaluation always await
  promises.
- Added bounded daemon connect, handshake, request, readiness, and idle
  connection deadlines.

### Security

- Restricted the unauthenticated local daemon transport to an ephemeral IPv4
  loopback port; the port must not be exposed or forwarded to a network.
- Restricted `open` and URL navigation to HTTP(S), canonical local/UNC `file:`
  URLs, and `about:blank`, while preserving localhost, loopback, and private
  network access.

### Fixed

- Restored the declared Rust 1.75 build by pinning the URL stack to an
  MSRV-compatible release and removing temporary-array borrows from request
  contract construction.
- Serialized target ownership changes and destructive session lifecycle work
  without blocking watcher-driven new-tab registration during ordinary actions.
- Kept transient persistence failures retryable and exposed the latest error in
  daemon status.
- Limited daemon log pruning to dated `daemon.log.YYYY-MM-DD` files.

## [0.2.0] - 2026-07-20

### Added

- Persistent default and isolated sessions with target ownership, restoration,
  idle cleanup, resource limits, and structured disconnect errors.
- Browser attachment, target lifecycle tracking, inspection,
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
- Kept runtime command results JSON-only (help and shell completions remain
  text) and made invalid explicit session/target selectors fail instead of
  falling back to active state.
