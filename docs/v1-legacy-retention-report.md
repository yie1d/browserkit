# browserkit v1 legacy retention report

> Date: 2026-07-13
>
> Status: historical/resolved. This report records why the old v1 workspace
> surface temporarily existed. It is not current guidance. The session-only
> migration has removed the ordinary agent-visible legacy surface.

## Resolved Decision

browserkit's current product shape is a persistent browser runtime above
cdpkit-rs. `bk` and the daemon are entrypoints into that runtime; cdpkit-rs
remains the typed CDP protocol layer.

The previous v1 retention recommendation was resolved by a breaking migration:

- workspace CLI commands and aliases were removed;
- `BK_WS` and `--ws` were removed;
- workspace fields are not written to schema v3 state;
- daemon routes `ws.*`, `tab.*`, `nav.*`, `page.*`, old `storage.*`, and `v2.*`
  aliases were removed;
- non-functional streaming debug commands `debug monitor`, `debug har`, and
  `debug events` were removed.

Current users should use the session runtime:

```text
connect
open
attach
snapshot
find
search
act
navigate
wait
evaluate
html
console
pdf
screenshot
tabs
close
session
status
dialog
```

Administration remains explicit:

```text
browser discover|connect|list|disconnect
daemon start|status|stop
```

Developer escape hatches remain explicit:

```text
debug block|unblock|cdp
```

## Historical Context

The old v1 surface existed for transition, fallback, and diagnostics:

- old scripts could call `goto`, `info`, `click`, `type`, `tab`, or workspace
  commands;
- some capabilities had not yet moved to the session model;
- browser and daemon commands were useful for development and operations;
- old persisted state still contained workspace fields.

Those reasons no longer justify keeping ordinary agent-visible v1 APIs. Useful
capabilities were rebuilt as session-native commands or kept as admin/developer
commands with explicit names.

## Migration Notes

State migration is one-way:

1. When schema v2 `~/.bk/state.json` is detected, browserkit first creates
   `state.v2.backup.json` or a numbered variant.
2. Existing v2 sessions are preserved.
3. Restorable workspace records are converted into sessions or merged into the
   default session when safe.
4. Duplicate targets, conflicting hosts, and non-restorable records are dropped
   with structured warnings.
5. Schema v3 is written only after conversion succeeds.

`bk status` reports migration metadata, including preserved sessions, migrated
records, merged attached tabs, dropped targets/hosts, warnings, and backup path.
Cleanup operations also report `cleanup_errors` when some session targets could
not be closed or detached.

## Historical Lessons

- Keep browserkit docs centered on runtime/session/attach existing Chrome, not
  a collection of browser automation CLI aliases.
- Keep cdpkit-rs protocol-only; runtime policy belongs in browserkit.
- Separate removed interfaces from retained capabilities. `find`, `search`,
  `html`, `console`, `pdf`, `dialog`, storage, request blocking, and raw CDP
  remain useful only through their current session/admin/developer surfaces.
- Do not reintroduce compatibility forwarding for removed workspace routes; it
  would recreate the ambiguity this migration removed.
