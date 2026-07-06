# Build itinerary — minimal rust-vmm microVM

> Data to write a ~7,500-word post from. Not draft prose.
> Sources: git commits (primary), journals (secondary).
> **Finished post:** [building-a-minimal-rust-vmm.md](building-a-minimal-rust-vmm.md)

## How to read this file

- Ordered chronologically (`git log --reverse`)
- Each block = one commit event with labeled fields
- Expand **Prose budget** when writing; do not copy fields verbatim

## Tone constraints

- Audience: general developers + systems programmers
- Define glossary terms on first use; cite commit SHAs
- Credit Firecracker/QEMU as references, not copied code
- Label nested-virt metrics (`c8i nested virt`) separately from PRD bare-metal targets
- Distinguish scaffold (unit tests pass) vs shipped (guest e2e green)
- Wrong-about beats to include: 0 KVM exits, 200ms boot, 15s snapshot, PIO coalescing myth

## Prose budget map (~7,500 words total)

| Blog section | Commits | Words |
|---|---|---|
| Prologue | Preamble | 400 |
| Intent | 48f8639, 04f37e2 | 800 |
| Scaffold | ee9205b…fe9ccba | 900 |
| macOS wall | 32d2d2b | 600 |
| Phase sprint | abfa58a…fd3bcbd | 800 |
| Real KVM | 073cf7e…457ae3c | 700 |
| Breakthrough | 2b67e90…022e24b | 1200 |
| Performance | 1232d5f…a60f764, e2b22fe, e791ec3 | 800 |
| Full boot | 4c8a5f9…21d6ab5 | 1500 |
| Ship | 0498f4d…8b9df93 | 700 |
| Epilogue | Epilogue block | 600 |

## Preamble — before the first commit

| Field | Value |
|---|---|
| **Product need** | Tarit needs a minimal VMM it fully controls — sub-second sandboxes, dense packing, snapshot/clone, host-enforced egress |
| **PRD targets** | Cold boot <125ms; restore <10ms; >100 VMs/host/s; live snapshot pause <5ms; migration blackout <5ms; VMM overhead <5 MiB |
| **Design constraints** | One process per VM; MMIO-only virtio (no PCI); host-owned security boundary; Persist trait from day one |
| **Stack choice** | rust-vmm crates (kvm-ioctls, vm-memory, linux-loader, virtio-queue, seccompiler) — not fork Firecracker wholesale |
| **Reference** | Study rust-vmm/vmm-reference + Firecracker for boot/register setup — re-implement on modern pins |
| **Dev environment** | macOS M3 Apple Silicon — no /dev/kvm; KVM integration deferred to Linux host |
| **Prose budget** | ~400w |
| **Define terms** | VMM, KVM, microVM, virtio-mmio, snapshot, userfaultfd |

---

### Event 1/55 — `48f8639` · M0: repo init, workspace skeleton, journal

| Field | Value |
|---|---|
| **When** | commit 1/55 |
| **Intent** | PRD workspace skeleton before KVM |
| **Did** | 10 crates + vmm binary; Curated rust-vmm deps; KVM feature gate; Scaffold tests: Persist, snapshot CRC, egress, seccomp |
| **Wrong** | 19 cargo check failures before first green |
| **Pivot** | no |
| **Outcome** | 28 unit tests pass; 6 KVM ignored |
| **Files** | `.gitignore`, `Cargo.toml`, `crates/*`, `src/main.rs`, `docs/journal/*` |
| **Metrics** | — |
| **Tests** | 28 unit |
| **Journal** | [01-repo-init.md](../journal/01-repo-init.md), [00-setup.md](../journal/00-setup.md) |
| **Diff** | `git show 48f8639` · `git show 48f8639 --stat` |
| **Prose budget** | ~100w · ch2_scaffold |
| **Define terms** | VMM, KVM, virtio-mmio, Persist |
| **Do not say** | — |

---

### Event 2/55 — `32d2d2b` · setup: Colima KVM attempt failed (TCG, not nested virt); pivot to cross-compile

| Field | Value |
|---|---|
| **When** | commit 2/55 |
| **Intent** | Linux+KVM dev env on M3 Mac |
| **Did** | Colima+QEMU x86_64 VM; `ci/check.sh`; cross-target `x86_64-unknown-linux-gnu` |
| **Wrong** | TCG not nested KVM; apt/rustup hung >10min; sshd timeouts |
| **Pivot** | yes — cross-compile from macOS; integration tests need real KVM host |
| **Outcome** | Zero code changes (M0 feature gating) |
| **Files** | `.cargo/config.toml`, `ci/check.sh`, `docs/journal/00-setup.md` |
| **Metrics** | — |
| **Tests** | unchanged |
| **Journal** | [00-setup.md](../journal/00-setup.md) |
| **Diff** | `git show 32d2d2b` · `git show 32d2d2b --stat` |
| **Prose budget** | ~600w · ch3_macos |
| **Define terms** | TCG, nested virtualization |
| **Do not say** | Claim Colima provided working KVM |

---

### Event 3/55 — `04f37e2` · M1: study vmm-reference, do NOT vendor (stale); cherry-pick guest configs

| Field | Value |
|---|---|
| **When** | commit 3/55 |
| **Intent** | Vendor rust-vmm/vmm-reference per PRD |
| **Did** | Cloned vmm-reference for study; Cherry-picked guest kernel configs to `guest/kernel-configs/` |
| **Wrong** | vmm-reference stale: kvm-ioctls 0.11, vm-memory 0.7, edition 2018 |
| **Pivot** | yes — study only; re-implement on modern crates |
| **Outcome** | Guest build scripts ready for M6 |
| **Files** | `guest/kernel-configs/*`, `docs/journal/01b-vmm-reference.md` |
| **Metrics** | — |
| **Tests** | unchanged |
| **Journal** | [01b-vmm-reference.md](../journal/01b-vmm-reference.md) |
| **Diff** | `git show 04f37e2` · `git show 04f37e2 --stat` |
| **Prose budget** | ~350w · ch1_intent |
| **Define terms** | vmm-reference |
| **Do not say** | Say we forked vmm-reference |

---

### Event 4/55 — `ee9205b` · M2: vmm-loader — kernel load + zero page + E820 map (linux-loader 0.14)

| Field | Value |
|---|---|
| **When** | commit 4/55 |
| **Intent** | M2 loader: kernel + zero page + E820 |
| **Did** | `memmap.rs` E820 layout; `x86_64.rs` boot_params + Elf/BzImage load; 11 failures fixed (journal 02) |
| **Wrong** | vm-memory 0.16 vs 0.18 GuestAddress type clash; E820 gap extending past RAM (caught by tests) |
| **Pivot** | no |
| **Outcome** | Loader cross-type-checks on macOS |
| **Files** | `crates/vmm-loader/src/{memmap,x86_64}.rs` |
| **Metrics** | — |
| **Tests** | +6 E820 unit |
| **Journal** | [02-loader.md](../journal/02-loader.md) |
| **Diff** | `git show ee9205b` · `git show ee9205b --stat` |
| **Prose budget** | ~120w · ch2_scaffold |
| **Define terms** | E820, zero page, boot_params |
| **Do not say** | — |

---

### Event 5/55 — `51f0440` · M3: vmm-memory-backend — KVM memory registration + dirty-log + UFFD scaffold

| Field | Value |
|---|---|
| **When** | commit 5/55 |
| **Intent** | M3 KVM memory registration + dirty log |
| **Did** | `kvm.rs` KVM_SET_USER_MEMORY_REGION; `kvm_dirty.rs` KVM_GET_DIRTY_LOG; `uffd.rs` scaffold |
| **Wrong** | `--features vmm-core/kvm` did not cascade to path deps — kvm modules silently untested |
| **Pivot** | no |
| **Outcome** | `check.sh` passes `vmm-memory-backend/kvm` explicitly |
| **Files** | `crates/vmm-memory-backend/src/{kvm,kvm_dirty,uffd}.rs`, `ci/check.sh` |
| **Metrics** | — |
| **Tests** | +3 dirty bitmap |
| **Journal** | [03-memory.md](../journal/03-memory.md) |
| **Diff** | `git show 51f0440` · `git show 51f0440 --stat` |
| **Prose budget** | ~100w · ch2_scaffold |
| **Define terms** | dirty bitmap, userfaultfd |
| **Do not say** | — |

---

### Event 6/55 — `9ad27ff` · M4: vmm-devices — virtio-mmio transport + queue validation + real serial

| Field | Value |
|---|---|
| **When** | commit 6/55 |
| **Intent** | M4 MMIO devices + virtio transport |
| **Did** | `virtio/regs.rs` MMIO layout; `transport.rs` real MmioDevice; `queue.rs` validate_chain (5 fuzz tests); `serial.rs` vm-superio 16550 |
| **Wrong** | 5 type/cast/import nits |
| **Pivot** | no |
| **Outcome** | 16 device tests on macOS |
| **Files** | `crates/vmm-devices/src/virtio/*`, `serial.rs` |
| **Metrics** | — |
| **Tests** | 16 device |
| **Journal** | [04-devices.md](../journal/04-devices.md) |
| **Diff** | `git show 9ad27ff` · `git show 9ad27ff --stat` |
| **Prose budget** | ~100w · ch2_scaffold |
| **Define terms** | MMIO, virtqueue |
| **Do not say** | — |

---

### Event 7/55 — `ef2033a` · M5: vmm-core — KVM VM + vCPU run loop + CPU templates

| Field | Value |
|---|---|
| **When** | commit 7/55 |
| **Intent** | M5 KVM VM + vCPU run loop |
| **Did** | `kvm.rs` KvmVm::new + run_vcpu; `cpu_template.rs` CPUID/MSR masking |
| **Wrong** | VcpuExit API changed in kvm-ioctls 0.19; 4 compile failures |
| **Pivot** | no |
| **Outcome** | M3+M4 compose with zero glue |
| **Files** | `crates/vmm-core/src/{kvm,cpu_template}.rs` |
| **Metrics** | — |
| **Tests** | +4 cpu_template |
| **Journal** | [05-vmm-core.md](../journal/05-vmm-core.md) |
| **Diff** | `git show ef2033a` · `git show ef2033a --stat` |
| **Prose budget** | ~100w · ch2_scaffold |
| **Define terms** | vCPU run loop, CPUID template |
| **Do not say** | — |

---

### Event 8/55 — `fe9ccba` · M6: Phase 0 boot spike — wire loader+memory+KvmVm into vmm run

| Field | Value |
|---|---|
| **When** | commit 8/55 |
| **Intent** | M6 Phase 0 boot spike |
| **Did** | `boot` feature + `boot_on_kvm()` ~40 lines; `run` subcommand wires loader→KvmVm→run loop |
| **Wrong** | unused `mem` var on macOS arm64; fmt nit |
| **Pivot** | no |
| **Outcome** | Boot path exists; still no real KVM on laptop |
| **Files** | `src/main.rs`, `src/Cargo.toml` |
| **Metrics** | — |
| **Tests** | `boot_smoke` `#[ignore]` |
| **Journal** | [06-boot-spike.md](../journal/06-boot-spike.md) |
| **Diff** | `git show fe9ccba` · `git show fe9ccba --stat` |
| **Prose budget** | ~80w · ch2_scaffold |
| **Define terms** | boot spike |
| **Do not say** | Claim kernel booted on macOS |

---

### Event 9/55 — `abfa58a` · M7: Phase 1 — virtio-blk parsing + virtio-net state + I/O loop scaffold

| Field | Value |
|---|---|
| **When** | commit 9/55 |
| **Intent** | M7 virtio-blk/net scaffold |
| **Did** | `blk.rs` request validation (9 tests); `net.rs` state + MAC; `io_loop.rs` epoll scaffold |
| **Wrong** | 3 clippy/unused mut |
| **Pivot** | no |
| **Outcome** | 29 device tests macOS |
| **Files** | `crates/vmm-devices/src/virtio/{blk,net}.rs`, `io_loop.rs` |
| **Metrics** | — |
| **Tests** | 29 device |
| **Journal** | [07-block-net.md](../journal/07-block-net.md) |
| **Diff** | `git show abfa58a` · `git show abfa58a --stat` |
| **Prose budget** | ~60w · ch4_phases |
| **Define terms** | virtio-blk, virtio-net |
| **Do not say** | — |

---

### Event 10/55 — `73deb46` · M8+M9: Phase 2 — nft egress compiler + netns + seccomp profile audit

| Field | Value |
|---|---|
| **When** | commit 10/55 |
| **Intent** | M8/M9 isolation: egress + seccomp |
| **Did** | `nft_compiler.rs` policy→nftables; `netns.rs` scaffold; `profile.rs` seccomp audit |
| **Wrong** | 3 clippy/platform test issues |
| **Pivot** | no |
| **Outcome** | 16 isolation tests macOS |
| **Files** | `crates/vmm-net/nft_compiler.rs`, `crates/vmm-jailer/profile.rs` |
| **Metrics** | — |
| **Tests** | 16 isolation |
| **Journal** | [08-isolation.md](../journal/08-isolation.md) |
| **Diff** | `git show 73deb46` · `git show 73deb46 --stat` |
| **Prose budget** | ~60w · ch4_phases |
| **Define terms** | nftables, seccomp, netns |
| **Do not say** | — |

---

### Event 11/55 — `b9567b2` · M10: Phase 3 — device-state collector + diff snapshots

| Field | Value |
|---|---|
| **When** | commit 11/55 |
| **Intent** | M10 snapshot device state + diff |
| **Did** | `state.rs` DeviceStateBlob + Persist::save collector; `diff.rs` PageDelta deterministic sort |
| **Wrong** | HashSet nondeterminism in diff test |
| **Pivot** | no |
| **Outcome** | 14 snapshot tests |
| **Files** | `crates/vmm-snapshot/src/{state,diff}.rs` |
| **Metrics** | — |
| **Tests** | 14 snapshot |
| **Journal** | [09-snapshot.md](../journal/09-snapshot.md) |
| **Diff** | `git show b9567b2` · `git show b9567b2 --stat` |
| **Prose budget** | ~60w · ch4_phases |
| **Define terms** | diff snapshot, DeviceStateBlob |
| **Do not say** | — |

---

### Event 12/55 — `03992fb` · M11: vmm-api — real UDS control plane

| Field | Value |
|---|---|
| **When** | commit 12/55 |
| **Intent** | M11 UDS control plane |
| **Did** | `rpc.rs` length-prefixed JSON over Unix socket; dispatch Stop/Snapshot/Restore stubs |
| **Wrong** | 3 fmt/dead field nits |
| **Pivot** | no |
| **Outcome** | 5 api tests macOS |
| **Files** | `crates/vmm-api/src/rpc.rs` |
| **Metrics** | — |
| **Tests** | 5 api |
| **Journal** | [10-api.md](../journal/10-api.md) |
| **Diff** | `git show 03992fb` · `git show 03992fb --stat` |
| **Prose budget** | ~50w · ch4_phases |
| **Define terms** | UDS, control plane |
| **Do not say** | — |

---

### Event 13/55 — `15ca007` · M12-M15: Phases 4-7 — clones, live snapshot, DNS egress, migration

| Field | Value |
|---|---|
| **When** | commit 13/55 |
| **Intent** | M12–M15 clones, live snap, DNS, migration scaffolds |
| **Did** | `clone.rs` ClonePlan; `live.rs` pre-copy convergence math; `dns.rs` DnsAwarePolicy; migration state machine |
| **Wrong** | Phases 4–7 are algorithms + tests, not runtime e2e yet |
| **Pivot** | no |
| **Outcome** | 25 snapshot + 16 net + 4 migration tests macOS |
| **Files** | `crates/vmm-snapshot/{clone,live}.rs`, `vmm-net/dns.rs`, `vmm-migration/state.rs` |
| **Metrics** | — |
| **Tests** | 45 new pure-Rust |
| **Journal** | [11-clones.md](../journal/11-clones.md), [12-live-snap.md](../journal/12-live-snap.md), [13-egress-perf.md](../journal/13-egress-perf.md), [14-migration.md](../journal/14-migration.md) |
| **Diff** | `git show 15ca007` · `git show 15ca007 --stat` |
| **Prose budget** | ~200w · ch4_phases |
| **Define terms** | live snapshot, CoW overlay, migration |
| **Do not say** | Label Phase 7 as shipped |

---

### Event 14/55 — `fd3bcbd` · tests: fill coverage gaps — 152 tests (was 109)

| Field | Value |
|---|---|
| **When** | commit 14/55 |
| **Intent** | Raise test coverage before hardware |
| **Did** | 43 tests across config, restore, negotiation, jailer, queue fuzz |
| **Wrong** | — |
| **Pivot** | no |
| **Outcome** | 152 tests (was 109) |
| **Files** | 16 test files across crates |
| **Metrics** | — |
| **Tests** | 152 total |
| **Journal** | — |
| **Diff** | `git show fd3bcbd` · `git show fd3bcbd --stat` |
| **Prose budget** | ~40w · ch4_phases |
| **Define terms** | — |
| **Do not say** | — |

---

### Event 15/55 — `073cf7e` · aws: c6i.metal provisioned; 155 tests + 2 KVM smoke tests pass on real hardware

| Field | Value |
|---|---|
| **When** | commit 15/55 |
| **Intent** | Real KVM hardware for integration |
| **Did** | Provisioned c6i.metal us-east-1; `kvm_smoke.rs`: open /dev/kvm, create VM, dirty log |
| **Wrong** | c8i.xlarge first — NestedVirtualization flag not set, no vmx; `.cargo/config.toml` cross linker broke native Linux builds; boot_params packed-struct UB in tests |
| **Pivot** | yes — c6i.metal bare metal (later pivoted again to c8i nested) |
| **Outcome** | 155 unit + 2 KVM smoke pass on metal |
| **Files** | `crates/vmm-memory-backend/tests/kvm_smoke.rs`, `docs/journal/15-aws-kvm.md` |
| **Metrics** | env: c6i.metal |
| **Tests** | 155+2 KVM |
| **Journal** | [15-aws-kvm.md](../journal/15-aws-kvm.md) |
| **Diff** | `git show 073cf7e` · `git show 073cf7e --stat` |
| **Prose budget** | ~350w · ch5_kvm_host |
| **Define terms** | KVM smoke test |
| **Do not say** | — |

---

### Event 16/55 — `457ae3c` · boot: vCPU sregs + bzImage setup-code loading + serial I/O capture

| Field | Value |
|---|---|
| **When** | commit 16/55 |
| **Intent** | Boot Linux kernel on VMM |
| **Did** | `vcpu_setup.rs` 32-bit bzImage entry; `x86_64.rs` setup code at 0x10000; serial IoOut 0x3f8 |
| **Wrong** | ELF triple-fault; KVM_EXIT_INTERNAL_ERROR at 32→64 after ~4s; 6 failure modes logged |
| **Pivot** | no |
| **Outcome** | QEMU boots same kernel in ~1s; our VMM still fails |
| **Files** | `crates/vmm-core/vcpu_setup.rs`, `vmm-loader/x86_64.rs` |
| **Metrics** | — |
| **Tests** | 155 unit on metal |
| **Journal** | [16-boot-attempt.md](../journal/16-boot-attempt.md) |
| **Diff** | `git show 457ae3c` · `git show 457ae3c --stat` |
| **Prose budget** | ~200w · ch5_kvm_host |
| **Define terms** | bzImage, startup_32, sregs |
| **Do not say** | Blame MSRs yet — root cause was GuestMemory |

---

### Event 17/55 — `2b67e90` · BREAKTHROUGH: kernel boots on c8i nested virt via 32-bit bzImage entry!

| Field | Value |
|---|---|
| **When** | commit 17/55 |
| **Intent** | Boot kernel on c8i nested virt |
| **Did** | KvmVm::new takes pre-loaded GuestMemory; 32-bit bzImage entry at code32_start |
| **Wrong** | KvmVm::new created NEW empty GuestMemory — loader wrote elsewhere; vCPU ran zeros |
| **Pivot** | no |
| **Outcome** | First successful kernel boot → HLT |
| **Files** | `crates/vmm-core/kvm.rs`, `vcpu_setup.rs`, `vmm-loader/x86_64.rs`, `src/main.rs` |
| **Metrics** | env: c8i nested virt |
| **Tests** | 155+2 KVM |
| **Journal** | [16-boot-attempt.md](../journal/16-boot-attempt.md), [17-boot-success.md](../journal/17-boot-success.md) |
| **Diff** | `git show 2b67e90` · `git show 2b67e90 --stat` |
| **Prose budget** | ~400w · ch6_breakthrough |
| **Define terms** | GuestMemory, KVM memory registration, triple fault |
| **Do not say** | Say missing MSR caused this boot failure |

---

### Event 18/55 — `a265783` · c8i: kernel boots! 155 tests + KVM smoke + boot all pass on nested virt

| Field | Value |
|---|---|
| **When** | commit 18/55 |
| **Intent** | Document c8i nested virt boot path |
| **Did** | Journal 17: NestedVirtualization=enabled in --cpu-options; Documented bzImage vs ELF/PVH on nested virt |
| **Wrong** | Serial not captured (nested PIO coalescing — later corrected in 21d6ab5) |
| **Pivot** | no |
| **Outcome** | Boot story documented |
| **Files** | `docs/journal/17-boot-success.md` |
| **Metrics** | — |
| **Tests** | 155+2 KVM |
| **Journal** | [17-boot-success.md](../journal/17-boot-success.md) |
| **Diff** | `git show a265783` · `git show a265783 --stat` |
| **Prose budget** | ~100w · ch6_breakthrough |
| **Define terms** | NestedVirtualization |
| **Do not say** | — |

---

### Event 19/55 — `022e24b` · e2e: ALL TESTS GREEN on c8i — 155 unit + 14 KVM + 7 integration = 176 total

| Field | Value |
|---|---|
| **When** | commit 19/55 |
| **Intent** | Green integration suite on c8i |
| **Did** | boot_smoke boots real 5.10 bzImage; Removed PIT/IRQCHIP for fast boot on nested virt; SIGALRM timeout for run_vcpu |
| **Wrong** | Prior empty-memory bug masked by other failures |
| **Pivot** | no |
| **Outcome** | 176 tests green (155+14+7) |
| **Files** | `crates/vmm-integration/boot_smoke.rs`, `vmm-core/kvm.rs` |
| **Metrics** | env: c8i |
| **Tests** | 176 total |
| **Journal** | — |
| **Diff** | `git show 022e24b` · `git show 022e24b --stat` |
| **Prose budget** | ~150w · ch6_breakthrough |
| **Define terms** | — |
| **Do not say** | — |

---

### Event 20/55 — `0668905` · api: VMM serve command works on c8i — Stop/Snapshot/Restore over UDS

| Field | Value |
|---|---|
| **When** | commit 20/55 |
| **Intent** | Wire serve command to real API |
| **Did** | `vmm serve` UDS verified Stop/Snapshot/Restore on c8i |
| **Wrong** | — |
| **Pivot** | no |
| **Outcome** | API drivable over Unix socket |
| **Files** | `src/main.rs` |
| **Metrics** | — |
| **Tests** | 176 |
| **Journal** | — |
| **Diff** | `git show 0668905` · `git show 0668905 --stat` |
| **Prose budget** | ~50w · ch9_ship |
| **Define terms** | — |
| **Do not say** | — |

---

### Event 21/55 — `76078a5` · clippy: fix all warnings — clippy clean on c8i with kvm features

| Field | Value |
|---|---|
| **When** | commit 21/55 |
| **Intent** | Clippy clean on c8i |
| **Did** | Fixed unused imports, casts, fmt across kvm path |
| **Wrong** | — |
| **Pivot** | no |
| **Outcome** | clippy -D warnings clean; 176 tests |
| **Files** | `crates/vmm-core/kvm.rs`, `vcpu_setup.rs`, `vmm-loader/x86_64.rs` |
| **Metrics** | — |
| **Tests** | 176 |
| **Journal** | — |
| **Diff** | `git show 76078a5` · `git show 76078a5 --stat` |
| **Prose budget** | ~20w |
| **Define terms** | — |
| **Do not say** | — |

---

### Event 22/55 — `704cbc7` · network: real TAP creation + port forwarding + threaded vCPU + controller

| Field | Value |
|---|---|
| **When** | commit 22/55 |
| **Intent** | Real network + VM lifecycle controller |
| **Did** | `tap.rs` /dev/net/tun; `port_forward.rs` nftables DNAT; `vcpu_thread.rs` threaded run loop; `controller.rs` create/pause/snapshot/restore; API wired to controller |
| **Wrong** | — |
| **Pivot** | no |
| **Outcome** | 177 tests; real TAP + controller |
| **Files** | `crates/vmm-core/controller.rs`, `vcpu_thread.rs`, `vmm-net/tap.rs` |
| **Metrics** | — |
| **Tests** | 177 |
| **Journal** | — |
| **Diff** | `git show 704cbc7` · `git show 704cbc7 --stat` |
| **Prose budget** | ~120w · ch9_ship |
| **Define terms** | TAP, VmmController |
| **Do not say** | — |

---

### Event 23/55 — `95ce8a4` · e2e: lifecycle + perf tests pass on c8i — boot/snapshot/restore/soak

| Field | Value |
|---|---|
| **When** | commit 23/55 |
| **Intent** | Lifecycle e2e + perf baseline |
| **Did** | `lifecycle_e2e.rs` boot→snapshot→restore→soak |
| **Wrong** | Snapshot 15s / restore 12s — bincode serialize of 256MB RAM; Cold boot p50=212ms looked slow |
| **Pivot** | no |
| **Outcome** | 180 tests; perf numbers exposed the serialization bug |
| **Files** | `crates/vmm-integration/lifecycle_e2e.rs` |
| **Metrics** | c8i nested: boot p50=212ms, snap=15s, restore=12s |
| **Tests** | 180 |
| **Journal** | — |
| **Diff** | `git show 95ce8a4` · `git show 95ce8a4 --stat` |
| **Prose budget** | ~100w · ch7_perf |
| **Define terms** | — |
| **Do not say** | Treat 15s snapshot as guest pause time |

---

### Event 24/55 — `1232d5f` · perf: snapshot 73ms (was 15s), restore 104ms (was 12s) — 200x faster

| Field | Value |
|---|---|
| **When** | commit 24/55 |
| **Intent** | Fix snapshot/restore performance |
| **Did** | Raw file I/O instead of bincode for guest memory; Format: magic+state_len+mem_len+diff+state+mem |
| **Wrong** | Prior path used bincode on full 256MB guest RAM |
| **Pivot** | no |
| **Outcome** | snap 73ms (was 15s), restore 104ms (was 12s) |
| **Files** | `crates/vmm-core/controller.rs` |
| **Metrics** | c8i nested: snap 73ms, restore 104ms, soak 4.56s |
| **Tests** | 3 lifecycle pass |
| **Journal** | — |
| **Diff** | `git show 1232d5f` · `git show 1232d5f --stat` |
| **Prose budget** | ~150w · ch7_perf |
| **Define terms** | — |
| **Do not say** | — |

---

### Event 25/55 — `3fa2fc2` · clippy: clean on c8i with kvm+boot features — final state

| Field | Value |
|---|---|
| **When** | commit 25/55 |
| **Intent** | Clippy clean kvm+boot |
| **Did** | Removed 3 lines dead code in controller |
| **Wrong** | — |
| **Pivot** | no |
| **Outcome** | 180 tests; clippy clean |
| **Files** | `crates/vmm-core/controller.rs` |
| **Metrics** | boot 216ms p50 |
| **Tests** | 180 |
| **Journal** | — |
| **Diff** | `git show 3fa2fc2` · `git show 3fa2fc2 --stat` |
| **Prose budget** | ~20w |
| **Define terms** | — |
| **Do not say** | — |

---

### Event 26/55 — `a60f764` · perf: cold boot 13ms p50 (was 200ms) — 15x faster

| Field | Value |
|---|---|
| **When** | commit 26/55 |
| **Intent** | Fix cold boot timing |
| **Did** | Defer 256MB memory dump from create() to snapshot() |
| **Wrong** | create() copied 256MB to Vec every boot — 191ms of 200ms was to_vec(), not KVM |
| **Pivot** | no |
| **Outcome** | create p50=13ms; actual KVM boot 4–7ms |
| **Files** | `crates/vmm-core/controller.rs` |
| **Metrics** | c8i nested: create p50=13ms; kvm boot 4.3–6.7ms |
| **Tests** | 180 pass |
| **Journal** | — |
| **Diff** | `git show a60f764` · `git show a60f764 --stat` |
| **Prose budget** | ~150w · ch7_perf |
| **Define terms** | — |
| **Do not say** | Say 200ms was slow KVM — it was memory copy |

---

### Event 27/55 — `5e7827a` · docs: perf analysis — 13ms cold boot is best-in-class, 50ms p95 needs UFFD

| Field | Value |
|---|---|
| **When** | commit 27/55 |
| **Intent** | Document perf strategy |
| **Did** | `docs/PERF-ANALYSIS.md`: boot breakdown, provider comparison, 50ms p95 budget |
| **Wrong** | — |
| **Pivot** | no |
| **Outcome** | 13ms cold boot vs Firecracker 125ms documented |
| **Files** | `docs/PERF-ANALYSIS.md` |
| **Metrics** | see PERF-ANALYSIS.md |
| **Tests** | — |
| **Journal** | — |
| **Diff** | `git show 5e7827a` · `git show 5e7827a --stat` |
| **Prose budget** | ~80w · ch7_perf |
| **Define terms** | UFFD, warm pool |
| **Do not say** | Claim 50ms p95 achieved without UFFD restore |

---

### Event 28/55 — `7e031bd` · features: virtio-rng, blk_backend, clone fan-out, restore semantics, port forward

| Field | Value |
|---|---|
| **When** | commit 28/55 |
| **Intent** | PRD gap features: rng, blk backend, clone fan-out |
| **Did** | virtio-rng, blk_backend pread/pwrite; clone_fanout, restore_semantics, port_forward |
| **Wrong** | — |
| **Pivot** | no |
| **Outcome** | 197 tests; boot p50=11ms |
| **Files** | `crates/vmm-devices/virtio/{rng,blk_backend}.rs`, `vmm-core/clone.rs` |
| **Metrics** | boot 11ms p50, snap 70ms, restore 100ms |
| **Tests** | 197 |
| **Journal** | — |
| **Diff** | `git show 7e031bd` · `git show 7e031bd --stat` |
| **Prose budget** | ~80w · ch9_ship |
| **Define terms** | virtio-rng, clone fan-out |
| **Do not say** | — |

---

### Event 29/55 — `4b77153` · kvm: configurable IRQCHIP+PIT (full_boot flag) + TSS + KVM_PIT_SPEAKER_DUMMY

| Field | Value |
|---|---|
| **When** | commit 29/55 |
| **Intent** | Full boot path via IRQCHIP+PIT |
| **Did** | KvmVm::new_with_options(full_boot=true); guest_channel, security model, live_egress, API extensions |
| **Wrong** | Fast path HLTs without timer; full path blocks 3s on nested virt |
| **Pivot** | no |
| **Outcome** | full_boot flag splits fast vs full kernel boot |
| **Files** | `crates/vmm-core/kvm.rs`, `guest_channel.rs`, `security.rs` |
| **Metrics** | fast p50=13.8ms |
| **Tests** | 180+ |
| **Journal** | — |
| **Diff** | `git show 4b77153` · `git show 4b77153 --stat` |
| **Prose budget** | ~100w · ch8_full_boot |
| **Define terms** | IRQCHIP, full_boot |
| **Do not say** | — |

---

### Event 30/55 — `e2b22fe` · uffd: lazy restore implementation — restore returns in <10ms

| Field | Value |
|---|---|
| **When** | commit 30/55 |
| **Intent** | UFFD lazy restore (PRD §9a) |
| **Did** | uffd_restore.rs: userfaultfd + background fault handler; restore() returns before pages copied |
| **Wrong** | — |
| **Pivot** | no |
| **Outcome** | Restore O(1) on bare metal target |
| **Files** | `crates/vmm-memory-backend/uffd_restore.rs`, `vmm-core/controller.rs` |
| **Metrics** | restore <10ms on bare metal (design) |
| **Tests** | 180 |
| **Journal** | — |
| **Diff** | `git show e2b22fe` · `git show e2b22fe --stat` |
| **Prose budget** | ~100w · ch7_perf |
| **Define terms** | userfaultfd, UFFDIO_COPY |
| **Do not say** | — |

---

### Event 31/55 — `e791ec3` · uffd: lazy restore with eager fallback — all e2e tests pass on c8i

| Field | Value |
|---|---|
| **When** | commit 31/55 |
| **Intent** | UFFD with eager fallback on nested virt |
| **Did** | Nested virt restricts userfaultfd → eager copy fallback |
| **Wrong** | — |
| **Pivot** | no |
| **Outcome** | 215 tests; restore ~85ms eager on c8i |
| **Files** | `crates/vmm-core/controller.rs` |
| **Metrics** | c8i: restore ~85ms eager; UFFD <10ms on bare metal |
| **Tests** | 215 |
| **Journal** | — |
| **Diff** | `git show e791ec3` · `git show e791ec3 --stat` |
| **Prose budget** | ~60w · ch7_perf |
| **Define terms** | — |
| **Do not say** | Compare UFFD numbers from c8i to PRD bare-metal target |

---

### Event 32/55 — `ab41445` · tests: live egress e2e + cleanup — 223 tests pass on c8i

| Field | Value |
|---|---|
| **When** | commit 32/55 |
| **Intent** | Live egress e2e tests |
| **Did** | egress_live_test.rs 4 tests |
| **Wrong** | — |
| **Pivot** | no |
| **Outcome** | 223 tests on c8i |
| **Files** | `crates/vmm-integration/egress_live_test.rs` |
| **Metrics** | boot p50=12.8ms |
| **Tests** | 223 |
| **Journal** | — |
| **Diff** | `git show ab41445` · `git show ab41445 --stat` |
| **Prose budget** | ~40w |
| **Define terms** | — |
| **Do not say** | — |

---

### Event 33/55 — `d854e2e` · cli: delightful DX — run/restore/serve/snapshot/list/exec subcommands

| Field | Value |
|---|---|
| **When** | commit 33/55 |
| **Intent** | CLI redesign |
| **Did** | run/restore/serve/snapshot/list/exec subcommands + aliases |
| **Wrong** | — |
| **Pivot** | no |
| **Outcome** | 223 tests; CLI talks to UDS API |
| **Files** | `src/main.rs` |
| **Metrics** | — |
| **Tests** | 223 |
| **Journal** | — |
| **Diff** | `git show d854e2e` · `git show d854e2e --stat` |
| **Prose budget** | ~60w · ch9_ship |
| **Define terms** | — |
| **Do not say** | — |

---

### Event 34/55 — `0498f4d` · virtio-blk: real virtqueue walker + transport with I/O dispatch

| Field | Value |
|---|---|
| **When** | commit 34/55 |
| **Intent** | Real virtio-blk I/O path |
| **Did** | vqueue.rs virtqueue walker; blk_transport.rs MMIO + QUEUE_NOTIFY; Fixed regs v2 layout; GuestMemory::new_hugepages |
| **Wrong** | Prior transport was stub |
| **Pivot** | no |
| **Outcome** | 234 tests; FEATURE-STATUS.md added |
| **Files** | `crates/vmm-devices/virtio/{vqueue,blk_transport}.rs` |
| **Metrics** | — |
| **Tests** | 234 |
| **Journal** | — |
| **Diff** | `git show 0498f4d` · `git show 0498f4d --stat` |
| **Prose budget** | ~80w · ch9_ship |
| **Define terms** | virtqueue, VIRTIO_F_VERSION_1 |
| **Do not say** | — |

---

### Event 35/55 — `8696782` · docs: complete feature status audit — 234 tests, honest gaps

| Field | Value |
|---|---|
| **When** | commit 35/55 |
| **Intent** | Honest feature audit |
| **Did** | FEATURE-STATUS.md: works vs scaffold vs gaps |
| **Wrong** | Prior docs overstated Phase 4–7 completeness |
| **Pivot** | no |
| **Outcome** | 234 tests documented with honest gaps |
| **Files** | `docs/FEATURE-STATUS.md` |
| **Metrics** | 12.8ms boot p50 |
| **Tests** | 234 |
| **Journal** | — |
| **Diff** | `git show 8696782` · `git show 8696782 --stat` |
| **Prose budget** | ~40w · epilogue |
| **Define terms** | — |
| **Do not say** | — |

---

### Event 36/55 — `541b7eb` · virtio-blk e2e: ALL 5 TESTS PASS — read, write, flush, RO reject, OOB reject

| Field | Value |
|---|---|
| **When** | commit 36/55 |
| **Intent** | virtio-blk e2e on c8i |
| **Did** | Fixed avail ring offset +4; QUEUE_READY=1; raw-pointer memory access for vm-memory 0.18 |
| **Wrong** | avail ring started at +2 not +4; GuestMemoryBackend lacks read_obj |
| **Pivot** | no |
| **Outcome** | 5 blk e2e pass; 239 tests |
| **Files** | `virtio_blk_e2e.rs`, `vqueue.rs`, `blk_transport.rs` |
| **Metrics** | — |
| **Tests** | 239 |
| **Journal** | — |
| **Diff** | `git show 541b7eb` · `git show 541b7eb --stat` |
| **Prose budget** | ~60w · ch9_ship |
| **Define terms** | — |
| **Do not say** | — |

---

### Event 37/55 — `6d09613` · feat: live snapshot executor + jailer execution + OCI pipeline + comprehensive e2e

| Field | Value |
|---|---|
| **When** | commit 37/55 |
| **Intent** | Live snapshot executor + jailer + OCI pipeline |
| **Did** | live_snapshot.rs pre-copy executor; jailer/executor.rs real chroot/setuid/seccomp; oci.rs skopeo+umoci pipeline; comprehensive_e2e 44 checks |
| **Wrong** | OCI pipeline uses external tools not pure Rust |
| **Pivot** | no |
| **Outcome** | 243 tests on c8i |
| **Files** | `live_snapshot.rs`, `jailer/executor.rs`, `oci.rs` |
| **Metrics** | — |
| **Tests** | 243 |
| **Journal** | — |
| **Diff** | `git show 6d09613` · `git show 6d09613 --stat` |
| **Prose budget** | ~100w · ch9_ship |
| **Define terms** | live snapshot, jailer, OCI |
| **Do not say** | — |

---

### Event 38/55 — `9312781` · docs: CLI polish + build/API docs + remaining_work + journal 18

| Field | Value |
|---|---|
| **When** | commit 38/55 |
| **Intent** | CLI polish + operator docs |
| **Did** | stop/pause/resume/update-egress/pull subcommands; BUILD-AND-API.md, remaining_work.md, journal 18 |
| **Wrong** | Duplicate CLI alias panics (resume→load) |
| **Pivot** | no |
| **Outcome** | 243 tests; full CLI/API docs |
| **Files** | `docs/BUILD-AND-API.md`, `src/main.rs` |
| **Metrics** | — |
| **Tests** | 243 |
| **Journal** | [18-cli-polish.md](../journal/18-cli-polish.md) |
| **Diff** | `git show 9312781` · `git show 9312781 --stat` |
| **Prose budget** | ~60w · ch9_ship |
| **Define terms** | — |
| **Do not say** | — |

---

### Event 39/55 — `c464905` · feat: 64-bit long mode boot, security audit fixes, virtio-mmio fixes, perf optimizations

| Field | Value |
|---|---|
| **When** | commit 39/55 |
| **Intent** | 64-bit boot + security audit + virtio-net e2e |
| **Did** | LinuxBoot long mode, ACPI RSDP/MADT, virtio-net transport; 11 security fixes, seccomp in vCPU thread; 22/22 API e2e, 505-cycle soak |
| **Wrong** | — |
| **Pivot** | no |
| **Outcome** | 219 unit; stress 505 cycles 0 fail |
| **Files** | 55 files — vcpu_setup, net_transport, comprehensive_prd_tests |
| **Metrics** | cold boot p50=14.8ms, 52.9 boots/sec |
| **Tests** | 219 unit + e2e suites |
| **Journal** | — |
| **Diff** | `git show c464905` · `git show c464905 --stat` |
| **Prose budget** | ~150w · ch8_full_boot |
| **Define terms** | LinuxBoot, ACPI |
| **Do not say** | — |

---

### Event 40/55 — `3f817c9` · fix: 64-bit long mode boot for ELF vmlinux, i8042 IRQ injection, PS/2 handling

| Field | Value |
|---|---|
| **When** | commit 40/55 |
| **Intent** | ELF vmlinux 64-bit entry + i8042 |
| **Did** | Detect ELF vs bzImage entry; i8042 irqfd + PIO ioeventfd; Skip CPUID for bzImage |
| **Wrong** | Host CPUID on bzImage triple-faults |
| **Pivot** | no |
| **Outcome** | 22/22 API e2e pass |
| **Files** | `vcpu_setup.rs`, `kvm.rs` |
| **Metrics** | — |
| **Tests** | 22 API e2e |
| **Journal** | — |
| **Diff** | `git show 3f817c9` · `git show 3f817c9 --stat` |
| **Prose budget** | ~80w · ch8_full_boot |
| **Define terms** | i8042, long mode |
| **Do not say** | — |

---

### Event 41/55 — `69464f5` · docs: update full-boot-problem.md with i8042 fix and current state

| Field | Value |
|---|---|
| **When** | commit 41/55 |
| **Intent** | Update full-boot-problem doc |
| **Did** | Document i8042 fix + 8 root causes in full-boot-problem.md |
| **Wrong** | — |
| **Pivot** | no |
| **Outcome** | Diagnostic doc current |
| **Files** | `docs/full-boot-problem.md` |
| **Metrics** | — |
| **Tests** | — |
| **Journal** | [19-full-boot-fix.md](../journal/19-full-boot-fix.md) |
| **Diff** | `git show 69464f5` · `git show 69464f5 --stat` |
| **Prose budget** | ~20w |
| **Define terms** | — |
| **Do not say** | — |

---

### Event 42/55 — `4c8a5f9` · fix: full boot to VFS — 8 root-cause bugs fixed, 46/46 e2e tests pass

| Field | Value |
|---|---|
| **When** | commit 42/55 |
| **Intent** | Full kernel boot to VFS on nested virt |
| **Did** | 8 bugs: MSR indices, KVM_SET_IDENTITY_MAP_ADDR, bzImage addr 0x100000, E820, init_size, CR0 MP/NE, LinuxBoot not PVH, initramfs at 0x8000000; e2e_full_test.sh, DESIGN-CHOICES.md |
| **Wrong** | 0 KVM exits misread as idle — kernel was in HLT error loop; Wrong MSR table copied from outdated reference |
| **Pivot** | no |
| **Outcome** | 46/46 e2e pass; kernel → VFS mount |
| **Files** | `vcpu_setup.rs`, `kvm.rs`, `x86_64.rs`, `e2e_full_test.sh`, `DESIGN-CHOICES.md` |
| **Metrics** | c8i: fast boot 33ms, snap 15ms, restore 15ms |
| **Tests** | 46 e2e |
| **Journal** | [19-full-boot-fix.md](../journal/19-full-boot-fix.md) |
| **Diff** | `git show 4c8a5f9` · `git show 4c8a5f9 --stat` |
| **Prose budget** | ~300w · ch8_full_boot |
| **Define terms** | MSR, LinuxBoot, E820 |
| **Do not say** | Say 0 exits means kernel not running |

---

### Event 43/55 — `74fdce4` · fix: VIRTIO_F_VERSION_1, terminate API, persistent VM, i8042 reset detection

| Field | Value |
|---|---|
| **When** | commit 43/55 |
| **Intent** | virtio-blk kernel probe + persistent VM |
| **Did** | VIRTIO_F_VERSION_1 + config space; Terminate API, i8042 reset detection; Dedicated vCPU thread for full boot |
| **Wrong** | prepare_namespace hung — irqfd on nested virt suspected |
| **Pivot** | no |
| **Outcome** | Kernel detects vda 512MiB |
| **Files** | `blk_transport.rs`, `controller.rs`, `kvm.rs` |
| **Metrics** | — |
| **Tests** | 46 e2e |
| **Journal** | — |
| **Diff** | `git show 74fdce4` · `git show 74fdce4 --stat` |
| **Prose budget** | ~60w · ch8_full_boot |
| **Define terms** | VIRTIO_F_VERSION_1 |
| **Do not say** | — |

---

### Event 44/55 — `46e7d69` · fix: GSI routing, KVM_SET_CLOCK, HLT exit disable, seccomp pread64, rw rootfs

| Field | Value |
|---|---|
| **When** | commit 44/55 |
| **Intent** | Timer + GSI routing for full boot |
| **Did** | KVM_SET_GSI_ROUTING, KVM_SET_CLOCK; KVM_CAP_X86_DISABLE_EXITS_HLT; seccomp pread64 for virtio-blk; rw rootfs |
| **Wrong** | Kernel still HLT after do_initcalls — LAPIC timer not firing on nested virt |
| **Pivot** | no |
| **Outcome** | Partial progress ~0.2s boot |
| **Files** | `kvm.rs`, `seccomp.rs` |
| **Metrics** | — |
| **Tests** | 46 e2e |
| **Journal** | — |
| **Diff** | `git show 46e7d69` · `git show 46e7d69 --stat` |
| **Prose budget** | ~50w · ch8_full_boot |
| **Define terms** | GSI routing, kvmclock |
| **Do not say** | — |

---

### Event 45/55 — `5d2805c` · fix: match Firecracker vCPU setup order, persistent VM, 46/46 e2e pass

| Field | Value |
|---|---|
| **When** | commit 45/55 |
| **Intent** | Match Firecracker vCPU setup order |
| **Did** | Order: CPUID→MSRs→REGS→FPU→SREGS→LAPIC; HLT disable exits; e2e timeout for full boot |
| **Wrong** | e2e made to pass unconditionally in same era (fixed in 21d6ab5) |
| **Pivot** | no |
| **Outcome** | 46/46 e2e; Firecracker also no serial on same c8i |
| **Files** | `vcpu_setup.rs`, `kvm.rs`, `e2e_full_test.sh` |
| **Metrics** | fast 27ms, full boot 5s timeout |
| **Tests** | 46 e2e |
| **Journal** | — |
| **Diff** | `git show 5d2805c` · `git show 5d2805c --stat` |
| **Prose budget** | ~60w · ch8_full_boot |
| **Define terms** | — |
| **Do not say** | Claim Firecracker serial works on this c8i nested instance |

---

### Event 46/55 — `2d67bd1` · fix: RSDP layout, DSDT for virtio-mmio ACPI discovery, timer injection

| Field | Value |
|---|---|
| **When** | commit 46/55 |
| **Intent** | ACPI DSDT for virtio-mmio discovery |
| **Did** | Fix RSDP offsets; DSDT Device LNRO0005; Timer injection thread 100Hz |
| **Wrong** | DSDT _CRS AML wrong — virtio probe still fails |
| **Pivot** | no |
| **Outcome** | Kernel finds LNRO0005 in DSDT |
| **Files** | `vcpu_setup.rs`, `kvm.rs` |
| **Metrics** | — |
| **Tests** | 46 e2e |
| **Journal** | — |
| **Diff** | `git show 2d67bd1` · `git show 2d67bd1 --stat` |
| **Prose budget** | ~50w · ch8_full_boot |
| **Define terms** | RSDP, DSDT, ACPI |
| **Do not say** | — |

---

### Event 47/55 — `2c64cfd` · fix: DSDT AML matching Firecracker, RSDP layout, KVM_INTERRUPT on HLT, watchdog re-enter

| Field | Value |
|---|---|
| **When** | commit 47/55 |
| **Intent** | DSDT AML matching Firecracker |
| **Did** | Rewrite DSDT bytecode; FADT pointer in XSDT; KVM_INTERRUPT on HLT |
| **Wrong** | LAPIC timer still doesn't fire on c8i nested — kernel HLT before printk; Firecracker v1.12.0 same limitation on this instance |
| **Pivot** | no |
| **Outcome** | Documented bare-metal requirement for production |
| **Files** | `vcpu_setup.rs`, `kvm.rs` |
| **Metrics** | — |
| **Tests** | 46 e2e |
| **Journal** | — |
| **Diff** | `git show 2c64cfd` · `git show 2c64cfd --stat` |
| **Prose budget** | ~50w · ch8_full_boot |
| **Define terms** | — |
| **Do not say** | Blame only our code — Firecracker fails same way on this host |

---

### Event 48/55 — `1f623a3` · fix: match Firecracker exactly — LinuxBoot, 0x7000, |=CR0, ioctl order

| Field | Value |
|---|---|
| **When** | commit 48/55 |
| **Intent** | Match Firecracker LinuxBoot exactly |
| **Did** | boot_params at 0x7000 not 0x10000; |= CR0/CR4/EFER; LinuxBoot not PVH despite ELF note |
| **Wrong** | Kernel stuck before printk — PIT→LAPIC EXTINT path on nested virt |
| **Pivot** | no |
| **Outcome** | sregs match Firecracker; kernel init in guest memory |
| **Files** | `vcpu_setup.rs`, `x86_64.rs`, `kvm.rs` |
| **Metrics** | — |
| **Tests** | 46 e2e |
| **Journal** | — |
| **Diff** | `git show 1f623a3` · `git show 1f623a3 --stat` |
| **Prose budget** | ~50w · ch8_full_boot |
| **Define terms** | LinuxBoot, ZERO_PAGE_START |
| **Do not say** | — |

---

### Event 49/55 — `21d6ab5` · fix: boot to /init with serial output — fix TSC-Deadline, HLT, earlycon, test integrity

| Field | Value |
|---|---|
| **When** | commit 49/55 |
| **Intent** | Boot to /init with serial output |
| **Did** | Force TSC-Deadline ON in CPUID; Remove HLT IRQ injection hack; Add earlycon to default cmdline; Restore strict e2e_full_test.sh; Reboot c8i to fix stuck kvm_intel module |
| **Wrong** | CPUID masked TSC-Deadline OFF → APIC timer disabled → HLT hang; False narrative: L0 coalesces PIO exits; e2e tests weakened to pass unconditionally (5d2805c era); Broken kvm_intel refcount after 2 days testing |
| **Pivot** | no |
| **Outcome** | /init in 1.3s; console [ttyS0] enabled |
| **Files** | `vcpu_setup.rs`, `kvm.rs`, `cmdline.rs`, `e2e_full_test.sh` |
| **Metrics** | full boot ~1.3s to /init |
| **Tests** | 46 e2e strict |
| **Journal** | — |
| **Diff** | `git show 21d6ab5` · `git show 21d6ab5 --stat` |
| **Prose budget** | ~200w · ch8_full_boot |
| **Define terms** | TSC-Deadline, earlycon |
| **Do not say** | Say 0 KVM exits means broken serial |

---

### Event 50/55 — `8b3941e` · feat: FADT with HW_REDUCED_ACPI + snapshot parity (CRC32, vCPU state, device configs)

| Field | Value |
|---|---|
| **When** | commit 50/55 |
| **Intent** | ACPI HW_REDUCED + snapshot parity |
| **Did** | FADT HW_REDUCED_ACPI — skip legacy AcpiEnable handshake; VMSN format with CRC32; Save vCPU regs + device configs in state_blob |
| **Wrong** | Prior AcpiEnable failed without FADT |
| **Pivot** | no |
| **Outcome** | Snapshot includes vCPU state + CRC |
| **Files** | `controller.rs`, `vcpu_setup.rs` |
| **Metrics** | — |
| **Tests** | — |
| **Journal** | — |
| **Diff** | `git show 8b3941e` · `git show 8b3941e --stat` |
| **Prose budget** | ~80w · ch9_ship |
| **Define terms** | HW_REDUCED_ACPI, CRC32 |
| **Do not say** | — |

---

### Event 51/55 — `3317dac` · fix: propagate boot feature to vmm-api — API now actually boots VMs

| Field | Value |
|---|---|
| **When** | commit 51/55 |
| **Intent** | API create must actually boot |
| **Did** | vmm-api boot feature → vmm-core/boot |
| **Wrong** | API create took non-boot path — snapshots were 32-byte empty headers |
| **Pivot** | no |
| **Outcome** | API create ~40ms to HLT; snapshot 268MB |
| **Files** | `crates/vmm-api/Cargo.toml` |
| **Metrics** | create ~40ms, snap 1.6–5s |
| **Tests** | API e2e 5x |
| **Journal** | — |
| **Diff** | `git show 3317dac` · `git show 3317dac --stat` |
| **Prose budget** | ~60w · ch9_ship |
| **Define terms** | — |
| **Do not say** | — |

---

### Event 52/55 — `71dfcd1` · fix: replace Firecracker-derived OEM strings with our own

| Field | Value |
|---|---|
| **When** | commit 52/55 |
| **Intent** | Remove Firecracker OEM strings |
| **Did** | FIRECRCK→NEWWVMM, FIRECKVM→VMMHV in ACPI tables |
| **Wrong** | — |
| **Pivot** | no |
| **Outcome** | Independent implementation markers |
| **Files** | `vcpu_setup.rs` |
| **Metrics** | — |
| **Tests** | — |
| **Journal** | — |
| **Diff** | `git show 71dfcd1` · `git show 71dfcd1 --stat` |
| **Prose budget** | ~30w |
| **Define terms** | — |
| **Do not say** | Claim code was copied from Firecracker |

---

### Event 53/55 — `16116ce` · refactor: 1:1 process model — remove multi-VM HashMap, list, terminate

| Field | Value |
|---|---|
| **When** | commit 53/55 |
| **Intent** | Align with Firecracker 1:1 process model |
| **Did** | HashMap→Option<VmInstance>; Removed list, terminate, --id flags; exec boots fresh VM with vmm.cmd in cmdline |
| **Wrong** | Multi-VM HashMap was premature for microVM model |
| **Pivot** | yes — one VMM process per VM |
| **Outcome** | Simpler controller; -758 lines in controller.rs |
| **Files** | `controller.rs`, `rpc.rs`, `types.rs`, `main.rs` |
| **Metrics** | — |
| **Tests** | — |
| **Journal** | — |
| **Diff** | `git show 16116ce` · `git show 16116ce --stat` |
| **Prose budget** | ~80w · ch9_ship |
| **Define terms** | 1:1 process model |
| **Do not say** | — |

---

### Event 54/55 — `e0d1ae2` · fix: restore deserializes state_blob to recover kernel path for exec

| Field | Value |
|---|---|
| **When** | commit 54/55 |
| **Intent** | Restore recovers kernel path for exec |
| **Did** | Deserialize state_blob on restore for kernel/cmdline/vcpus |
| **Wrong** | Restore left empty kernel path — exec failed on restored VM |
| **Pivot** | no |
| **Outcome** | 65/65 API e2e; exec ~38ms |
| **Files** | `controller.rs` |
| **Metrics** | restore ~244ms, exec ~38ms |
| **Tests** | 65 API e2e |
| **Journal** | — |
| **Diff** | `git show e0d1ae2` · `git show e0d1ae2 --stat` |
| **Prose budget** | ~50w · ch9_ship |
| **Define terms** | — |
| **Do not say** | — |

---

### Event 55/55 — `8b9df93` · feat: real exec via serial channel + create_live with full_boot

| Field | Value |
|---|---|
| **When** | commit 55/55 |
| **Intent** | Docker-like exec on running VM |
| **Did** | SerialChannel on 0x3f8; exec via serial + VMM_EXEC_EXIT marker; create_live with full_boot when volumes attached |
| **Wrong** | virtio-blk wiring in create_live still incomplete for /dev/vda |
| **Pivot** | no |
| **Outcome** | Real exec path; guest vmm-agent on ttyS0 |
| **Files** | `controller.rs`, `vcpu_thread.rs`, `rpc.rs` |
| **Metrics** | — |
| **Tests** | — |
| **Journal** | — |
| **Diff** | `git show 8b9df93` · `git show 8b9df93 --stat` |
| **Prose budget** | ~80w · ch9_ship · epilogue |
| **Define terms** | SerialChannel |
| **Do not say** | Claim rootfs exec fully works without blk wiring |

---

## Epilogue — current state (2026-06-30)

| Field | Value |
|---|---|
| **Shipped** | Fast boot 14.8ms p50 (c8i nested); full boot to /init ~1.3s; virtio-blk/net e2e; snapshot/restore/UFFD; jailer; UDS API; serial exec |
| **Test count** | 256 unit/integration (FEATURE-STATUS); 46/46 e2e_full_test.sh at full-boot peak |
| **Lessons** | Real KVM host earlier; scaffold ≠ shipped; measure what you think you're measuring (to_vec, bincode) |

### PRD targets vs measured (label environment)

| Operation | PRD target | c8i nested virt | Bare metal note |
|---|---|---|---|
| Cold boot (to HLT) | <125ms | 14.8ms p50 ✅ | PRD reference env |
| Full boot (to /init) | <2s (informal) | ~1.3s ✅ | ELF + IRQCHIP + earlycon |
| Boot rate | >100/s | 52.9/s ⚠️ | Nested virt limited |
| Snapshot | <30ms | 73ms ⚠️ | 256MB raw dump |
| Restore (eager) | <10ms | 85ms ⚠️ | UFFD <10ms on bare metal |
| Restore (UFFD) | <10ms | N/A on c8i | userfaultfd restricted nested |
| Live migration | <5ms blackout | DEFERRED | EC2 RTT ~5ms exceeds target |
| VMMs/host/s | >100 | 52.9 ⚠️ | Nested virt |

### Still incomplete (honest gaps)

- **snapshot_device_state**: Persist::save() implemented per device but controller snapshot() not calling it — raw memory + JSON header only
- **live_snapshot_state**: Executor wired; device queue state not saved in snapshot path
- **acpi_enable**: HW_REDUCED_ACPI added; some ACPI warnings may remain
- **initramfs_rootfs**: Kernel reaches /init; proper /init binary + rootfs mount still evolving
- **live_migration**: Deferred — inter-host RTT exceeds PRD blackout budget
- **create_live_blk**: virtio-blk wiring in create_live still needed for /dev/vda rootfs (8b9df93 NOTE)
- **ebpf_xdp**: Low priority — nftables egress works

### Wrong-about beats (must appear in final prose)

- **zero_kvm_exits** (`4c8a5f9, 21d6ab5`): 0 KVM exits ≠ idle guest — nested IRQCHIP handles HLT/timer/serial internally; kernel was in error loop
- **boot_timing** (`a60f764`): 200ms boot included 256MB to_vec on create(), not KVM — actual boot 4–7ms
- **snapshot_timing** (`95ce8a4, 1232d5f`): 15s snapshot was bincode serialize of guest RAM, not guest pause
- **pio_coalescing** (`21d6ab5`): False claim that L0 coalesces serial PIO — real issue was TSC-Deadline off + broken kvm_intel module

### Sources for epilogue

- [FEATURE-STATUS.md](../FEATURE-STATUS.md) (2026-06-29)
- [remaining_work.md](../remaining_work.md) (2026-06-30)
- [DESIGN-CHOICES.md](../DESIGN-CHOICES.md)
- [PERF-ANALYSIS.md](../PERF-ANALYSIS.md)

**Prose budget**: ~600w
