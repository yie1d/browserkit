# browserkit v2 Act Parity Design

Date: 2026-07-14

## Purpose

browserkit should finish migrating ordinary page interactions from legacy
workspace commands into the session-native `act` command before deleting the
old top-level action commands and `act.*` daemon routes.

This spec narrows the broader v2 migration plan to action parity only. The
desired end state is:

- Agents use one action surface: `bk act <kind> ...`.
- `act` resolves `--session` and `--target` through v2 session state.
- Legacy workspace action commands are removed only after a matching v2 kind
  exists and is tested.
- Low-level CDP and DOM interaction helpers remain in `page::interaction`;
  only daemon routing and parameter parsing move to the v2 action path.

## Current State

The v2 `act` daemon handler currently supports:

- `click`
- `type`
- `press`

It already uses the session model, returns structured errors, supports
`--session`, `--target`, `--timeout`, and `--no-state-diff`, and computes
`state_diff` unless disabled.

Legacy action functionality still exists through workspace routes:

- `act.click`
- `act.type`
- `act.scroll`
- `act.select`
- `act.hover`
- `act.focus`
- `act.fill`
- `act.upload`
- `act.dropdown_options`
- `act.drag`
- `act.keys`

The public legacy CLI commands that call these routes are hidden, but still
parse and execute:

- `bk click`
- `bk type`
- `bk scroll`
- `bk select`
- `bk hover`
- `bk focus`
- `bk fill`
- `bk upload`
- `bk drag`
- `bk keys`
- `bk options`

## Goals

- Add v2 `act` parity for `scroll`, `select`, `hover`, `focus`, `fill`,
  `upload`, `drag`, and dropdown option inspection.
- Treat `press` as the v2 replacement for legacy `keys`.
- Keep `click` and `type` behavior on the existing v2 route.
- Reuse existing interaction helpers instead of duplicating CDP details.
- Keep all v2 action failures structured through `Response::error_detail`.
- Update README and command references so the documented action path is v2-only.
- Remove each legacy CLI command and daemon route after its v2 replacement has
  a failing-then-passing test.

## Non-Goals

- Do not move browser automation concepts into cdpkit-rs.
- Do not redesign `snapshot` element refs.
- Do not preserve workspace action routes as a long-term compatibility layer.
- Do not implement new action capabilities beyond legacy parity.
- Do not remove developer/internal `browser`, `daemon`, or `debug` commands in
  this action-focused phase.
- Do not delete workspace persistence or migration code as part of this spec.

## Public CLI Shape

The external action surface remains one command:

```sh
bk act <kind> [action arguments] [--session <name>] [--target <targetId>]
```

Supported kinds after this migration:

| Legacy command | v2 command |
| --- | --- |
| `bk click --ref 42` | `bk act click --ref 42` |
| `bk click --x 100 --y 200` | `bk act click --x 100 --y 200` |
| `bk type --ref 42 "text"` | `bk act type --ref 42 --text "text"` |
| `bk keys Enter` | `bk act press --keys Enter` |
| `bk scroll down` | `bk act scroll --direction down` |
| `bk scroll --ref 42` | `bk act scroll --ref 42` |
| `bk scroll --selector "#main"` | `bk act scroll --selector "#main"` |
| `bk select --ref 42 value` | `bk act select --ref 42 --value value` |
| `bk hover --ref 42` | `bk act hover --ref 42` |
| `bk focus --ref 42` | `bk act focus --ref 42` |
| `bk fill --set ref:42=value` | `bk act fill --set ref:42=value` |
| `bk upload --ref 42 file.pdf` | `bk act upload --ref 42 file.pdf` |
| `bk drag --from-ref 10 --to-ref 20` | `bk act drag --from-ref 10 --to-ref 20` |
| `bk options --ref 42` | `bk act options --ref 42` |

The v2 CLI should prefer named flags over positional values for action-specific
data where that keeps parsing unambiguous. `type` keeps `--text`, `select` uses
`--value`, and `scroll` uses `--direction`. File paths for `upload` may remain
positional after the target flags because they are naturally variadic.

Index-based targeting should not be expanded in v2. Existing v2 `act` uses
stable snapshot refs and coordinates. Legacy commands that accepted `--index`
should be removed rather than carried into the new public API.

## Daemon Request Shape

The CLI sends all migrated actions to `cmd = "act"` or `cmd = "v2.act"`.

Common fields:

```json
{
  "kind": "scroll",
  "session": "default",
  "target": "TARGET_ID",
  "timeout": 30000,
  "no_state_diff": false
}
```

Action-specific fields:

| Kind | Fields |
| --- | --- |
| `click` | `ref` or `x` + `y` |
| `type` | `ref`, `text`, `append` |
| `press` | `keys[]` |
| `scroll` | `direction`, `amount`, `ref`, or `selector` |
| `select` | `ref`, `value` |
| `hover` | `ref` |
| `focus` | `ref` |
| `fill` | `fields[]`, each with `ref` and `value` |
| `upload` | `ref` or `selector`, plus `files[]` |
| `drag` | `from_ref` or `from_selector`, plus `to_ref` or `to_selector` |
| `options` | `ref` |

`index`, `wid`, and legacy workspace target aliases must not be accepted by the
v2 handler.

## Handler Design

The v2 `act` handler should keep its current structure:

1. Parse and validate JSON parameters.
2. Resolve session and target.
3. Get the tab CDP session id.
4. Capture optional before state.
5. Execute the action.
6. Capture optional after state and compute `state_diff`.
7. Return a standardized JSON response.

The implementation should expand `ActKind` and `ActParams`, then add small
`execute_<kind>` functions. Those functions should call existing helpers in
`page::interaction` whenever possible:

- `scroll_page`, `scroll_to_element_by_target`, `scroll_to_element_by_selector`
- `select_by_target`
- `hover_by_target`
- `focus_by_target`
- `fill_fields_by_target`
- `upload_files_by_target`, `upload_files_by_selector`
- `drag_by_target`
- `dropdown_options_by_target`
- `dispatch_key_combo` through the existing press path

If a helper currently only accepts `ElementTarget::Index`, v2 should not expose
that branch. New helper changes should be limited to shared interaction
functions that are independent of workspace state.

## Response Design

All v2 action responses should include:

```json
{
  "ok": true,
  "data": {
    "action": "select",
    "result": "completed",
    "target": "TARGET_ID",
    "state_diff": null
  }
}
```

Action-specific data may be added under `data`, for example:

- `ref`
- `value`
- `files`
- `options`
- `results` for batch `fill`
- `new_tab` for click-triggered tab creation once session-native new-tab
  detection exists

Legacy response fields such as `wid`, `tid`, and `status` should not be added
to new v2 responses.

## Deletion Order

Migration should proceed in small TDD batches.

1. Extend v2 `act` parsing and CLI dispatch for one or two action kinds at a
   time.
2. Verify the new kind rejects missing or incompatible parameters with
   `INVALID_ARGUMENT`.
3. Verify the new kind routes through session errors when no session exists.
4. Update README and `docs/bk-browser/references/commands.md`.
5. Remove the matching legacy top-level CLI commands.
6. Remove the matching legacy daemon routes only after no remaining CLI path
   calls them.
7. Update `docs/ROADMAP.md`.

Suggested batches:

- Batch A: `scroll`, `hover`, `focus`
- Batch B: `select`, `options`
- Batch C: `fill`
- Batch D: `upload`, `drag`
- Batch E: remove legacy `keys` after documenting `press` as the replacement
- Batch F: remove legacy `click` and `type` after confirming existing v2 parity

`act.click` new-tab detection currently depends on workspace tab tracking in
the legacy handler. Do not delete useful behavior silently. Either implement
session-native new-tab detection before deleting legacy click, or explicitly
record that v2 click returns no `new_tab` until a later follow-up.

## Testing Strategy

Every production behavior change should follow red-green TDD.

Focused tests:

- CLI parsing tests for each new `bk act <kind>` shape.
- CLI rejection tests for removed top-level legacy commands.
- `parse_act_params` tests for each new kind and validation failure.
- Handler tests confirming missing session returns `SESSION_NOT_FOUND`.
- Dispatch tests confirming removed legacy daemon routes return `unknown command`.

Runtime tests with Chrome available should cover at least:

```sh
bk connect
bk open https://example.com
bk snapshot
bk act scroll --direction down
bk act hover --ref <ref>
bk act focus --ref <ref>
bk act select --ref <ref> --value <value>
bk act fill --set ref:<ref>=value
bk act upload --ref <ref> <absolute-file-path>
bk act press --keys Enter
```

Required repository verification:

```sh
cargo test --quiet
cargo clippy --all -- -D warnings
git diff --check
```

## Risks

- The old action route uses workspace state for touch tracking, dialog policy,
  and some new-tab detection. Session-native replacements must not accidentally
  depend on workspace ids.
- Carrying `--index` into v2 would preserve an unstable addressing model and
  weaken the observe/act contract.
- Moving too many actions in one diff can hide behavior regressions.
- `fill`, `upload`, and `drag` have more complicated parameter shapes and
  should not be batched with simple actions.

## Acceptance Criteria

- README no longer says Phase 2 actions require legacy commands.
- `docs/bk-browser/references/commands.md` lists v2 `act` replacements for
  removed legacy actions.
- `bk act` supports the action kinds listed in this spec using session targets.
- Removed legacy CLI commands fail to parse.
- Removed legacy daemon routes return `unknown command`.
- `cargo test --quiet`, `cargo clippy --all -- -D warnings`, and
  `git diff --check` pass before each commit.
