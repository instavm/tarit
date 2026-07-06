# Remaining work

The authoritative backlog and the **Path to production-grade PaaS (VMM +
orchestrator)** roadmap live in [`docs/remaining_work.md`](docs/remaining_work.md).

Quick summary of what remains on the path to a production PaaS (see that doc for
detail):

- **VMM:** restore-clone disk isolation (per-restore CoW overlay), suspend that
  frees RAM, snapshot GC + durable/cross-node snapshot storage, wire the jailer
  (cgroup/seccomp/chroot) into the serve path, virtio-blk durability + balloon,
  live-snapshot consistency as a CI gate, aarch64.
- **Orchestrator (taritd):** snapshot-restore replenishment + CPU-isolated
  rate-limited refill + hysteresis (see orch/docs/REPLENISHMENT.md), real auth/
  RBAC/mTLS + per-tenant quotas, HA (write-behind crash recovery, Postgres fleet
  failover, cross-node restore/migration), cross-cloud autoscaler drivers,
  networking at scale, graceful drain, OCI->golden image pipeline, observability.
- **Cross-cutting:** ComputeSDK bench in CI + quarterly 10k-scale stress, close the
  `ci/check.sh` clippy/boot gaps, runbooks + tenancy/security model docs.

Current headline (bare metal, ComputeSDK methodology, `node -v` TTI): sequential
p95 ~38ms, burst(100) ~57ms, staggered(200ms) ~39ms, 100% success — ~10x faster
than the fastest public ComputeSDK providers.
