# Production readiness

Tarit is not yet approved for hostile multi-tenant production use.
`TARIT_PRODUCTION=1` intentionally fails closed while the mandatory isolation
path listed below is incomplete. This document records what this hardening
change proves and what remains a release blocker; billing and product-layer
concerns are outside its scope.

## Implemented and testable

- VM records carry monotonic revisions and an actual startup path
  (`cold`, `warm`, or `snapshot_restore`). SQLite and PostgreSQL reject
  same-revision records with different persisted content, reject stale resource
  incarnations, and fence ownership deletion.
- Every guest receives a private CoW rootfs overlay. Guest read-only mount
  semantics are independent from host-side base-image immutability.
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
- Snapshot files are opened without following symlinks and validated before
  restore. VMM control frames have absolute deadlines, and OCI extraction uses
  private workspaces and no-follow file access.
- Orchestrated snapshots are full snapshots only. Incremental requests fail
  with `422` until every parent can be relocated into a durable manifest-backed
  chain; direct VMM incremental snapshots remain available for local testing.
- Release workflows pin third-party actions, publish checksums and an SPDX
  SBOM, and attest released artifacts.

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

1. Route every orchestrated VMM through a unique per-VM uid/gid jail with
   private staged assets, mount and PID namespaces, mandatory cgroup placement,
   cleanup, and coordinator-thread seccomp. The optional jailer is not yet the
   standard `taritd` launch path.
2. Move peer routes to a separate listener with mandatory mTLS, certificate
   rotation, and host-session fencing. Heartbeat rows currently identify a host
   by a reusable string rather than an authenticated boot/session lease.
3. Replace public snapshot paths and host ids with opaque artifact handles backed
   by durable, authenticated storage and a fleet-wide artifact index. Add
   cryptographic measurements or fs-verity/Merkle metadata so lazy restore can
   avoid a full memory scan without weakening integrity.
4. Replace shell-based resumed-guest network repair with a typed guest-agent
   operation covering address, route, gateway, and DNS state. Gate it with a
   real KVM restore test that proves DNS and outbound connectivity.
5. Replicate kernels, rootfs images, snapshots, and required guest-agent
   artifacts across failure domains; node-local paths are not an HA substrate.
6. Resolve images to immutable digests and enforce signature/provenance policy
   before admission. Mutable OCI tags are insufficient for production rollout
   or rollback guarantees.
7. Enforce per-tenant and per-VM I/O and network bandwidth quotas in addition to
   CPU, memory, process, and VM-count limits.

These are security or correctness boundaries, not optional roadmap features.

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
