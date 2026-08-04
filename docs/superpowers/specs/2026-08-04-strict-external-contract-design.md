# Strict External Contract Design

## Goal

Ensure every public CLI option and canonical daemon request field either changes behavior in its declared scope or is rejected explicitly. Invalid names, types, enum values, and option-command combinations must never be silently ignored or normalized.

## Considered approaches

1. **Central request schema plus CLI scope preflight (selected).** Keep the ergonomic global CLI options, reject unsupported combinations before daemon startup, and extend the canonical request contract with JSON type metadata. This keeps one source of truth for transport validation and leaves business constraints in handlers.
2. **Handler-only validation.** Add ad-hoc checks to every handler. This avoids a central schema but repeats type logic and makes future drift between the allowlist and handlers likely.
3. **Move global options into every applicable subcommand.** Clap would reject unsupported combinations automatically, but Session/target/timeout definitions would be duplicated across most commands and existing invocation order would change.

## CLI contract

- `--session` is accepted only by commands that bind, select, or operate on a Session.
- `--target` is accepted only by commands that select or operate on one target. `dialog list` and `dialog policy` are Session-wide and must not send it.
- `--timeout` is accepted only by `snapshot`, `act`, `navigate`, `evaluate`, `wait`, `network watch`, and `download`.
- `--no-state-diff` is accepted only by `act`.
- Unsupported combinations fail locally before client creation or daemon auto-start and name both the option and command.

## Daemon request contract

`allowed_request_fields` becomes a command-to-field schema containing the expected JSON shape. Validation runs before Session activity and dispatch:

- string, boolean, unsigned integer, signed integer, object, array, and string-array shapes are distinguished;
- optional fields may be omitted but may not be `null` or carry another type;
- `act` remains under its existing action-specific parser after the params-object check;
- unknown commands continue to reach the existing unknown-command response;
- handlers retain required-field, range, mutual-exclusion, and semantic validation.

Known enum-like fields that currently default silently, especially `snapshot.wait`, reject unknown values with `INVALID_ARGUMENT`.

## Testing

- CLI tests reproduce ignored globals and the `dialog list/policy` target mismatch before implementation.
- Table-driven protocol tests provide valid values for every canonical field and reject representative wrong types.
- Targeted tests prove invalid `snapshot.wait` no longer falls back.
- Full `cargo test`, Clippy, rustfmt, and diff checks remain required. No Chrome process is started or connected.

## Scope

No migration, compatibility alias, deprecated field, dependency, or response-format change is introduced. The design and implementation remain uncommitted until explicitly requested.
