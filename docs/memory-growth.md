# Grow guest memory when restoring a new snapshot

Experimental; requires Linux x86-64/KVM and a new guest kernel/agent/template.
The hardware acceptance gate below must pass before deployment. This feature
is opt-in and does not convert old snapshots or add live shrinking.

Create a VM with **2 GiB boot RAM and a 4 GiB maximum reservation**:

```json
POST /v1/vms
{"memory_mib":4096,"boot_memory_mib":2048,"vcpus":1}
```

Snapshot it normally (`POST /v1/vms/{id}/snapshot`, `{"diff":false}`), then:

```json
POST /v1/restore
{"snapshot_id":"<returned UUID>","target_memory_mib":4096}
```

The resumed guest keeps its processes and existing RAM. The virtio-mem driver
plugs the extra RAM and Linux onlines it. Restore succeeds only after both the
VMM's plugged-block count and the agent's read-only sysfs check confirm the
requested amount. The check uses a 30-second host-clock deadline; failure drops
the unpublished restored VM and follows the existing orchestrator cleanup path.
Subsequent snapshots retain the plugged bitmap and requested size, so the next
restore may omit `target_memory_mib`.

The low-level equivalent is `memory: {size_mib:4096, boot_size_mib:2048}` in the
VMM create config and `target_memory_mib:4096` in its restore request. The CLI
supports `vmm create --mem 4096 --boot-memory-mib 2048 ...` and
`vmm restore --snapshot ... --target-memory-mib 4096` (normal existing CLI
network/overlay restrictions still apply). Python `CreateVmRequest` /
`RestoreRequest` and the generated TypeScript schemas include the new fields.

## Limits and resource semantics

- `memory_mib` remains the **maximum reserved** RAM in VM records, placement,
  quotas, cgroups and snapshot disk-space admission. It does not become the
  guest's currently online RAM. Use guest `/proc/meminfo` to observe usable RAM.
- The maximum backing is allocated at creation and the snapshot RAM extent spans
  that maximum, including unplugged blocks. A 2 GiB boot / 4 GiB maximum template
  therefore has a 4 GiB RAM snapshot extent. This version offers no host-memory
  overcommit or smaller snapshot format.
- Boot RAM must be 128 MiB aligned, between 128 and 3328 MiB, and below the
  maximum. Maximum and requested total must also be 128 MiB aligned; the
  existing 65536 MiB ceiling applies. Targets below the saved requested total
  or above the saved maximum fail. An omitted target preserves the saved target.
- Boot RAM stays at GPA zero; the fixed growth aperture starts at GPA 4 GiB.
  No saved address or existing page moves when growing. Packed snapshot offsets
  and DMA dirty tracking account for this split, including lazy restore and
  suspend/re-arm. Authenticated lazy restore still verifies chunk hashes.
- A hotplug template cannot claim an ordinary warm-pool VM. Configurable warm
  classes and live resize endpoints are outside this first version.
- The VMM maps the entire private reserved aperture, including unplugged blocks;
  it does not advertise `VIRTIO_MEM_F_UNPLUGGED_INACCESSIBLE`. The reservation and
  existing process cgroup remain the isolation limit for a hostile guest.
- Custom guests must include virtio-mem and the updated Tarit agent. Memory-only
  fast-boot snapshots do not support expansion. Rebuild templates; no manual
  `/sys/.../probe` or legacy snapshot conversion is attempted.

## Build and rollout

Build the kernel with `vmm/guest/build-minimal-kernel.sh` from this branch. Both
its base config and override list enable `CONFIG_VIRTIO_MEM=y`; the build checks
that memory hotplug/hotremove and virtio-mem survived `olddefconfig`. Rebuild the
agent and image provenance/boot digest using the normal image pipeline. The
existing downloadable kernel release is not updated by this patch.

Upgrade the VMM, taritd, guest kernel and agent together on a canary host before
creating new templates. New device state has a framed snapshot trailer; older
VMMs cannot restore these snapshots. Explicit peer growth uses a distinct
internal endpoint so older taritd peers reject it instead of silently ignoring
`target_memory_mib`. Ordinary requests retain their existing route and defaults.
No deployment, release artifact, snapshot migration or production configuration
change is included.

## Hardware acceptance

On an explicitly authorized, isolated Linux x86-64 KVM test host, build this
branch's VMM (`boot` feature) and taritd and supply the new kernel and an ext4
agent-enabled rootfs. Budget at least 16 GiB free RAM and 20 GiB scratch disk;
use a reflink-capable scratch filesystem for Tarit's snapshot lifecycle.
The default gate refuses to start below 12 GiB `MemAvailable`. For an approved
smaller host, `TARIT_MEMORY_GROWTH_SERIAL=1` stops the source before each restore,
admits at most one 4 GiB VM, and requires at least 6 GiB `MemAvailable`. That mode
skips simultaneous source/clone isolation and source-liveness checks; it still
checks PID/nonce/RAM preservation, second-generation restore and failed-target
cleanup. Do not run either mode if existing workloads leave less available RAM.
Use a separate temporary kernel-build directory: the kernel script replaces
its own source/build directories and must not point at shared baseline fixtures.

```sh
TARIT_KERNEL=/path/to/new/vmlinux \
TARIT_ROOTFS=/path/to/rootfs.ext4 \
TARIT_VMM_BIN=/path/to/new/vmm \
TARITD_BIN=/path/to/new/taritd \
TARIT_TEST_SOCKET_ROOT=/path/to/scratch \
bash orch/tests/e2e_memory_growth.sh
```

The script builds the agent and a static C witness, injects both into its private
copy of the rootfs, and starts its own local taritd. It never targets an existing
service or changes host network/firewall configuration. It verifies:

1. Approximately 2 GiB of guest-visible RAM at boot.
2. Snapshot/restore to 4 GiB with the same running PID, in-memory nonce and
   per-page markers.
3. Touching and verifying 3 GiB of anonymous RAM, beyond the boot capacity.
4. Source/clone memory isolation and a second full snapshot/restore preserving
   the expanded memory and process state without a target override.
5. Oversized and shrinking targets fail without publishing a running VM;
   source stays healthy and subsequent valid restores can reuse the reservation.

Local validation at draft creation: 152 device/backend tests, 31 protocol tests,
461 orchestrator tests plus the new reservation test, 123 Linux core tests in a
container without KVM, and 24 TypeScript / 12 Python SDK tests pass. The complete
Linux VMM and orchestrator test targets compile; guest agent and witness compile
with strict C warnings. Rust 1.99 VMM Clippy without the boot feature and protocol Clippy pass. The
Rust 1.88 Linux boot-feature Clippy invocation is blocked by existing duplicate
`cfg` attributes in loader, UFFD and jailer modules, outside this change.

Still required before readiness claims: run that hardware gate; qualify the
same flow with production jail/cgroup/seccomp, signed lazy snapshots, SMP,
network/persistent-volume load, and forced guest-agent/driver timeout. The local
container tests do not provide KVM or prove guest hotplug behavior.

Protocol reference: Linux's
[virtio-mem wire definitions](https://github.com/torvalds/linux/blob/v6.12/include/uapi/linux/virtio_mem.h).
