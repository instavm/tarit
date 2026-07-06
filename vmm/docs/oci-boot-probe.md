# OCI virtio-blk activation probe — c8i nested virt

Three probes, one verdict:

| probe | full_boot | irqfd | status_writes | notify_count | final_status | elapsed |
|---|---|---|---|---|---|---|
| A: baseline (no virtio) | false | n/a | n/a | n/a | n/a | 50 ms (101 HLTs) |
| B: virtio, no IRQCHIP | false | no | 0 | 0 | 0x0 | 50 ms (101 HLTs) |
| C: virtio, IRQCHIP+PIT | true | yes | 0 | 0 | 0x0 (no bits) | 20023 ms (0 PIO, 0 HLT) |

**Conditions.** 128 MiB guest, in-tree `guest/bzImage` (Linux 5.10.230 with CONFIG_VIRTIO_BLK=y, CONFIG_VIRTIO_MMIO=y, CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES=y confirmed by `strings`), virtio-blk at 0xd0000000 IRQ 5, cmdline includes `virtio_mmio.device=4K@0xd0000000:5`. ioeventfd always registered; irqfd only with IRQCHIP.

**Verdict C — IRQCHIP path hangs the vCPU, not MMIO coalescing.** With `full_boot=true` (in-kernel IRQCHIP + PIT) the vCPU produces zero PIO/HLT/MMIO exits over the entire window — the kernel never reaches serial init, so the virtio driver never gets a chance to enumerate the device. With `full_boot=false` (no IRQCHIP) the kernel boots fine to HLT but has no IRQ source to schedule virtio probe. 

This means OCI boot-to-login is blocked by **our IRQCHIP+PIT integration in `KvmVm::new_with_options`**, not by L0 MMIO coalescing on nested virt. Likely missing pieces: per-vCPU `set_lapic` call to seed the BSP LAPIC, MADT/ACPI tables consistent with the IRQCHIP topology, or `setup_cpuid` exposing `apic`/`x2apic` bits. **Action.** Update remaining_work.md: the bare-metal caveat is wrong; the real fix is local. Add LAPIC init + revisit cpuid template.
