# Production readiness

**Hold v0.2.0.** The release candidate is under qualification. Hostile multi-tenant
production use still has open security and durability gates. Setting
`TARIT_PRODUCTION=1` enforces startup and admission checks. [Configuration](orch/docs/CONFIGURATION.md#cluster-and-durability)

## Release evidence

| Revision | Verified result |
| --- | --- |
| Main `08cbfb3` | The protected-main KVM lifecycle and performance job passed. [KVM run](https://github.com/instavm/tarit/actions/runs/44989376832) |
| Rehearsal `696f4ed` | The first six-hour Azure case stopped after 72 minutes on Ubuntu 24.04 / Linux 6.6.155. An exec-triggered wake exceeded the 30-second repair deadline after 261 verified forks. A separate two-hour diagnostic passed 24,593 API operations and 419 verified forks. [Wake failure](docs/LIVE_FORK_KNOWN_FAILURE_AUDIT.md#wake-repair-timeout) |
| Candidate `7b1ec28` | The build-only workflow produced attested Linux artifacts. Installed Python wheel, sdist and npm consumer tests passed. On November 22, 2026 at 15:42 UTC, the four-case Azure rotation started with the verified VMM and daemon. [Candidate build](https://github.com/instavm/tarit/actions/runs/45039199864) |

The matrix pairs Ubuntu 24.04 and Alpine 3.20 with Linux 6.6.155 and 5.10.230.
Each case requires six continuous hours plus crash recovery, ingress wake and
persistent-volume checks. A failed case stays in the record alongside its rerun.
[Soak procedure](orch/docs/OPERATIONS.md#continuous-lifecycle-qualification)

## Stop-ship items

| Gate | Evidence still required |
| --- | --- |
| Lifecycle and ownership | Complete all four epochs and 12 auxiliary checks. Verify multi-hour hibernation, timer and application-state repair, and fenced ownership handoff across independent hosts. Single-host PostgreSQL races and restart recovery passed. [Ownership tests](https://github.com/instavm/tarit/pull/193) |
| Fork and snapshot rollback | Finish cancellation, dirty-rate non-convergence, corruption and near-ENOSPC cases. Verify source progress, a single writer and removal of staged files across crash/restart. |
| CPU compatibility and latency | Exercise guest-enabled paravirtual MSRs across VMM builds. Record 100-sample fork, snapshot and restore distributions on a larger reflink-capable host, including cross-node forks. |
| Durable storage | Exercise independent NFS server loss, stale handles and host-crash durability. Generic NFS loopback restarts passed with an in-flight write. The immutable object provider uses local files. Remote conditional writes, checksum verification and outage recovery remain open. [Storage coverage](orch/docs/SANDBOX-MIGRATION-AND-VOLUMES.md#acceptance-gates) |
| Release from main | Verify the merged revision against the rehearsed artifacts, run the protected gates and retain hashes, configuration, logs and teardown results for the release build. [Release workflow](.github/workflows/release.yml) |

The candidate SDK rejects a valid NFS volume response. The [OpenAPI and SDK correction](https://github.com/instavm/tarit/pull/197) changes the embedded schema and both SDKs. Rebuild and attest the release artifacts from the revision that includes it.

## Implemented and testable

- **Isolation and admission.** Production startup requires per-VM UID/GID jails, PID and network namespaces, cgroups and a signed-image policy. taritd owns host TAPs and passes validated descriptors to jailed VMMs. API admission bounds request bodies, concurrency, deadlines and PTY usage. [Isolation](orch/docs/ISOLATION.md)
- **Ownership and recovery.** Revision and incarnation checks fence writes and deletion. Suspend parks the vCPUs and device workers and releases resident RAM. Hibernation also releases the VMM and its capacity. Concurrent ingress joins one wake operation. [Lifecycle and recovery](orch/docs/RESILIENCE.md)
- **Fork and restore.** The running source and child share a checkpoint of RAM, devices and writable root disk. Each child has independent lazy memory and a private disk overlay. Before admitting traffic, Tarit repairs clocks, entropy, clone identity and network configuration. Applications with cached keys or PRNG state need a guest hook. [Failure audit](docs/LIVE_FORK_KNOWN_FAILURE_AUDIT.md)
- **Artifacts and storage.** Orchestrated snapshots are standalone and authenticated. Fleet fork verifies transferred artifacts and enforces replica policy. OCI conversion verifies descriptors and bounds extraction. Production generic NFS requires Kerberos privacy. [Snapshot API](orch/docs/API.md) · [Volume providers](orch/docs/SANDBOX-MIGRATION-AND-VOLUMES.md)
- **Release artifacts.** The guest kernel uses checksum-pinned source. Release builds checksum and attest binaries and SDK packages. Registry publishing has a separate opt-in. [Guest kernel](vmm/guest/README.md) · [SDK release](sdk/README.md#versioning-and-publication)

## Required gates

CI runs formatting, tests, lint and dependency policy. The protected-main KVM
job uses a one-job runner labeled `tarit-kvm-ephemeral`. It exercises suspend,
disk isolation, checkpoint/wake loops and the VMM integration suite. It takes
100 cold and 100 warm create-to-exec samples. Both require 100% success,
with p99 limits of 5,000 ms cold and 1,000 ms warm. The continuous release rotation is a separate run.
[KVM workflow and thresholds](.github/workflows/kvm.yml)

## PaaS capability gaps (non-billing)

Product work beyond these release gates includes declarative deployments and
rollback, volume backup and restore, managed secrets, service discovery and
TLS lifecycle, centralized observability, placement and disruption policies,
and promotion of images between environments.
