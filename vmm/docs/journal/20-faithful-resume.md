# 20 — Faithful resume (and the robustness bugs it flushed out)

**Milestone:** snapshot → restore → **resume a running guest**, validated on c8i
with a real Debian/systemd rootfs.

## What I did

The snapshot format used to be a memory image plus a handful of vCPU registers
(rip/rsp/cr*), and `create_live` stored a zeroed vCPU state. A restore could
therefore never resume execution — it rebuilt a *paused, memory-only* VM.

Faithful resume now works end to end:

- **Capture** the full vCPU state the moment the vCPU thread pauses — REGS,
  SREGS, FPU, XSAVE, XCRS, MSRs (TSC, TSC_DEADLINE, syscall/segment bases,
  sysenter, misc-enable, EFER…), LAPIC, MP_STATE, VCPU_EVENTS. The vCPU thread
  does the capture itself while stopped (all the `KVM_GET_*` ioctls are in its
  seccomp allow-list) and hands it to the controller via a shared cell.
- **Serialize** it into the snapshot blob (kvm-bindings gained the `serde`
  feature). One owned `StateBlob` type is now used for both save and restore so
  the two halves can't drift; restore also recovers volumes/net (previously
  dropped).
- **Reconstruct** a *running* VM on restore: a fresh KVM VM + virtio-blk over the
  restored memory, the vCPU state re-applied (`SET_CPUID2` first, then the rest),
  and the vCPU thread resumed. The kernel/GDT/ACPI are already in the restored
  memory, so we don't rewrite them.

Result on c8i: a real systemd guest snapshotted and **restored back to running
in ~102 ms**, with the restored vCPU thread alive and pausable afterwards (the
definitive liveness proof — a broken restore would have tripped the dead-thread
fallback instead). See `ci/restore-roundtrip.sh`.

## What went wrong (the interesting part)

Booting a real glibc/systemd guest under the per-thread vCPU seccomp filter
detonated a chain of bugs:

1. **`sigaltstack` SIGSYS.** The Rust runtime / panic-unwind path needs
   sigaltstack, mprotect, rt_sigaction/procmask, sched_yield, restart_syscall,
   mremap, getrandom, gettid. Added them (matches Firecracker's vCPU filter).
2. **`openat("/proc/sys/vm/overcommit_memory")` SIGSYS.** glibc's malloc lazily
   reads that file on its first large (mmap-backed) allocation — which happened
   deep in the run loop while buffering a chatty guest's serial output. Fix:
   **warm up the allocator before installing seccomp** so glibc caches the read
   while `openat` is still allowed. (Also bounded the serial buffer — it was an
   unbounded leak.)
3. **`pause()` hung forever.** A seccomp SIGSYS kills the vCPU thread *without
   unwinding*, so the "exited" flag never got set and pause()/snapshot()/stop()
   spun forever. Added a drop-guard (sets exited on panic-unwind) and a
   zero-signal liveness probe in pause() so it can never spin on a dead thread.
4. **Mutex-poison cascade.** Joining a signal-killed thread panics in the std
   thread lifecycle; that panic poisoned the controller mutex and turned every
   later request into a `PoisonError` panic. Joins are now isolated with
   `catch_unwind`, and the controller locks via a poison-recovering helper.

Along the way: `pause()`/`resume()` used to only flip the state enum without
actually stopping the vCPU (a "paused" VM kept burning CPU) — now fixed.

## What I learned

- **glibc + a tight seccomp filter = warm up the allocator first.** Firecracker
  sidesteps this with musl; we use glibc, so the pre-seccomp warmup is essential.
  (Stored as a repo memory.)
- **A vCPU thread can die without unwinding.** Every "wait for the thread"
  path needs a liveness escape hatch, and every `join()` in a `Drop` needs
  `catch_unwind` (a panic in Drop during an unwind aborts the process).
- **Overcommit is already ours for free:** guest RAM is `mmap(MAP_NORESERVE)`
  demand-paged, and idle vCPUs block in `KVM_RUN` at ~0% CPU (in-kernel IRQCHIP),
  so the host scheduler time-slices unpinned vCPU threads. cgroup enforcement is
  an orchestrator concern.
- **Nested-virt perf gates lie.** Cold boot on c8i nested KVM is seconds, not the
  <125 ms bare-metal target, so the boot/creation-rate gates are now
  `VMM_PERF_STRICT`-only; snapshot (~46 ms) and restore (~46 ms) still gate
  unconditionally.
