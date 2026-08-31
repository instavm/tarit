# Production readiness

Tarit is not yet approved for hostile multi-tenant production use.
`TARIT_PRODUCTION=1` enforces the implemented isolation and durability
configuration gates. This document records what the hardening work proves and
what remains a release blocker; billing and product-layer concerns are outside
its scope.

## Implemented and testable

- VM records carry monotonic revisions and an actual startup path
  (`cold`, `warm`, or `snapshot_restore`). SQLite and PostgreSQL reject
  same-revision records with different persisted content, reject stale resource
  incarnations, and fence ownership deletion.
- Every guest receives a private CoW rootfs overlay. Guest read-only mount
  semantics are independent from host-side base-image immutability.
- Every orchestrated jailed VMM can run as PID 1 in a dedicated PID namespace
  and in an empty process network namespace. taritd retains the host TAP
  topology and passes a validated TAP queue descriptor into the isolated VMM.
- Restored guests receive typed, validated address, route, gateway, and DNS
  repair instead of interpolated shell input. The guest agent applies and
  verifies IPv4 state through the kernel without depending on image-provided
  network tools, and restore readiness verifies host-to-guest reachability.
- Per-VM cgroup I/O limits and TAP ingress/egress limits are applied on boot,
  restore, and recovery. Reconciliation removes obsolete limits instead of
  leaving stale throttles in place.
- Peer RPC uses short-lived HMACs bound to method, canonical path, payload hash,
  nonce, source host, and target host. The shared key is never sent. Replay
  caches are bounded per source and globally, and legacy bearer-secret headers
  are rejected.
- Public VM and live-status responses omit host identity, ownership metadata,
  host paths, boot arguments, VMM sockets, process ids, and device
  configuration. Internal and VMM errors are not reflected to tenants.
- Fleet routing rejects unhealthy or stale owners instead of forwarding to
  their last advertised address. Deleted VMs have consistent not-found/list
  semantics.
- API bodies, request rate, concurrency, deadlines, pending PTY sessions, active
  PTY connections globally and per tenant/VM, WebSocket messages, VMM frames,
  and PTY idle time are bounded. Invalid credentials pass through the outer
  admission limit.
- Suspend is distinct from pause: it retains ownership and scheduler quota,
  releases resident guest memory, and requires successful rehydration before
  resume returns.
- Hibernation releases the VMM and scheduler allocation. HTTP, PTY, SSH, and
  share ingress activate a hibernated VM through a single-flight restore gate;
  failed activation leaves a retryable hibernated record instead of a second
  VMM or leaked capacity.
- Restore keeps KVM realtime advancement disabled so guest monotonic timers do
  not jump by the host hibernation interval. The mandatory repair barrier sets
  guest realtime from a host timestamp before admitting workload traffic.
- Live fork takes one memory, device, and disk boundary, publishes authenticated
  artifacts, and restores each child with independent lazy RAM and a private
  disk overlay. VMGenID notification on supported kernels is paired with a
  mandatory userspace repair barrier; older kernels use the barrier-only path.
- Provider-neutral local and NFS-backed raw block volumes are generation-fenced,
  tenant-scoped, and ordered with hibernate, resume, recovery, and deletion.
- Snapshot files are opened without following symlinks and validated before
  restore. VMM control frames have absolute deadlines, and OCI extraction uses
  private workspaces and no-follow file access.
- Orchestrated snapshots are full snapshots only. Incremental requests fail
  with `422` until every parent can be relocated into a durable manifest-backed
  chain; direct VMM incremental snapshots remain available for local testing.
- Release workflows pin third-party actions, publish checksums and an SPDX
  SBOM, and attest released artifacts.
- The production guest kernel is built reproducibly from checksum-pinned Linux
  source. Its config verifier requires the virtio devices used by the VMM and
  rejects unused legacy hardware entropy drivers.

## Required gates

Normal CI runs formatting, unit/integration tests, linting, dependency policy,
and security analysis. The protected-main KVM workflow additionally:

- builds release VMM and orchestrator binaries on a dedicated KVM runner;
- verifies real suspend memory release, state preservation, and bounded
  resume-to-exec latency;
- runs the VMM workspace KVM integration suite;
- takes 100 end-to-end cold and warm create-to-exec samples, requires 100%
  success, and enforces p99 ceilings of 5,000 ms cold and 1,000 ms warm; and
- runs strict VMM cold, snapshot-restore, and suspend/resume performance gates
  and uploads the measurements.

This workflow requires a registered runner labeled `self-hosted`, `linux`, `x64`, and `kvm`; until one is provisioned, the hardware gates remain pending.

Privileged KVM jobs run only from the protected `main` ref; pull-request code is
not executed on the persistent privileged runner.

## Stop-ship items

The earlier peer-mTLS/session-fencing, opaque authenticated artifact,
cross-node replication, and immutable signed-image stop-ships now have focused
implementation and c8i evidence recorded in the September roadmap. They remain
regression gates, but the following unresolved items now block release:

1. Close the intermittent clone-repair stall found by the continuous c8i load
   gate. The gate now carries a long-lived process with cached PRNG state,
   session-ticket key material, nonce allocation state, and a framework-style
   session cache; every successful fork and resume must rotate the first three,
   clear the cache, reject the inherited ticket, and leave the source unchanged.
   Alpine/Linux 5.10 runs passed 225 and 278 API operations, and Ubuntu/Linux
   6.6 runs passed 261 and 217. Earlier sequences produced a repair command that
   started and did not complete before the 30-second admission deadline.
   Restore control requests now use the lifecycle timeout, the repair exchange
   remains bounded by a host-monotonic deadline, and failures report the last
   repair stage. Retained state proved the application marker was complete while
   the agent slept on restored guest time; repair now uses a timer-independent
   signal/marker handoff with no guest-clock sleep before admission. The four-case
   Ubuntu/Alpine and Linux 6.6/5.10 service must complete sustained rotation
   without this stall before ingress admission is considered qualified.
2. Pass the remaining phase-by-phase kill, cancellation, dirty-rate
   non-convergence, corruption, and near-ENOSPC rollback tests without source
   pause leaks, duplicate writers, terminal-ID resurrection, or staged files.
   Unexpected UFFD handler exit, descriptor loss, UNMAP, and REMAP now fail the
   VMM closed and preserve retryable hibernated state. The fixed vCPU MSR
   omission is repaired, but the dynamic KVM custom-MSR set still needs the
   cross-kernel/build pvclock, PV-EOI, steal-time, and async-PF enabled/disabled
   compatibility matrix.
3. Qualify shared/cloud volume durability and failure recovery, including NFS
   server loss/restart with protected transport and object-store interruption,
   rather than extrapolating from local-block tests.
4. Complete the cross-build/CPU-template rejection matrix, multi-hour hibernation and
   ownership-lease qualification, and record 100-sample fork/snapshot/restore
   latency distributions on a larger reflink-capable fixture.
5. Run the full protected release workflow from an immutable source revision
   and retain its binaries, hashes, configuration, logs, and cleanup audit.

These are security or correctness boundaries, not optional roadmap features.
The source-derived threat and regression map is
[`docs/LIVE_FORK_KNOWN_FAILURE_AUDIT.md`](docs/LIVE_FORK_KNOWN_FAILURE_AUDIT.md).

## PaaS capability gaps (non-billing)

After the security stop-ship items are closed, a production PaaS control plane
still needs the following product milestones; they are not implemented claims:

- a declarative app, service, and deployment model;
- immutable revisions with rolling and blue-green rollout and rollback;
- durable volumes with backup and tested restore workflows;
- managed secrets and configuration with rotation;
- service discovery, ingress, custom domains, and TLS lifecycle management;
- centralized logs, metrics, traces, and deployment/runtime events;
- autoscaling plus disruption, affinity, and placement policies; and
- controlled artifact and image promotion between environments.
