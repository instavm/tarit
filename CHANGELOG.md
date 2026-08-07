# Changelog

All notable changes to Tarit are documented in this file. The `proto/`,
`vmm/`, and `orch/` workspaces are versioned together.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Until 1.0, minor
versions may contain breaking changes.

## [Unreleased]

## [0.1.2] - 2026-08-07

### Added

- `vmm snapshot --live` on the CLI and a wire-compatible `live` flag on the
  snapshot API request, exposing live snapshots of a running guest.
- Integration test that verifies pages written by device DMA during a live
  snapshot survive restore byte-for-byte.

### Changed

- Live snapshot final stop targets sub-millisecond downtime (500µs default,
  measured at 92–279µs on c8i): pause/park polling is now µs-granular, device
  I/O quiesce happens before the vCPU pause, and the reported downtime spans
  the entire guest-visible blackout including both handshakes.

### Fixed

- Live snapshots no longer capture stale device-DMA pages: the software
  host-dirty tracker (virtio used rings, blk/net/vsock payloads — invisible
  to KVM's dirty log) is merged into every pre-copy round and the final stop.
- Net and vsock I/O pump threads are parked during the live snapshot final
  stop, so no VMM thread writes guest memory while the residual is copied.

## [0.1.1] - 2026-07-27

### Added

- `vmm kernel install`: downloads a reproducibly built Linux 6.12 LTS `vmlinux`
  over HTTPS and verifies its pinned SHA-256, falling back to a checksum-pinned
  source build.

### Changed

- Fenced lifecycle and snapshot transitions, with tightened isolation and
  admission control.
- SSH gateway client authentication no longer accepts RSA public keys.
- Lifecycle and snapshot clients use a 600s request deadline instead of a 5s
  per-read timeout, so suspend and snapshot no longer fail while guest RAM is
  copied.

### Fixed

- Live snapshots now pre-copy and restore correctly: state is captured at the
  final stop, dirty bits consumed from KVM are replayed so a following diff
  snapshot stays a diff, and only the residual is copied during downtime.
- SSH gateway command mode and serial exec: command strings run through `sh -c`,
  client EOF no longer tears down the VMM stream, and exec markers no longer
  desync.
- Warm pools stayed empty because the golden overlay was deleted with its source
  VM.

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
