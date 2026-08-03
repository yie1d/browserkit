# Browserkit Runtime Hardening Design

## Scope

This change hardens the current session-only runtime without adding compatibility aliases or a separate file-opening API. The open and navigate commands remain the only URL entry points.

## URL policy

- Allow HTTP and HTTPS for every host, including localhost, loopback, and private networks.
- Allow valid file URLs, including Windows drive paths encoded as file URLs and UNC file URLs. Chrome remains responsible for file existence and access errors.
- Allow only about:blank from the about scheme.
- Reject javascript, data, browser-internal schemes, and unknown schemes before CDP is called.
- Trim surrounding whitespace before parsing so policy checks cannot be bypassed.

## Runtime lifecycle

- An owned target is transactional: until registration succeeds, every failure closes the newly created target.
- Per-session tab capacity is reserved atomically before target creation and released on every failed path. Registration consumes the reservation.
- Every request operating on a session records activity centrally and requests debounced persistence.
- Session commands, target watcher events, idle cleanup, session close, and browser disconnect use the same per-session lifecycle gate before mutating session-owned targets.
- The global lock order is session bind, then session lifecycle, then target registration, then a DashMap entry. Ordinary commands and watcher events share lifecycle read access; cleanup, close, disconnect, and bind take lifecycle write access. No DashMap guard is held across an await.
- Target ownership lookup, registration, reservation consumption, and removal are serialized by the target-registration lock. Any owner discovered before awaiting a lifecycle lock is revalidated after the lock is acquired.
- Idle cleanup and explicit close/disconnect operations rebuild or revalidate their target plan after acquiring the lifecycle gate. New activity or target creation therefore cancels an outdated cleanup decision instead of being removed by a stale snapshot.
- Watcher events that resume after a session has been removed or disconnected become no-ops and detach any CDP session they initialized but could not register.
- Zero cleanup intervals are rejected by configuration validation, and timeout arithmetic is saturating.

## Operational reliability

- Persistence writes distinguish incompatible on-disk state from recoverable runtime I/O failures. Future-schema state remains fail-closed for the daemon run; ordinary write or worker failures expose a degraded reason but later debounced requests retry automatically. A successful retry clears the transient degraded reason.
- Protocol errors always use the structured v2 error object.
- Daemon logs rotate daily, default to info, and avoid logging raw search text, dialog content, selectors, or URL query/fragment data. Retention deletes only regular files matching the appender's exact `daemon.log.YYYY-MM-DD` naming form.
- Remove dead managed-Chrome launcher configuration and code; browser discovery and connection remain supported.
- Add a pinned RustSec audit job to CI.
- Correct project-agent memory documentation so it does not require a nonexistent repository path.

## Maintainability

New policy and lifecycle logic lives in focused modules. Existing large command files are not mechanically rewritten: only code directly touched by these fixes is extracted, minimizing regression risk while preventing further growth of the hotspots.

## Verification

- Add deterministic regression tests for watcher registration/removal racing session cleanup and target ownership decisions.
- Add persistence tests proving a transient failure does not permanently disable later writes while future-schema protection remains permanent.
- Add log-pruning tests proving similarly prefixed files are preserved.
- Run `cargo test --all-targets --locked`, `cargo clippy --all-targets --locked -- -D warnings`, `cargo build --locked`, `cargo fmt --check`, and `git diff --check`.
- Do not attach to, launch, close, or terminate the user's normal Chrome. Real Chrome acceptance, if needed later, must use an isolated temporary profile and debugging port.
