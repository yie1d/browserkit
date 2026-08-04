# Loopback-only daemon design

## Goal

Remove daemon request authentication completely. Browserkit is a local, single-user runtime whose daemon is reachable only through an ephemeral IPv4 loopback port. A per-daemon bearer token does not create a meaningful security boundary against processes running as the same user, while it expands the protocol, lifecycle, error model, filesystem state, tests, and documentation.

## Trust boundary

```text
bk CLI
  -> newline-delimited JSON over 127.0.0.1 only
  -> browserkit daemon/runtime
  -> cdpkit typed CDP
  -> user-selected Chrome or Edge instance
```

- The daemon binds exactly `127.0.0.1:0`; no configuration can widen the listener.
- The local operating-system user boundary is trusted. Browserkit does not attempt to defend against a malicious process already running as the same user.
- The daemon port must not be exposed or forwarded to another host.
- Browser CDP endpoint security is a separate boundary and is not changed here.
- Any future non-loopback transport is a new feature that must introduce an authentication and authorization design together with that transport. No dormant auth abstraction is retained.

## External contract

The request envelope has exactly two fields:

```json
{"cmd":"session.list","params":{}}
```

`cmd` is required and `params` defaults to JSON null. Unknown top-level fields, including `token`, are rejected. There is one daemon server constructor and it always uses the loopback listener.

## Complete removal

- Delete the token module and `~/.bk/daemon.token` lifecycle.
- Remove `Request.token`, client injection, handler context state, server validation, optional-token constructors, and token forwarding in internal requests.
- Remove `UNAUTHORIZED` from the public error model and all token-specific tests.
- Replace tests with contract tests for the two-field request envelope and the single loopback server path.
- Rewrite product documentation and the architecture tour around the actual loopback-only trust boundary.
- Remove all completed `docs/superpowers` artifacts from the final product tree; intermediate design and plan commits remain available in Git history only.

## Verification

- Targeted RED/GREEN protocol and server tests.
- `cargo test --all-targets --locked`
- `cargo clippy --all-targets --locked -- -D warnings`
- `cargo fmt --all -- --check`
- `cargo run --locked -- --help`
- `git diff --check`
- Repository-wide scan for token/auth remnants and completed planning artifacts.

No validation may connect to, launch, close, or terminate the user's Chrome.
