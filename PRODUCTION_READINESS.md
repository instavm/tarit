# Production readiness

**Hold v0.2.0.** The release candidate is under qualification. Hostile multi-tenant
production use still has open security and durability gates. Setting
`TARIT_PRODUCTION=1` enforces startup and admission checks. [Configuration](orch/docs/CONFIGURATION.md#cluster-and-durability)

## Release evidence

The candidate build and SDK package checks passed. Sustained fork and wake
qualification remains open. [Failure audit](docs/LIVE_FORK_KNOWN_FAILURE_AUDIT.md#wake-repair-timeout) · [Soak procedure](orch/docs/OPERATIONS.md#continuous-lifecycle-qualification)

## Stop-ship items

| Gate | Evidence still required |
| --- | --- |
| Lifecycle and ownership | Complete the guest/kernel rotation and verify multi-hour hibernation and ownership handoff between independent hosts. |
| Fork and snapshot rollback | Verify source recovery and cleanup after cancellation, corruption and storage exhaustion. |
| CPU compatibility and latency | Check cross-build restore and measure fork, snapshot and restore latency on representative hosts. |
| Durable storage | Verify independent NFS server loss, stale handles, host-crash durability and remote object recovery. [Storage coverage](orch/docs/SANDBOX-MIGRATION-AND-VOLUMES.md#acceptance-gates) |
| Release from main | Include the NFS volume SDK fix, run the protected checks and verify the final artifacts. [Release workflow](.github/workflows/release.yml) |

## Implemented and testable

- **Isolation and admission.** Production startup requires per-VM UID/GID jails, PID and network namespaces, cgroups and a signed-image policy. taritd owns host TAPs and passes validated descriptors to jailed VMMs. API admission bounds request bodies, concurrency, deadlines and PTY usage. [Isolation](orch/docs/ISOLATION.md)
- **Ownership and recovery.** Revision and incarnation checks fence writes and deletion. Suspend parks the vCPUs and device workers and releases resident RAM. Hibernation also releases the VMM and its capacity. Concurrent ingress joins one wake operation. [Lifecycle and recovery](orch/docs/RESILIENCE.md)
- **Fork and restore.** The running source and child share a checkpoint of RAM, devices and writable root disk. Each child has independent lazy memory and a private disk overlay. Before admitting traffic, Tarit repairs clocks, entropy, clone identity and network configuration. Applications with cached keys or PRNG state need a guest hook. [Failure audit](docs/LIVE_FORK_KNOWN_FAILURE_AUDIT.md)
- **Artifacts and storage.** Orchestrated snapshots are standalone and authenticated. Fleet fork verifies transferred artifacts and enforces replica policy. OCI conversion verifies descriptors and bounds extraction. Production generic NFS requires Kerberos privacy. [Snapshot API](orch/docs/API.md) · [Volume providers](orch/docs/SANDBOX-MIGRATION-AND-VOLUMES.md)
- **Release artifacts.** The guest kernel uses checksum-pinned source. Release builds checksum and attest binaries and SDK packages. Registry publishing has a separate opt-in. [Guest kernel](vmm/guest/README.md) · [SDK release](sdk/README.md#versioning-and-publication)

## Required gates

CI checks formatting, tests, lint and dependency policy. The protected-main KVM job
checks lifecycle, disk isolation and cold and warm create-to-exec latency.
[KVM workflow and thresholds](.github/workflows/kvm.yml)

## PaaS capability gaps (non-billing)

Product work beyond these release gates includes declarative deployments and
rollback, volume backup and restore, managed secrets, service discovery and
TLS lifecycle, centralized observability, placement and disruption policies,
and promotion of images between environments.
