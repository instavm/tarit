# Changelog

All notable changes to Tarit are documented in this file. The `proto/`,
`vmm/`, and `orch/` workspaces are versioned together.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Until 1.0, minor
versions may contain breaking changes.

## [Unreleased]

### Added

- `tarit-bench sustained`: creates sandboxes at a fixed rate for a fixed
  time (`--rate`, `--duration-secs`, `--hold-ms`, `--max-in-flight`) and
  reports the rate it reached and what it had to shed.

- `sdk/computesdk`: a `@computesdk/tarit` provider for ComputeSDK.
- `test/soak/api_soak.py`: keeps N sandboxes busy through the public API
  (commands, synced disk writes, checkpoint and restore, fork, pause,
  recycle), checks the data each holds, and fails on errors or leftovers.
- `scripts/setup-kvm-runner.sh` provisions the self-hosted runner the KVM
  workflow needs; `scripts/run-readiness-gates.sh` runs that workflow's
  gates, the e2e suite and a bounded soak on a host and keeps the logs.
- Incremental checkpoints through taritd: snapshots accept `diff=true`, and
  every checkpoint after the first (or after a restore or wake) pauses the
  guest only for pages changed since the previous one, while still producing
  a standalone snapshot. The VMM exposes the same as `vmm snapshot --base`.
- Every snapshot can capture the guest's writable disks with its memory, so a
  checkpoint restores on its own; snapshots of VMs on storage without reflink
  use a paused, extent-copied capture.
- Snapshot list and delete endpoints.
- Images carry an optional prewarm command (`taritd image build --prewarm`,
  `taritd image prewarm`) that the golden VM of the image's warm pool runs
  before its snapshot; changing it on a running taritd rebuilds that
  golden. Stored in a new `prewarm` column of the images table.
- The warm pool learns: each registered image (the newest four at startup,
  then any registered later) gets a golden snapshot and a pool of its own,
  and so does any other shape creates ask for after its first cold boot.
  Classes unused for 30 minutes are retired and their snapshots deleted.
  Tuned with `TARIT_WARM_POOL_LEARN_CLASSES`, `_LEARN_TARGET` and
  `_LEARN_IDLE_SECS`; `GET /v1/warm-pool` and the depth metric include them.
- Warm-pool classes take a `prewarm` command (`TARIT_WARM_POOL_PREWARM`) that
  the golden VM runs before its snapshot, so clones start with an image's
  runtime in their page cache.
- A create that finds the warm pool empty restores a clone of its class's
  golden snapshot on demand instead of cold-booting, so a burst larger than
  the pool is served at restore speed.
- Docker and Podman images run as VMs without root (`vmm pull
  docker-daemon:`/`podman:`), and images build without skopeo.
- `test/compare/`: a head-to-head harness against Firecracker covering cold
  boot, exec, snapshots, restore chains, fan-out, bursts, reliability, and an
  agent-workload suite (coding session, resume, fork, burst, disk, bulk
  network, soak) with correctness checks.
- `VmmController::net_counters` reports per-device packet counts and queue
  kicks the vCPU had to handle.
- Generated Python and TypeScript clients now cover the public OpenAPI contract.
  Their handwritten layers provide API-key setup, typed failures,
  deadline-bounded execution polling, stable-child-id live-fork retries, and
  bounded PTY/WebSocket sessions with terminal resize, typed protocol failures,
  and server-side session cleanup.
- SDK releases now share the server version, build and install both registry
  artifacts before release, retain checksums and attestations, and publish
  through registry-trusted short-lived identities after a protected tag.
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

- A restore or fork child seeds and verifies its private disk upper while
  the network is provisioned and the VMM process starts, instead of after.

- Finished command records, with their captured output, are deleted after
  `TARIT_EXECUTION_RETENTION_HOURS` (default 24; 0 keeps them). Every
  `/v1/execute` stored one, so a busy host's database grew without bound.

- Live snapshots of lazily restored VMs (every checkpoint after a restore,
  and every fork) do much less I/O. Untouched memory the restore snapshot
  records as all zeros is neither read nor hashed; on a reflink filesystem
  the rest of the untouched memory shares the restore snapshot's blocks,
  after each chunk is checked against its manifest, instead of being
  written and synced; and publication hashes only the staged image's data
  extents. For a 256 MiB node:22 sandbox the live snapshot went from about
  500 to 250 ms and the bytes written from 84 to 22 MiB.

- virtio-blk reads go straight into guest memory with one `preadv` per
  request (per run of sectors, under a copy-on-write overlay) instead of
  through a freshly zeroed bounce buffer: large reads from the host page
  cache went from about 1.3 to 2-3 GB/s on the reference host. The block
  worker's seccomp profile allows `preadv`.
- An authenticated lazy restore serves a memory chunk its integrity
  manifest records as all zeros with `UFFDIO_ZEROPAGE`, without reading or
  hashing it; untouched guest memory is most of a freshly booted snapshot.

- taritd keeps a warm pool by default (eight 1 vCPU / 256 MiB clones of a
  golden snapshot); `TARIT_WARM_POOL=0` turns it off. A golden that cannot
  be built is retried with backoff (1 s doubling to a minute) instead of on
  every refill round.
- `TARIT_MAX_VMS` defaults to the host's vCPU ceiling (at least 32) instead
  of a fixed 32, and taritd raises its open-file limit to the hard limit.
- A create that finds the host full stops an idle warm VM of a class it
  cannot use and takes its capacity; warm refills wait while any create is
  waiting for capacity.

- VM boots, publications and warm-pool claims on a host no longer run one at
  a time: they hold the supervisor's lifecycle gate shared, and only
  shutdown, stops, exit reconciliation and re-adoption hold it exclusively.
  Concurrent claims each take a different warm VM.
- virtio-net offers checksum and segmentation offloads, mergeable receive
  buffers, EVENT_IDX, and 1024-entry queues, receives straight into guest
  buffers, and waits for receive buffers instead of dropping frames. Both
  queue kicks reach the network thread through KVM. Bulk download in a 1 vCPU
  guest is on par with Firecracker.
- Restores map the RAM image instead of copying it; restore and teardown no
  longer wait in polling loops.
- Full and live snapshots are sparse: zero pages are not written. Live
  snapshots of lazily restored guests read untouched pages from the restore
  snapshot (verified against its integrity manifest) instead of faulting all
  of RAM in. Through taritd, a 1 GiB guest using 200 MiB checkpoints in about
  2 s into 250 MiB, down from 5-8 s and 1 GiB.
- The copy-on-write disk issues one syscall per run of sectors instead of
  one per 512-byte sector.
- virtio-blk supports EVENT_IDX and advertises read-only devices.
- x2APIC is offered to guests on bare-metal hosts.
- taritd boots guests with a quiet console and derives its default capacity
  from the host.
- The SSH gateway rejects a wrong key without the one-second delay, so an
  agent's other keys no longer cost each login a second apiece. Every offered
  key still gets the same answer; registration is checked only after the
  client signs, so offers cannot reveal which keys are registered.
- Live snapshots now pause, capture, and resume every configured vCPU through
  one bounded barrier; live fork supports SMP guests instead of rejecting them.
- Hibernated exec, PTY, SSH, and public-share requests now join the same
  registered activation instead of competing for or replacing its boot
  reservation.
- Guest IPv4 restore repair is applied and verified through the Linux network
  API, removing the `iproute2` dependency from minimal OCI guests.
- Continuous lifecycle qualification now carries absolute and monotonic timers
  through two live sibling forks and verifies independent delivery and state.
- Continuous lifecycle qualification can keep a separate logical VM at true
  scale-to-zero for the full workload epoch while other VMs continue forking,
  then verify single-flight wake, clocks, timers, identity, and artifact cleanup.
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

- Deletes on a host ran one at a time: each held the host-wide terminal
  transition gate while its VMM was torn down. The teardown now runs outside
  that gate, under the VM's own operation gate, and only the state checks
  before it and the record transition after it take the host-wide gate. 40
  concurrent deletes went from 39 to about 270 a second.

- Deleting a VM could return 404 after it had been torn down: the periodic
  exit reconciler drove the same terminal transition the DELETE was driving,
  without the gate the DELETE held, and both tried to retire the VM's fork
  snapshot. The reconciler now takes the VM's operation gate and the
  terminal gate, in DELETE's order, and finishes only what is still pending.
  An 8-worker snapshot/restore/fork/delete stress went from 2 failures in
  120 iterations to none in 200.

- Creates slowed down as a host's history grew: every create counted its
  tenant's active VMs by reading the whole `vms` table, deleted VMs
  included, while holding the store. The count now uses an index on
  `(owner_key, status)` and is skipped for tenants without a quota, and the
  store keeps a 64 MiB page cache instead of SQLite's 2 MiB. With about
  3,000 past VMs, a warm-pool create went from about 3 ms to 1.2 ms, and
  eight at a time from about 340 to 1,300 per second on the reference host.

- Stopping a lazily restored VM (every fork child, and restores through
  taritd) waited up to half a second for its page-fault handler to notice:
  the handler only checked for shutdown between 500 ms polls. Shutdown now
  wakes it through an eventfd; deleting a fork child went from 506 ms to
  about 30 ms.

- A burst of creates could fail one with 409 "already has a supervisor-owned
  lifecycle task": a create claimed a warm VM that an on-demand restore had
  just published but whose restore task had not finished. Claims and idle
  reclaim now skip such a VM until its task ends.
- A create whose on-demand restore found the host full fell back to a cold
  boot, which needs the same capacity and starts slower and without the
  class's prewarm; it now waits for capacity and restores.

- taritd shuts down cleanly in about 2 s during a burst of creates; it took
  the 30 s HTTP drain timeout and sometimes exited with an error. Creates
  waiting for capacity kept waiting out their admission deadline after
  shutdown began, and a VM published while shutting down failed because the
  store writer had stopped, leaving a lifecycle the shutdown sweep could not
  reap. Such publications now land durably like terminal transitions do.
- A warm-pool golden VM stays paused while its disk is copied; it kept running
  after its RAM snapshot, so the copy could change underneath and fail.
- The warm pool now serves API creates. The API gave every create an id
  before the pool was consulted, and the pool only serves creates without one,
  so every create cold-booted. Creates naming an image are served by a class
  built from that image.
- A warm-pool refill fills back to the class's target, as documented. Each
  round started a bounded number of VMs and re-checked the low watermark, so a
  refill stopped as soon as the pool crossed it: a target of 100 settled at 76.
- A warm-pool golden VM runs a command before its snapshot, so a clone's first
  command no longer reads the guest shell and libc from disk (about 2 MiB,
  20-40 ms).
- AMD guests boot: the hypervisor CPUID bit is advertised.
- Confined path opens fail with EACCES instead of killing the thread.
- Restored guests no longer hang on a device interrupt raised just before
  the snapshot; virtio-rng state is part of the snapshot.
- Large guest block writes no longer hang the disk; a timed-out command no
  longer wedges the guest agent.
- A runtime directory stays in place while any VM in the process uses it.
- Unpacking an OCI image in a user namespace that maps only part of the ID
  range no longer fails on files it cannot chown.
- A create rejected while its request is checked (an image it may not use, a
  volume it cannot attach) leaves no VM record behind.
- Pause, suspend, and snapshot now require block, network, and vsock workers to
  acknowledge quiescence before capturing state or releasing guest RAM. Resume
  waits for every worker to leave its parked state before restarting vCPUs, so
  rapid resume/suspend cycles cannot reuse a stale acknowledgement. Worker
  startup, unexpected exit, and five-second quiescence failures are surfaced
  instead of publishing a partially functional VM or snapshot. If a partial
  vCPU or device-worker transition cannot be rolled back and acknowledged,
  the VM is fenced paused and the orchestrator durably records that state
  before returning the failure.
- OCI image conversion now verifies manifest, config, and layer descriptors and
  streams layers through disk-size-derived expansion, entry-count, file-size,
  path-length, and layer-count limits before unpack. Rejected images publish no
  rootfs and leave no private build workspace.
- The OCI compatibility gate follows a fork request for its full bounded
  lifetime, so a contended snapshot phase cannot make it miss the child repair
  state; VM-create and VM-delete failures now retain the exact HTTP status and
  response body.

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
