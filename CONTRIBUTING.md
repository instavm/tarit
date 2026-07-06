# Contributing to Tarit

Thanks for your interest. This document covers the repository layout, how to
build and test, and the conventions we follow.

## Layout

```
vmm/     the Tarit VMM (microVM monitor) - its own cargo workspace
orch/    taritd, the orchestrator and control plane - its own cargo workspace
proto/   tarit-proto, the shared UDS wire protocol crate (KVM-free)
```

`vmm/` and `orch/` are independent cargo workspaces, so the VMM builds and ships
on its own. Both depend on `proto/` (a path dependency) for the wire types.

## The wire protocol lives in one place

All types that cross the VMM Unix socket (requests, responses, VM config, VM
status, PTY frames) are defined once in `proto/` (`tarit-proto`). `vmm-core` and
`vmm-api` re-export them; `taritd` consumes them. Do not add a second copy of a
wire type. If you change the protocol, change it in `proto/` and both sides move
together.

## Toolchain

- Rust stable.
- Host for running microVMs: x86_64 Linux with KVM. macOS can build and
  cross-check but cannot run guests.

## Build and test

Each workspace is built and tested independently.

```sh
# shared protocol crate
cd proto && cargo test

# the VMM (unit + non-KVM tests work on macOS; KVM tests need Linux+KVM)
cd vmm && cargo test
cargo build --release -p vmm --features boot   # Linux (boot = vmm-core/kvm + vmm-api/boot)

# the orchestrator
cd orch && cargo test
cargo build --release -p taritd
```

Before sending a change, run in the workspace you touched:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

KVM-dependent end-to-end tests live in `orch/tests/` and `vmm/ci/`; they run on a
Linux+KVM host. See `orch/docs/RESILIENCE.md` for the tested scenarios.

## Conventions

- One VMM process per microVM. MMIO-only transport, no
  PCI. Every device implements a `Persist` trait for snapshot/restore.
- KVM is behind a trait so non-KVM logic is unit-testable on macOS.
- Keep commits scoped and describe the why. Commit per logical change.
- Plain, precise docs and comments. No marketing filler.

## License and contributions

Tarit is licensed under **AGPL-3.0-or-later** (see `LICENSE`). The AGPL network
clause means anyone who runs a modified Tarit as a network service must offer
their modified source to that service's users.

By submitting a contribution, you agree that it is licensed under
AGPL-3.0-or-later. So that the project can also offer a commercial license
alongside the AGPL (dual licensing) without having to track down every
contributor later, you additionally grant The Tarit Authors the right to
relicense your contribution under other terms. For substantial contributions we
may ask you to sign a Contributor License Agreement (CLA) that records this.
