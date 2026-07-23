# Per-VM isolation and writable disks

Every VM the orchestrator assigns is a genuinely separate microVM (its own KVM
instance, vCPU threads, guest RAM, kernel, vsock and PID) with a **writable,
isolated filesystem**. No user's VM data is ever shared with another VM.

## How

A single immutable rootfs image is the shared **base**, opened read-only. Each VM
gets its own **sparse copy-on-write overlay** on top of it (`vmm-core`'s
`blk_backend` CoW: a dirty-bitmap sparse file; writes copy-up into the overlay,
reads hit overlay-or-base). The base file stays byte-for-byte unchanged, so:

- **Isolated:** VM A's writes land in `A`'s overlay only; VM B never sees them,
  and vice-versa. Deleting a VM removes its overlay.
- **Writable by default:** the guest mounts `/` read-write and writes go to the
  overlay. `TARIT_ROOTFS_READONLY=true` instead requests a read-only guest mount
  without changing host-side base isolation.
- **Thin-provisioned / inflatable:** the overlay stores only written sectors, so
  it costs ~0 bytes until filled, up to the base's virtual size. Make the base a
  large sparse ext4 (e.g. 3 GB) to hand every VM a big writable disk for free.

Wiring: `tarit-vmm-client::VolumeConfig.overlay`, set per VM in
`taritd`'s `build_vmm_config` under the protected
`<TARIT_SOCKET_DIR>/overlays/<vm-uuid>.cow` directory; removed in `stop_vm`.
Restore-based warm-pool clones pass the same per-VM path as the VMM
`Restore.overlay` override, so clones restored from one golden snapshot do not
share writable disk state.

## Production confinement gate

`TARIT_PRODUCTION=1` currently fails closed. Non-production mode remains useful
for local development, but `taritd` does not yet stage the complete jail that a
hostile multi-tenant workload requires. Production mode must not be enabled
until the orchestrated launch path performs all of the following atomically:

- allocate a unique nonzero UID/GID and a private PID, mount and network
  namespace for every VM;
- create a root-owned, non-writable jail root and bind/pre-open `/dev/kvm`, the
  kernel/initramfs and base disks read-only, with only the VM overlay and
  runtime/snapshot directory writable by the VM identity;
- map the host control socket to an in-jail path and rewrite every Create,
  Restore and snapshot asset path so chroot cannot turn it into a missing or
  host-relative path;
- apply mandatory cgroup v2 CPU, memory, swap, PIDs and I/O limits before guest
  code runs, then verify the child is in the intended cgroup and namespaces;
- install coordinator/API-thread seccomp before accepting guest-controlled work,
  in addition to the vCPU and device-thread profiles; and
- retain mount, namespace, pidfd and artifact ownership in the supervisor until
  confirmed process exit, then unmount and remove them in a retryable cleanup
  path.

Partial staging must fail the VM spawn. Falling back to an unjailed process is
never a production recovery path.

## Verified (bare metal, c8i.metal-48xl)

- VM A writes `/iso.txt`; `test -w /` → exit 0 (writable).
- On VM B, `test ! -e /iso.txt` → exit 0 (A's file absent = isolated).
- After B writes its own `/iso.txt`, A still reads its original content
  (exit 0); no cross-VM leakage.
- Base rootfs md5 unchanged; A and B have separate sparse overlays.
- Guest held a 1.8 GB file (disk inflates past the ~1 GB base content).
- pause / resume / snapshot / restore all return success on an overlay VM.

## Restore warm-pool clones

With `restore = true`, the warm-pool replenisher builds one golden snapshot for a
class, then restores each warm clone with `overlay_path_for(<clone-id>)`. The
golden snapshot and shared read-only base remain cached; per-clone overlays are
removed when those VMs are stopped or deleted.
