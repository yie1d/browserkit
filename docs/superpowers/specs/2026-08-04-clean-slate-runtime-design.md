# Browserkit Clean-Slate Runtime Design

## Context

Browserkit has not been put into use. The repository therefore does not need
startup migration, backward-compatible inputs, versioned command naming, or
managed-browser compatibility. Keeping those paths would make an unreleased
architecture look like a supported historical contract.

The product boundary is:

    bk CLI / TCP client
            |
            | authenticated newline-delimited JSON over loopback TCP
            v
    browserkit daemon / Session runtime
            |
            | typed CDP through cdpkit
            v
    an already-running Chrome or Edge debug endpoint

Browserkit connects to a browser. It does not launch, own, restart, or terminate
the browser process. A default Session uses the browser's default context; a
named Session creates an isolated BrowserContext in the same connected browser.
Browserkit may create and own tabs inside either kind of Session.

## Goals

- Make the repository describe one current architecture with no migration
  narrative or compatibility layer.
- Make Session the only persisted runtime boundary.
- Remove the obsolete managed-browser process model and its public fields.
- Keep browser connection reuse safe under concurrent client requests.
- Preserve persistence corruption and future-version protection without adding
  migration behavior.
- Align implementation, tests, help, documentation, and the architecture tour.

## Non-goals

- Launching Chrome or Edge.
- Reconnecting a browser automatically after daemon restart.
- Migrating or backing up any pre-release state format.
- Preserving deprecated command, parameter, element-index, or serialized JSON
  inputs.
- Changing daemon token ACLs or other unrelated platform security behavior.
- Connecting to or controlling the user's normal browser during validation.

## Current State Format

The first supported persistence format is schema version 1:

    {
      "version": 1,
      "sessions": []
    }

The state contains only Session metadata, Session tabs, tab ownership, the
active target, timestamps, and disconnected state. It does not contain browser
process metadata or a migration report.

The loader accepts exactly schema version 1. A missing file produces an empty
state. A malformed file, an unsupported version, or structurally invalid state
disables persistence for the daemon run and exposes the reason through
daemon.status.persistence. The daemon must not overwrite rejected state.
Runtime write failures remain recoverable: they are reported as a transient
persistence error and a later debounced write may retry.

There is no backup, conversion, field translation, or migration report. The
existing v2 fixture, migration module, migration tests, and status field are
deleted.

## Browser Connection Model

Browser contains only the normalized host key and the shared Arc<CDP>
connection. The following historical fields and behaviors are removed:

- managed;
- pid;
- child;
- process-killing Drop behavior;
- managed metadata merging;
- managed-browser persistence and restore;
- stale ~/.bk/chrome-* profile cleanup.

browser_launch_lock becomes browser_connect_lock. It remains an asynchronous
mutex because concurrent clients can otherwise both observe a missing browser,
open duplicate CDP connections, and race to replace the same map entry. The
lock protects only connection creation and second-chance lookup; it does not
imply browser launch capability.

browser.connect, browser.discover, and browser.list no longer emit managed or
pid. Browser disconnect closes browserkit's CDP connection and cleans up
Session-owned resources according to tab ownership; it never terminates the
browser process.

## Restart Behavior

Daemon startup loads Session metadata before advertising readiness. Every
loaded Session is marked disconnected. No network connection is attempted from
persisted state, because the current state does not persist a browser object or
connection endpoint as a restorable runtime resource.

The user or agent must run bk connect again after daemon restart. Binding the
Session to the connected browser refreshes transient CDP session IDs and target
state through the normal live connection path.

## Clean-Slate API

The codebase uses canonical names only. Internal enum variants, comments, test
names, and handler descriptions drop V1, V2, V3, legacy, migration-stage, and
workspace-replacement wording when those terms describe obsolete product
history rather than an external protocol fact.

Compatibility-only behavior is removed:

- numeric element-index actions are removed in favor of current element refs;
- snapshot/state deserialization requires the current fields;
- old workspace parameters do not receive specialized migration guidance;
- debug and network compatibility wrappers are removed and callers use the
  canonical implementation directly;
- removed command families are not maintained as a migration catalog.

Negative CLI tests may still assert the intended current command boundary, but
their names and assertions must be expressed in terms of valid canonical
commands rather than historical releases.

The cleanup must not remove capabilities merely because their implementation
originated in an older revision. Session-native navigation, querying, storage,
network inspection, tab ownership, and structured errors remain current
features. Only alternate historical surfaces and compatibility code are
removed.

## Documentation

README, AGENTS, CHANGELOG, ROADMAP, REDESIGN, command references,
Chrome-connection documentation, and the offline architecture tour are updated
to describe only the clean-slate runtime.

Obsolete design and plan artifacts that describe migration as current behavior
are removed from the working tree. Current documentation states:

- browserkit connects to an already-running CDP-enabled browser;
- Session is the only activity, isolation, and persistence boundary;
- daemon restart restores disconnected Session metadata only;
- browserkit never launches or manages the browser process;
- state schema version 1 has no migration path.

Git history remains the record of previous pre-release designs. The checked-out
repository must not present those designs as supported behavior.

## Error Handling

- Missing state: start with empty state and persistence enabled.
- Invalid JSON or invalid schema v1: start with empty in-memory state,
  persistence disabled, and an explicit status reason.
- Unsupported version: use the same fail-closed behavior; do not convert or
  overwrite it.
- Runtime persistence failure: retain the existing retryable degraded state and
  clear it after a successful later write.
- Browser connection failure: leave the Session disconnected and return the
  existing structured connection error.
- Partial Session cleanup: retain structured cleanup_errors; this is current
  failure reporting, not migration compatibility.

## Testing Strategy

Implementation follows test-driven changes. Tests must demonstrate:

1. schema version 1 serializes only current Session state;
2. browser metadata and migration metadata are absent;
3. missing state loads as empty;
4. malformed, wrong-version, and structurally invalid state fail closed without
   overwriting the input;
5. daemon.status contains persistence health but no migration field;
6. connect, discover, and browser list responses omit managed and pid;
7. concurrent browser connection requests still use one connection-creation
   critical section;
8. element actions and serialized page state accept only current formats;
9. canonical CLI commands continue to parse and route correctly;
10. browser disconnect and daemon shutdown do not own or terminate a browser
    process.

Validation includes Rust formatting, targeted tests during implementation,
cargo test --all-targets --locked, git diff --check, and repository-wide
searches for unintended migration, managed-browser, versioned-command, and
compatibility remnants. Historical words that are technically necessary in a
third-party protocol or changelog-independent legal notice must be reviewed
manually rather than removed mechanically.

No validation step may attach to, launch, close, or terminate the user's normal
Chrome. Browser acceptance testing, if later required, must use an explicitly
created temporary profile and dedicated debug port.

## Completion Criteria

The change is complete when implementation, tests, public JSON, help, and active
documentation expose one Session runtime; the repository contains no executable
migration path or managed-browser process model; the current state schema is
version 1; all relevant automated checks pass; and remaining uses of historical
terminology have been individually justified as current domain language.
