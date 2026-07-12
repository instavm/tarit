# Changelog

All notable changes to Tarit are documented in this file. The `proto/`,
`vmm/`, and `orch/` workspaces are versioned together.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Until 1.0, minor
versions may contain breaking changes.

## [Unreleased]

### Changed

- SSH gateway client authentication no longer accepts RSA public keys.

## [0.1.0] - 2026-07-03

Initial public release of Tarit, a microVM platform for secure, fast,
ephemeral sandboxes, licensed under AGPL-3.0-or-later.

### Added

- `vmm/` 0.1.0: the Tarit VMM, a minimal rust-vmm-based microVM monitor for
  x86_64 Linux with KVM. One process per microVM, MMIO virtio device model
  (block, net, vsock, serial), snapshot/restore with diff snapshots, live
  snapshots, suspend/resume, seccomp and jailer sandboxing, nftables-based
  egress filtering, and vsock exec/PTY into the guest.
- `orch/` 0.1.0: `taritd`, a multi-node orchestrator and control plane with
  an HTTP API, placement, warm pools, networking, snapshots, an SSH/PTY
  gateway, per-key usage stats, and an audit trail backed by PostgreSQL.
- `proto/` 0.1.0: `tarit-proto`, the shared dependency-light crate holding
  the Unix-domain-socket wire protocol between the VMM and any orchestrator.
- Guest tooling: `make guest` builds a guest kernel and pulls an Ubuntu
  rootfs; a guest agent handles exec and PTY inside the VM.
- Project docs (README, per-workspace docs, benchmarks), CI covering fmt,
  clippy, check, tests, and KVM type-checks across all three workspaces, and
  security policy files.
