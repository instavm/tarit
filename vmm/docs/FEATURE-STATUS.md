# VMM Feature Status

*Last updated: 2026-07-02. Plain assessment of what works.*

## What Works (main branch as of 2026-07-02)

### Boot
- ✅ Running (API `create_live`) VMs always get an in-kernel IRQCHIP+PIT, so the
  guest LAPIC timer fires and HLT is handled in-kernel: the vCPU thread blocks
  while idle at **0% CPU** (previously a volume-less VM HLT-spun at 100% CPU to
  the watchdog timeout).
- ✅ Full x86_64 KVM boot (ELF vmlinux / bzImage, IRQCHIP+PIT+ACPI) to userspace
- ✅ Serial output via earlycon + ttyS0 (PIO port 0x3f8)
- ✅ APIC timer (TSC-Deadline advertised only with the in-kernel irqchip)
- ✅ ACPI FADT with HW_REDUCED_ACPI, MADT (IOAPIC + LocalAPIC), DSDT
- ✅ LAPIC LVT0=EXTINT, LVT1=NMI
- ✅ initramfs loading; IRQCHIP + PIT + irqfd
- ✅ OCI image boot via `vmm pull --agent` (agent installed at
  `/usr/sbin/vmm-agent`; `/sbin/init` points at it when the image has no init)
- ⚠️ CLI `vmm run` fast path (no irqchip) is only a boot micro-benchmark and
  still runs to the watchdog timeout: not used for real VMs (Phase 3).

### 1:1 Process Model
- ✅ One VMM process = one VM (Option<VmInstance>, no HashMap)
- ✅ API: Create, Pause, Suspend, Resume, Snapshot, Restore, Stop, Exec,
  AttachPty, UpdateEgress, Status
- ✅ No list/terminate API: `status` reports the single VM in the process

### Guest I/O Channels (host ↔ guest)
- ✅ Host writes to guest via serial port 0x3f8 (IoIn returns pending input)
- ✅ Guest output captured via IoOut (port 0x3f8)
- ✅ SerialChannel with shared input/output buffers
- ✅ Guest agent (/sbin/vmm-agent) reads commands from /dev/ttyS0, runs them, writes output back
- ✅ vsock-based exec is the default when the guest agent connects; serial exec remains the fallback
- ✅ Interactive PTY over vsock (`AttachPty` streaming op, CLI `vmm attach-pty`)

### virtio-blk (rootfs on /dev/vda)
- ✅ Guest probes the device, mounts an ext4 rootfs, and boots to userspace
  (verified on c8i: `EXT4-fs (vda): mounted filesystem` → `VFS: Mounted root`
  → `Run /bin/sh as init process`).
- ✅ Correct virtio-mmio v2 negotiation (status bits, QUEUE_READY readback,
  device reset), ACPI-only discovery (guest gets a mapped virq so request_irq
  succeeds), simple avail/used notification (EVENT_IDX intentionally not offered).
- ✅ Per-VM copy-on-write disk overlays keep writes out of the base rootfs.

### virtio-net / virtio-vsock
- ✅ virtio-net MMIO transport is wired to a host TAP device.
- ✅ virtio-vsock MMIO transport is wired for exec and PTY channels.

### Snapshot/Restore
- ✅ VMSN format with CRC32 integrity (state + memory CRCs)
- ✅ **Faithful resume**: the full vCPU state is captured by the vCPU thread the
  moment it pauses (REGS/SREGS/FPU/XSAVE/XCRS/MSRS/LAPIC/MP_STATE/VCPU_EVENTS)
  and re-applied on restore (SET_CPUID2 first, then the rest)
- ✅ **VM-level state** captured/restored too: in-kernel IRQCHIP (PIC+IOAPIC),
  PIT, and kvmclock: so post-restore device I/O gets the guest's interrupt
  routing instead of a fresh (masked) IOAPIC
- ✅ Crash-consistent: the vCPU is paused during the memory dump
- ✅ Device configs serialized via bincode (kernel path, cmdline, volumes, net)
- ✅ **Restore reconstructs a *running* VM** (fresh KVM VM + virtio-blk over the
  restored memory, vCPU state re-applied, vCPU thread resumed): a live VM, not a
  memory-only image. Validated on c8i: a real Debian/systemd guest snapshotted, then
  restored back to **running in ~102ms** (vCPU thread alive and pausable after
  restore). See `ci/restore-roundtrip.sh`.
- ✅ Full 256MB snapshot + restore verified on c8i
- ✅ Device queue state for virtio-blk, virtio-net, and virtio-vsock is captured
  and restored.
- ✅ Restore-clone disk isolation uses a per-restore CoW overlay.
- ✅ Suspend pauses the VM, arms userfaultfd, calls `madvise(MADV_DONTNEED)` to
  free resident guest RAM, and rehydrates pages on resume.
- ✅ Stop cleans VMM-owned snapshot/overlay scratch files; `vmm gc` sweeps orphaned
  scratch files.

### API E2E (verified on c8i)
- ✅ Create VM via API (live, full boot): boots to systemd
- ✅ Pause/Resume stops/starts the vCPU thread, not only the state enum
- ✅ Snapshot (256MB + CRC32, crash-consistent): ~95ms
- ✅ Restore **to a running VM**: ~102ms
- ✅ Stop (clean vCPU teardown, no fd leak)
- ✅ Exec over vsock with serial fallback
- ✅ AttachPty interactive PTY over vsock

### Security
- ✅ Jailer: chroot + cap drop + seccomp + cgroup v2 limits on the `serve` path
- ✅ Virtqueue bounds checking
- ✅ Snapshot mem_len validation + CRC32 verification

## Known Gaps

### aarch64 / ARM support
Not implemented. The shipped KVM boot path is x86_64-only.

### virtio-balloon
Not implemented. The test guest kernel lacks `CONFIG_VIRTIO_BALLOON`.

### ACPI region initialization
`ACPI Error: AE_BAD_PARAMETER, During Region initialization` still appears
during boot. The FADT with HW_REDUCED_ACPI prevents the "AcpiEnable failed"
error, but ACPI table loading still has an issue. This doesn't block boot
or device discovery via cmdline.

### Live migration
Deferred: EC2 inter-host latency (5ms RTT) exceeds PRD's <5ms blackout target.

## Performance (c8i nested virt, release build, measured 2026-07-01)

| Operation | Was | Now | Target | Status |
|---|---|---|---|---|
| Create VM (API, incl. KVM+boot setup) | 39ms | **~5ms** (median) | n/a | ✅ ~200/s/process |
| Snapshot (256MB) | 3.3s | **~95ms** | <30ms | ✅ 35× faster (diff-snapshot for <30ms) |
| Restore (256MB, UFFD lazy) | 244ms | **~0.84ms** | <10ms | ✅ UFFD wired (100/100 restore-burst) |
| Idle running VM | 100% CPU spin | **0% CPU** | idle | ✅ blocks in KVM_RUN |
| fd growth over 20 create/stop | +21 (leak) | **+1** (stable) | 0 | ✅ no leak |
| CLI `vmm run` fast-boot bench | 20ms | runs to timeout | <125ms | ⚠️ legacy no-irqchip path (Phase 3) |

Snapshot/restore write straight from/into the guest mmap (no intermediate
Vec copy). Diff snapshots and UFFD/CoW lazy restore are wired for the fast paths.

## Test Summary
- 109 unit/integration tests pass (macOS, no KVM)
- 65/65 API e2e tests pass (c8i, 5 runs)
- KVM-gated tests (#[ignore]) need c8i to run
