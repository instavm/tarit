# Changelog

All notable changes to Tarit are documented in this file. The `proto/`,
`vmm/`, and `orch/` workspaces are versioned together.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Until 1.0, minor
versions may contain breaking changes.

## [Unreleased]

### Added

- Generated Python and TypeScript clients now cover the public OpenAPI contract.
  Their handwritten layers provide API-key setup, typed failures,
  deadline-bounded execution polling, and stable-child-id live-fork retries.
- The continuous mixed-OCI supervisor now publishes an atomically replaced,
  root-only status record and runs bounded orchestrator/VMM crash recovery,
  multi-ingress wake qualification, and persistent-volume hibernation between
  long-lived lifecycle epochs.
- Cross-node fork operations now persist the target daemon boot session and
  reuse an exact child-bound private artifact across interrupted retries.
- Version-2 snapshot state now records the state ABI, device-model ABI,
  architecture, CPU-template identity, and writer version for pre-KVM
  compatibility checks; version 1 remains the explicit legacy format.

### Changed

- Live snapshots now pause, capture, and resume every configured vCPU through
  one bounded barrier; live fork supports SMP guests instead of rejecting them.
- Hibernated exec, PTY, SSH, and public-share requests now join the same
  registered activation instead of competing for or replacing its boot
  reservation.
- Guest IPv4 restore repair is applied and verified through the Linux network
  API, removing the `iproute2` dependency from minimal OCI guests.
- Continuous lifecycle qualification now carries absolute and monotonic timers
  through two live sibling forks and verifies independent delivery and state.
- Continuous lifecycle qualification now guarantees live fork,
  snapshot/restore, and concurrent guest-work coverage before randomized
  transitions and reports per-action p50, p95, p99, and maximum latency.
- Runtime recovery qualification now stages the exact candidate guest agent in
  its private OCI-derived rootfs before testing hibernation and restore.
- Persistent-volume hibernation qualification now stages the exact candidate
  guest agent in its private OCI-derived rootfs before testing restore.
- Balloon release qualification now injects the exact candidate guest agent
  into each OCI fixture and creates cgroup pressure without blocking control on
  a throttled VMM.
- Clone-generation qualification now injects the exact candidate guest agent
  into a private OCI rootfs copy, supports minimal images without Python, and
  places large scratch artifacts on a configurable test volume.
- Differential restore qualification can use separate source and restore VMM
  binaries to verify rolling snapshot-format upgrades.

### Fixed

- Internal live-fork and hibernation snapshots now carry an explicit VM lease.
  Successful resume and terminal child deletion retire the exact RAM, integrity,
  and CoW files; user-created snapshots remain durable. Continuous lifecycle
  qualification also rejects stale ephemeral rows and untracked snapshot files.

- Production generic-NFS volume configuration now requires `krb5p`; mount
  admission verifies the live NFS version, TCP transport, hard-mount policy,
  and security flavor instead of accepting an `AUTH_SYS` downgrade.
- Failed all-vCPU live snapshot transitions resume the source and discard
  staging state; a live request against an already-paused VM fails without
  changing its state.
- Live snapshot capture and restore now reject malformed or incomplete runtime
  state instead of publishing a paused, partially restored VM.
- Restored x86 vCPUs apply LAPIC state before MSRs so TSC-deadline timers resume
  correctly on secondary CPUs.
- Duplicate activation can no longer replace an in-flight boot registration;
  waiters observe the result of the exact registered incarnation.
- Snapshot restore now repairs guest realtime inside the mandatory
  pre-admission barrier while leaving monotonic timers independent of the host
  hibernation interval.
- Continuous lifecycle shutdown now waits for the active epoch to remove its
  process tree, mounts, and runtime storage before the service exits.
- A restarted cross-node fork target can reclaim its own durable quota
  reservation immediately. Same-session duplicate requests, wrong-target
  takeover, and rebinding a private snapshot to another child still fail
  closed; replaying the same child ownership bind is idempotent.
- Pathname vsock listeners now retain the exact socket identity, refuse to
  replace a pre-existing filesystem entry, and unlink the owned socket during
  VMM teardown. Empty private runtime directories are removed after VM-owned
  artifacts are retired.
- Incremental snapshot qualification now proves full-snapshot fallback for the
  first diff request, restores a real parent/diff RAM chain, and verifies that
  a hardlink-spelled input is neither mutated nor reused as snapshot output.
- Snapshot restore rejects a checksum-valid incompatible CPU template or
  removed-manifest downgrade before publishing a VM. The compatibility gate
  can build a deliberately newer state ABI and prove the same fail-closed
  behavior across actual VMM binaries. Differential restore qualification can
  also force guest-sized VMM mappings into host swap and verify pre-parent and
  post-parent RAM contents.

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
