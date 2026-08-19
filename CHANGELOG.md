# Changelog

## [Unreleased]

### Added

- Added the client-neutral `BrowserRuntime -> BrowserSession -> Page -> Frame`
  lifecycle SDK, including attach and private-profile launch modes, explicit
  ownership, structured close reports, document epochs, and recursive OOPIF
  Session routing.
- Added compile-checked connect and launch examples with direct CDP escape
  hatches at runtime, page, and frame scope.

### Breaking Changes

- Raised the browserkit crate version to 0.4.0 and upgraded the public protocol
  dependency from cdpkit 0.6.0 to the exact cdpkit 0.7.1 release. Public APIs
  containing cdpkit types now require the 0.7.1 types, owned `Session` handles,
  and asynchronous subscription registration; downstream crates must migrate
  their cdpkit dependency and await subscription setup before enabling a
  domain or triggering events. This is a breaking 0.x public API migration.

### Changed

- Migrated the historical executable surface to cdpkit 0.7.1 sender and event
  contracts so the existing CLI and daemon continue to compile alongside the
  new SDK lifecycle spine.
- Retained ownership of SDK-created targets and isolated BrowserContexts at the
  root runtime so `BrowserRuntime::close()` still cleans them after child
  handles are dropped, without repeating resources already closed explicitly.
- Made public Runtime, Session, and Page close operations cancellation-safe: a
  single managed cleanup continues after caller cancellation, retries share its
  report, and concurrent target destruction is treated as completed cleanup.
- Aligned the maintained guides, bundled `bk` skill, architecture tour, and
  SDK examples with the 0.4.0 lifecycle and launch contracts.

## [0.3.0] - 2026-08-06

### Breaking Changes

- Raised the minimum supported Rust version from 1.75 to 1.88 so browserkit
  can use maintained, security-fixed URL/IDNA and time dependency stacks
  without compatibility pins.

### Changed

- Upgraded the protocol layer to the published cdpkit 0.6.0 release.
  Event watchers now choose an explicit buffer policy: long-lived watchers use
  `Unbounded`, while `wait networkidle` uses `Bounded(256)`. Watchers handle
  decode errors and stream closure explicitly; generated enums and root domain
  imports replace stringly or private generated APIs.

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

- Updated the URL/IDNA and logging time dependency stacks to resolve
  RUSTSEC-2024-0421 and RUSTSEC-2026-0009.
- Removed the unused direct rand dependency and updated the remaining
  transitive rand release to resolve RUSTSEC-2026-0097.
- Restricted the unauthenticated local daemon transport to an ephemeral IPv4
  loopback port; the port must not be exposed or forwarded to a network.
- Restricted `open` and URL navigation to HTTP(S), canonical local/UNC `file:`
  URLs, and `about:blank`, while preserving localhost, loopback, and private
  network access.

### Fixed

- Removed temporary-array borrows from request contract construction.
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
