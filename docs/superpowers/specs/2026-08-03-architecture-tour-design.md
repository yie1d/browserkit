# Browserkit Architecture Tour Design

## Goal

Create a self-contained, offline HTML architecture tour at
`docs/architecture-tour.html`. It should let a maintainer understand the real
browserkit runtime by navigating its layers, selecting architecture nodes, and
stepping through representative runtime flows without running a server or
connecting to Chrome.

## Source of truth

The page is derived from the current implementation, primarily:

- `src/main.rs` and `src/client.rs` for CLI and daemon startup;
- `src/daemon/protocol.rs`, `server.rs`, `state.rs`, and `handler/mod.rs` for the
  authenticated runtime boundary and request dispatch;
- `src/daemon/session.rs`, `target_lifecycle.rs`, and `target_close.rs` for
  session isolation, target ownership, and cleanup;
- `src/daemon/persist.rs` for schema v3 persistence and migration;
- `src/browser/` and `src/page/` for CDP connection and page capabilities.

The maintained README and command reference supply user-facing terminology.
Historical plans are context only and must not override current code.

## Deliverable

One standalone UTF-8 HTML file with inline CSS, inline JavaScript, and inline
SVG. It must not use external fonts, libraries, CDNs, network requests, build
steps, or a local server. Opening the file directly in a modern browser must be
enough.

## Information architecture

The page uses a persistent section navigator on wide screens and a compact
section selector on narrow screens. It contains these sections:

1. Overview and system positioning.
2. Process topology from Agent through `bk`, daemon, cdpkit, and Chrome.
3. CLI and daemon startup lifecycle.
4. Browser, Session, Tab, and ownership model.
5. Request routing and handler/page layering.
6. Concurrency gates and fixed lock order.
7. Persistence, migration, cleanup, and failure semantics.
8. Security boundary and URL policy.
9. Source tree map.
10. Design assessment, current limitations, and historical internal edges.

## Primary interactions

### Architecture explorer

The dominant visual is an inline SVG topology. Each labeled node is a native
button or keyboard-accessible SVG control. Selecting a node updates one detail
area with its responsibility, inputs, outputs, state ownership, and relevant
repository files. Connections show request, CDP, event, and persistence flow
with stable labels and arrow direction.

### Runtime flow explorer

A compact native control switches among four flows:

- first `bk` command and daemon auto-start;
- `bk open` and owned-tab registration;
- `act click` opening a new tab through the target watcher;
- Chrome WebSocket disconnect and session degradation.

Each flow uses the same horizontal sequence lanes and updates steps in place.
It must not animate continuously. Motion is limited to short state transitions
and disabled under `prefers-reduced-motion`.

### Section navigation

Navigation uses anchors and updates active state as the reader moves through the
document. Source references are rendered as readable repository-relative paths;
the page does not attempt browser-specific `file:` deep links into source files.

## Visual direction

Use a restrained technical-document aesthetic: neutral surfaces, one blue accent
for active flow, one warm accent for lifecycle/destructive boundaries, and a
monospace stack for commands and paths. Avoid dashboard KPI cards, decorative
gradients, heavy borders, and nested cards.

The topology and flow diagrams should dominate; prose remains concise and
scannable. Color always has a text or shape counterpart. The page follows system
light/dark preference and remains readable from 320 px upward.

## Content constraints

- Describe `bk` as a thin client and daemon as the runtime boundary.
- State that current binaries connect to an existing user browser and do not
  launch Chrome.
- Distinguish default and isolated sessions.
- Distinguish Owned close from Attached detach.
- Show the lock order exactly as `session_bind_lock -> lifecycle RwLock ->
  target_registration_lock -> DashMap entry`.
- Explain schema v3, v2 backup migration, debounced atomic writes, transient
  `last_error`, and permanent fail-closed state.
- Preserve localhost, loopback, private-network, canonical `file:`, UNC file
  URL, and `about:blank` behavior while identifying rejected schemes.
- Call out remaining internal managed-browser fields and Windows token ACL
  limitations as historical or security edges, not current product features.

## Accessibility and responsiveness

- Use semantic headings, landmarks, buttons, labels, and native form controls.
- Keep visible focus indicators and native tab order.
- Every SVG has a title, description, and text fallback.
- The selected diagram node and selected runtime flow expose state through ARIA
  attributes as well as color.
- Reflow diagrams and navigation without horizontal page scrolling at narrow
  widths.
- Do not require hover for essential information.

## Validation

1. Confirm the file has no external resource or network references.
2. Parse all internal section links and verify their targets exist.
3. Check JavaScript selectors against actual element IDs and exercise every node
   and flow option in a browser smoke test.
4. Render desktop and narrow screenshots and inspect text clipping, overlap,
   contrast, focus states, and diagram legibility.
5. Compare all architecture claims and repository paths against the current
   source tree.
6. Run `git diff --check`.

## Non-goals

- No live daemon status or Chrome connection.
- No executable browserkit commands from the page.
- No generated API reference or exhaustive command flag catalog.
- No replacement for README, `bk --help`, or the canonical command reference.
- No changes to runtime code.
