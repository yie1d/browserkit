# Browserkit Architecture Tour Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a self-contained offline HTML tour that explains browserkit's real runtime architecture through navigable sections, a selectable topology, and four switchable execution flows.

**Architecture:** Create one `docs/architecture-tour.html` with semantic HTML, inline responsive CSS, inline SVG, and a small dependency-free JavaScript controller. Static content remains useful with JavaScript disabled; JavaScript only manages selected topology details, flow switching, compact navigation, and active-section state.

**Tech Stack:** HTML5, CSS media queries and system color preference, inline SVG, browser-native JavaScript, PowerShell static validation, local browser visual inspection.

## Global Constraints

- Do not modify Rust runtime code.
- Do not load external fonts, scripts, styles, images, APIs, or CDN resources.
- Do not connect to or launch Chrome through browserkit.
- Keep all architecture claims aligned with current source and maintained docs.
- Support direct `file:` opening, keyboard operation, light/dark preference, and widths from 320 px upward.
- Keep source references repository-relative and do not invent clickable editor-specific URLs.

---

### Task 1: Build the semantic document and architecture content

**Files:**
- Create: `docs/architecture-tour.html`

**Interfaces:**
- Produces: section IDs `overview`, `topology`, `startup`, `runtime-model`, `routing`, `concurrency`, `persistence`, `security`, `source-map`, and `assessment`.
- Produces: topology controls with `data-node` values and matching detail records in `ARCHITECTURE_NODES`.
- Produces: flow selector `#flow-select`, sequence container `#flow-steps`, and matching records in `RUNTIME_FLOWS`.

- [ ] **Step 1: Write the static contract check and verify RED**

Run this PowerShell check before creating the page:

```powershell
$path = 'docs/architecture-tour.html'
if (-not (Test-Path -LiteralPath $path)) { throw 'architecture tour missing' }
```

Expected: fail with `architecture tour missing`.

- [ ] **Step 2: Create the standalone document shell**

Create a complete HTML5 document with `<meta charset="utf-8">`, responsive viewport, title `browserkit architecture tour`, a skip link, `<header>`, `<nav aria-label="Architecture sections">`, `<main>`, and one semantic `<section>` for each required ID. Add a `<noscript>` note stating that all written content remains available but diagram selection is disabled.

- [ ] **Step 3: Add repository-backed architecture content**

Include concise explanations and repository-relative paths for:

```text
src/main.rs                         CLI parsing and local output
src/client.rs                       daemon discovery, auto-start, token injection
src/daemon/protocol.rs              newline-JSON request/response boundary
src/daemon/server.rs                loopback listener and idle cleanup
src/daemon/state.rs                 shared runtime state and locks
src/daemon/handler/mod.rs           canonical command dispatch
src/daemon/session.rs               default/isolated sessions and ownership
src/daemon/target_lifecycle.rs      watcher-driven target registration
src/daemon/persist.rs               schema v3 and v2 migration
src/browser/                        CDP discovery and connection
src/page/                           page capabilities
```

State explicitly that current binaries connect to an existing browser, unmanaged user browsers are not auto-reconnected after daemon restart, and historical managed-browser fields remain internal compatibility edges.

- [ ] **Step 4: Add the topology and sequence markup**

Use inline SVG for the topology with labeled controls for `Agent`, `bk CLI`, `daemon`, `handlers`, `page`, `cdpkit`, `Chrome`, `DaemonState`, `watchers`, and `state.json`. Provide `<title>`, `<desc>`, arrow labels, and a text fallback list. Add a native `<select id="flow-select">` containing values `auto-start`, `open-tab`, `new-tab`, and `disconnect`, plus an ordered list with `id="flow-steps"`.

- [ ] **Step 5: Re-run the static existence check**

Expected: pass with exit code 0.

---

### Task 2: Add restrained responsive styling and local interactions

**Files:**
- Modify: `docs/architecture-tour.html`

**Interfaces:**
- Consumes: Task 1 section IDs, `data-node` controls, `#flow-select`, and `#flow-steps`.
- Produces: `selectNode(nodeId)`, `renderFlow(flowId)`, `setActiveSection(sectionId)`, and a compact-navigation change handler.

- [ ] **Step 1: Add responsive styling**

Use system font stacks, CSS custom properties with `prefers-color-scheme`, a neutral document background, one blue active color, one amber destructive-lifecycle color, and monospace path styling. Use a two-column navigation/content layout above 960 px and a native compact section selector below 960 px. At 640 px, stack topology nodes and sequence lanes vertically without page-level horizontal overflow. Preserve native focus outlines.

- [ ] **Step 2: Define architecture detail data**

Define `ARCHITECTURE_NODES` as a frozen object whose values contain exact `title`, `role`, `inputs`, `outputs`, and `files` fields. `selectNode(nodeId)` must validate the ID, update `aria-pressed` on every node control, and replace the single detail region's text using `textContent`, not HTML injection.

- [ ] **Step 3: Define runtime flow data**

Define `RUNTIME_FLOWS` with these exact titles and lane sequences:

```text
auto-start: Agent -> bk CLI -> daemon process -> TCP server -> handler
open-tab: Agent -> bk CLI -> open handler -> Chrome Target -> lifecycle registry
new-tab: Agent -> act handler -> Chrome -> target watcher -> Session
disconnect: Chrome -> disconnect monitor -> DaemonState -> Session cleanup -> persisted status
```

Each step record contains `lane`, `label`, `detail`, and `kind`. `renderFlow(flowId)` validates the value, rebuilds the ordered list with DOM methods, and updates a concise accessible summary.

- [ ] **Step 4: Add section navigation behavior**

Use anchor navigation as the baseline. Add an `IntersectionObserver` only when available to update the active desktop link and compact selector. The compact selector calls `document.getElementById(value).scrollIntoView({behavior: reducedMotion ? 'auto' : 'smooth'})`. The page must remain fully navigable without this script.

- [ ] **Step 5: Add initialization and reduced-motion handling**

On `DOMContentLoaded`, select `daemon`, render `auto-start`, synchronize the compact selector, and attach click/change handlers. Read `window.matchMedia('(prefers-reduced-motion: reduce)')` before selecting scroll behavior. No timer or looping animation is permitted.

---

### Task 3: Validate behavior, accessibility, and visual layout

**Files:**
- Modify if validation fails: `docs/architecture-tour.html`

**Interfaces:**
- Consumes: completed standalone page.
- Produces: validated offline artifact with no runtime dependencies.

- [ ] **Step 1: Run static safety checks**

Run PowerShell checks that assert all ten section IDs exist; every `href="#..."` target resolves; `ARCHITECTURE_NODES`, `RUNTIME_FLOWS`, `selectNode`, and `renderFlow` exist; and the document contains none of `http://`, `https://`, `fetch(`, `XMLHttpRequest`, `WebSocket`, `<script src=`, or `<link rel="stylesheet"`.

Expected: all checks pass with exit code 0.

- [ ] **Step 2: Run source-reference checks**

Extract every visible `src/...` path used as a source reference, trim optional line annotations, and assert that each path exists in the repository. Manually compare the lock order, ownership semantics, URL policy, persistence semantics, and daemon restart behavior against current source.

- [ ] **Step 3: Run browser interaction smoke tests**

Open the page directly from disk. Activate every topology node and confirm the detail title and file list change. Select every runtime flow and confirm its ordered steps and accessible summary change. Use keyboard-only navigation for node selection, flow switching, section anchors, and compact navigation.

- [ ] **Step 4: Run visual QA at desktop and narrow widths**

Inspect at approximately 1440x1000, 736x1000, and 390x844. Confirm no clipped labels, overlapping nodes, hidden focus state, horizontal page scroll, unreadable contrast, or content that requires hover. Verify both light and dark system themes if the browser tooling supports theme emulation.

- [ ] **Step 5: Run repository hygiene checks and commit**

Run:

```powershell
git diff --check
git status --short
```

Stage only `docs/architecture-tour.html` and this plan if it is not already committed, then commit with:

```text
docs: add interactive architecture tour
```
