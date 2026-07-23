## Summary

<!-- What does this change do, and why? -->

## Checklist

- [ ] For each workspace you touched (`vmm/`, `orch/`, `proto/`): `cargo fmt --all
      -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and
      `cargo test --workspace` pass locally.
- [ ] For changes to boot, devices, memory, snapshot/restore, net, or the
      jailer, review the protected-main KVM workflow after merge (or manually
      dispatch it from `main`). Privileged self-hosted runners never execute
      pull-request refs.
- [ ] Startup-path changes report cold, snapshot restore, suspend/resume, and
      warm-pool latency through the first successful guest exec, with explicit
      median/p95/p99 and success-rate gates.
- [ ] Wire-protocol changes were made in `proto/` only (not copied into `vmm/` or
      `orch/`), if this changes requests, responses, config, VM status, or PTY
      frames.
- [ ] No breaking change to the stable control contract (`vmm serve --socket`,
      `ApiRequest`/`ApiResponse`, length-prefixed JSON). If there is, it is called
      out above and versioned.
- [ ] Every `unsafe` block has a `// SAFETY:` comment.
- [ ] Docs updated if behavior or architecture changed.
