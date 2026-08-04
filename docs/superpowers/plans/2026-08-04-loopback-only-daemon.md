# Loopback-only daemon implementation plan

1. Add a failing protocol contract test proving a request with a `token` field is rejected. Run only that test and capture the RED result.
2. Delete `Request.token`; update all request construction sites and client serialization. Keep `deny_unknown_fields` so the RED test turns GREEN.
3. Collapse `DaemonServer` to one loopback-only constructor. Remove token state and validation from `HandlerContext`, connection handling, daemon startup, shutdown, and internal request forwarding.
4. Delete `src/daemon/token.rs`, remove `ErrorCode::Unauthorized`, and update exhaustive error-contract tests.
5. Replace token-oriented server and lifecycle tests with tests for plain requests, loopback binding, and shutdown signaling through the canonical request envelope.
6. Update README, AGENTS, changelog, architecture/roadmap/design docs, browser connection guide, skill docs, command reference, and architecture HTML to state the loopback-only local trust model.
7. Remove every completed file below `docs/superpowers/`, including this design and plan, so the final tree contains no migration or completed-plan artifacts.
8. Run formatting, all tests, clippy with warnings denied, CLI help smoke test, diff validation, and exhaustive repository scans. Review the complete diff for protocol, lifecycle, concurrency, persistence, CLI, and security consistency.
