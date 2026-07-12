# Task 7 approval evidence

2026-07-12

- `cargo test -p taritd share_runtime -- --nocapture` — PASS: 4 lifecycle tests, including `share_runtime_reaps_short_lived_bridges_before_shutdown`.
- `cargo test -p taritd share_gateway::tests::bridges_websocket_frames_and_negotiates_the_upstream_protocol -- --exact` — PASS.
- `cargo test -p taritd share_gateway::tests::remote_owner_bridges_text_binary_ping_pong_and_graceful_close -- --exact` — PASS.
- `cargo fmt --all -- --check` — PASS.
- `cargo clippy --workspace --all-targets -- -D warnings` — PASS.
- `cargo test --workspace` — PASS: 171 unit tests; all documented doctest suites passed.
