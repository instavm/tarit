# Live fork and lifecycle known-failure audit

The [release checklist](../PRODUCTION_READINESS.md) tracks the candidate and its
four-case rotation. This audit records what failed, which tests exercise the
failure, and where coverage ends.

## Earlier live-fork repair failure

The Azure run on `e9e6a7c` stopped during live-fork guest repair after 43,106
operations and 737 verified forks. Serial input stalled before the host could
record the guest repair stage. None of the four cases completed. The next
build added stage reporting. [Repair diagnostics](https://github.com/instavm/tarit/pull/191)

## Wake repair timeout

On November 22, 2026, revision `696f4ed` failed the first six-hour Azure case
on Ubuntu 24.04 / Linux 6.6.155, seed `20261122`. After 72 minutes, 14,346 API
operations and 261 verified forks, exec tried to wake a four-vCPU hibernated VM.
The last repair stage reported was `userspace`, immediately before the post-fork
hook. The 30-second host deadline expired without a completion acknowledgement.
The trace does not identify what blocked inside that interval.

On the same binaries, 100 focused wake replays passed. A separate two-hour
instrumented run passed 419 verified forks. Its positive control completed
repair and its deliberately stopped workload failed admission. A cold-start
writer race also reproduced before any fork or wake and has a separate
[fixture fix](https://github.com/instavm/tarit/pull/194). The mixed-workload
timeout remains open pending sustained qualification.

## Upstream-derived invariants

| Failure area | What the tests established | Coverage boundary |
| --- | --- | --- |
| Running fork | High-dirty tests preserved source and child data across Ubuntu/Alpine and both kernels. Ten injected VMM capture/publication failures resumed the source and removed staging. Local and cross-node retries recovered the same child after daemon death. | Remaining cancellation and non-convergence paths, physical host loss and non-overcommitted local and cross-node latency distributions. |
| Lazy RAM / UFFD | Handler loss, unexpected unmap/remap and descriptor loss terminated the VMM. API recovery preserved retryable hibernation, guest data and capacity. Discard, zero-refault and mixed-residency faults passed. | Covered for the in-process handler and tested guest matrix. |
| Balloon and restore | Eighty snapshot/restore/balloon cycles across both distributions and kernels verified guest data, host reclaim and kernel liveness under a 768 MiB cgroup limit. | Covers balloon and lazy memory interaction. The mixed wake failure above followed a balloon operation; causation is unknown. |
| Clone uniqueness | Siblings had distinct clone IDs, boot IDs and kernel random samples. Linux 6.6.155 delivered VMGenID. Linux 5.10.230 used the guest repair barrier. A test application rotated cached PRNG, ticket keys and nonces, cleared session state and rejected an inherited ticket. | Applications must implement their own cached-state repair. The sustained admission gate remains open. [Guest hook](../vmm/guest/agent/README.md#post-fork-repair) |
| Time and timers | Bounded hibernation and sibling-fork cases verified host-relative realtime, paused monotonic time and delivery of both timer classes after resume. | Multi-hour hibernation and ownership lease tests. |
| vCPU MSRs | Linux 6.6.155 and 5.10.230 restored online/offline vCPU state, including pvclock, PV-EOI, steal-time and async-PF. After onlining, the secondary vCPU made progress. | Cross-build compatibility with those features enabled and disabled. |
| Restored device I/O | Active serial/vsock streams, stale transport reset, queued TX and delayed block I/O passed real-KVM or device regression tests. The snapshot barrier drains device workers after vCPU producers stop. | Regression gate for the tested UART, vsock, net and block devices. |
| Snapshot compatibility | Checks covered corrupt components, CPU-template and state-ABI rejection before publication, v1-to-v2 restore, 4 GiB split RAM, swap, SMP, first-diff semantics and hardlink alias protection. | Retain these gates for format changes. Outstanding publication failures appear below. |
| CoW disk and artifacts | Storage pressure reproduced ENOSPC during publication. Range reflinks avoided a second full RAM copy on CoW storage. | Inject ENOSPC at each publication phase and verify source, files and filesystem after crash/restart. |
| NFS and shared volumes | Three loopback NFSv4.1 restarts each with AUTH_SYS and Kerberos privacy preserved an in-flight synchronous write. Busy detach, durable reopen and cleanup passed. Production requires `krb5p`. | Independent server/network loss, stale file handles, managed TLS helpers and cloud volume recovery. [NFS fixture](../orch/tests/e2e_nfs_kerberos.sh) |
| Lifecycle concurrency and ownership | Fork retry checks fenced duplicate IDs, wrong targets and stale owners. Two concurrent wake claims produced one winner in 100 races each on PostgreSQL 14 and 16. PostgreSQL 16 also recovered across two abrupt restarts on one host. | Independent-host failover, multi-hour leases and remaining transfer/publication fault cases. [Handoff tests](https://github.com/instavm/tarit/pull/193) |
| Isolation | KVM gates verified host virtualization remained available while guests lacked nested VMX/SVM and access to the KVM device. | Repeat syscall and path confinement gates as host kernels, microcode and devices change. [Isolation](../orch/docs/ISOLATION.md) |
| OCI input | Tests rejected digest, expansion and inode violations. Six interrupted TLS pulls rolled back fresh or existing destinations. Six whole-pull retries passed filesystem and payload verification, including a moved registry tag. | Registry retry coverage applies to image conversion. Remote object-volume recovery is a separate open gate. [Registry fixture](../vmm/ci/oci-registry-interruption.py) |

## Release execution rule

Exercise a failure at its own layer using real KVM, the storage service or the
fleet store as applicable. Retain the revision, host, kernel, image, seed,
operation and pre-state. For a failed soak, keep the per-operation trace and
guest/host logs. Check for stalls, panics, OOMs, seccomp kills, UFFD exits and
leaked processes, mounts or files before recording the result.

## Authoritative upstream references

- [Firecracker snapshot support and security/uniqueness](https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/snapshot-support.md)
- [Firecracker entropy for clones and VMGenID limits](https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/random-for-clones.md)
- [Firecracker userfaultfd restore handling](https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/handling-page-faults-on-snapshot-resume.md)
- [Firecracker balloon semantics](https://github.com/firecracker-microvm/firecracker/blob/main/docs/ballooning.md)
- [Firecracker restored-balloon RCU-stall report #5566](https://github.com/firecracker-microvm/firecracker/issues/5566)
- [QEMU post-copy state machine and failure recovery](https://www.qemu.org/docs/master/devel/migration/postcopy.html)
- [QEMU fast snapshot load synchronization](https://www.qemu.org/docs/master/devel/migration/fast-snapshot-load.html)
- [Linux userfaultfd manager obligations](https://kernel.org/doc/html/latest/admin-guide/mm/userfaultfd.html)
- [KVM guest-MSR enumeration API](https://docs.kernel.org/virt/kvm/api.html#kvm-get-msr-index-list-kvm-get-msr-feature-index-list)
- [KVM-specific paravirtual MSRs](https://docs.kernel.org/virt/kvm/x86/msr.html)
- [Linux NFS hard/soft and transport-security semantics](https://www.man7.org/linux/man-pages/man5/nfs.5.html)
- [Linux overlay/copy-up crash-durability semantics](https://docs.kernel.org/filesystems/overlayfs.html)
- [OCI descriptor verification requirements](https://github.com/opencontainers/image-spec/blob/main/descriptor.md)
- [Firecracker production host isolation recommendations](https://github.com/firecracker-microvm/firecracker/blob/main/docs/prod-host-setup.md)
- [Firecracker changelog of snapshot/device regressions](https://github.com/firecracker-microvm/firecracker/blob/main/CHANGELOG.md)
- [AWS c8i nested-virtualization architecture and responsibility boundary](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/amazon-ec2-nested-virtualization.html)
