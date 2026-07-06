# Tarit

**The fastest hypervisor and sandbox cloud for AI agents and RL environments.**

Tarit is a microVM platform for secure, fast, ephemeral
sandboxes, built for AI agent workloads. It boots a real hardware-virtualized VM
in milliseconds, runs a task inside it, and tears it down, giving each sandbox
kernel-level isolation instead of a shared-kernel container boundary.

It has two parts, developed together in this monorepo:

- **`vmm/`** - the Tarit VMM, a minimal [rust-vmm](https://github.com/rust-vmm)
  based microVM monitor (the hypervisor layer). One process runs one microVM.
  Usable on its own or under any orchestrator.
- **`orch/`** - `taritd`, a multi-node orchestrator and PaaS control plane that
  launches and manages microVMs across a fleet, with placement, warm pools,
  networking, snapshots, SSH/PTY access, per-key usage stats, and an audit trail.

They talk over a Unix-domain-socket protocol whose types live in one shared,
dependency-light crate, **`proto/`** (`tarit-proto`). That crate is the wire
contract, so you can drive the VMM from `taritd` or from your own control plane
without hand-copying types.

## Why microVMs

- **Real isolation.** Each sandbox is a KVM guest with its own kernel, not a
  namespaced process. A compromised or runaway workload cannot see the host or
  its neighbors.
- **Fast and cheap.** Minimal device model (MMIO virtio only, no PCI, no BIOS),
  demand-paged guest RAM, and snapshot/restore for sub-second starts.
- **Ephemeral by design.** Create, run, discard. Snapshots and copy-on-write
  overlays make many identical sandboxes cheap to spin up.
- **Built for agents.** vsock-based exec and interactive PTY, per-key usage
  metering and audit, and an orchestrator tuned for bursty create/exec/destroy.

## The VMM vs Firecracker

Tarit column from bare-metal validation; Firecracker column from its published
docs.

| | Tarit, bare metal (p50) | Firecracker |
|---|---|---|
| Ready to exec from snapshot | 83 ms (`node -v` result) | no published number |
| Snapshot restore to running VM | 2.9 ms | no published number |
| Warm-pool VM handout | 12.3 ms | n/a |
| Exec round trip in a running VM | 0.6 ms | n/a, no exec agent |
| Full snapshot, 256 MiB | 60 ms, guest keeps running | pause required |
| Live snapshot of a running guest | yes | no |
| Suspend that releases guest RAM | yes | no |
| PTY over the API | yes | serial console only |
| OCI image boot | built in | no |
| Egress filtering | per-VM allowlist + rate limits | rate limits only |

## Architecture at a glance

```
            HTTP API + CLI + SSH gateway
                        |
                   taritd (orch)          one multi-node control plane
                   /      |     \          placement, warm pool, fleet
                  /       |      \         usage + audit -> PostgreSQL
        vmm serve   vmm serve   vmm serve  one process per microVM
           |            |           |
        microVM      microVM     microVM   KVM guest, own kernel
```

`taritd` and any third-party orchestrator speak the same `tarit-proto` protocol
to `vmm serve`: one length-prefixed JSON request, one response, over a per-VM
Unix socket. See `vmm/docs/INTEGRATION.md` for bring-your-own-orchestrator.

## Quickstart

The quickstart is layered. Layer 1 gets your code running in a microVM. Layer 2
adds snapshots, suspend, and restore. Layer 3 runs a managed fleet with the
orchestrator. Take only the layer you need.

You need a Linux host with KVM (`/dev/kvm`) and a Rust toolchain. Running
microVMs needs root (or membership in the `kvm` group), so the commands use
`sudo`.

### Layer 1: run code in a microVM

```sh
git clone https://github.com/instavm/tarit && cd tarit
sudo make install      # build + install vmm, taritd, and the guest agent
sudo make guest        # one-time: build a guest kernel + pull an Ubuntu rootfs
```

`make guest` does the slow work once (kernel build + OCI pull) and writes
`guest-assets/vmlinux` and `guest-assets/rootfs.ext4`, so starting a VM afterwards
is instant. Boot one, run a command in it, tear it down:

```sh
sudo vmm serve --socket /tmp/vm.sock &
sudo vmm --socket /tmp/vm.sock create --kernel guest-assets/vmlinux --rootfs guest-assets/rootfs.ext4
sleep 12                                             # let the guest boot and dial the agent
sudo vmm --socket /tmp/vm.sock exec "uname -a"
sudo vmm --socket /tmp/vm.sock stop
```

Only want the hypervisor? `sudo make install-vmm` installs just `vmm`.

### Layer 2: snapshot, suspend, restore

Drive the same socket to capture and move VM state. A full snapshot writes memory
plus device state; `--diff` writes only dirty pages. Suspend releases resident
guest RAM; resume brings it back. Restore boots a fresh VMM from a snapshot.

```sh
sudo vmm --socket /tmp/vm.sock snapshot              # full snapshot, prints the .snap path
sudo vmm --socket /tmp/vm.sock snapshot --diff       # incremental (dirty pages only)
sudo vmm --socket /tmp/vm.sock suspend               # release resident guest RAM
sudo vmm --socket /tmp/vm.sock resume
sudo vmm restore --snapshot /path/to.snap            # restore into a new VMM process
```

Tarit also does **live snapshots**: a memory-consistent snapshot of a running
guest with no downtime, so a busy VM can be checkpointed or forked. See
[vmm/docs/STANDALONE.md](vmm/docs/STANDALONE.md) for the full device, egress,
jailer, and PTY surface.

### Layer 3: run a fleet with the orchestrator

`taritd` manages many microVMs across one or more nodes over an HTTP API, with
placement, warm pools, per-key usage accounting, and an SSH/PTY gateway. Single
node:

```sh
cd orch
TARIT_API_KEY=$(openssl rand -hex 24) \
TARIT_VMM_BIN=$(command -v vmm) \
TARIT_KERNEL=$PWD/../guest-assets/vmlinux TARIT_ROOTFS=$PWD/../guest-assets/rootfs.ext4 \
  taritd serve
```

Then create and drive VMs over HTTP, and scale to a multi-node cluster:

- Orchestrator quickstart and config: [orch/docs/QUICKSTART.md](orch/docs/QUICKSTART.md),
  [orch/docs/CONFIGURATION.md](orch/docs/CONFIGURATION.md)
- Architecture and HTTP API: [orch/docs/ARCHITECTURE.md](orch/docs/ARCHITECTURE.md),
  [orch/docs/API.md](orch/docs/API.md)
- Multi-node cluster, failover, autoscaling: [orch/docs/RESILIENCE.md](orch/docs/RESILIENCE.md),
  [orch/docs/AUTOSCALING.md](orch/docs/AUTOSCALING.md)
- Usage stats and audit trail: [orch/docs/USAGE-AND-AUDIT.md](orch/docs/USAGE-AND-AUDIT.md)
- Drive the VMM from your own control plane: [vmm/docs/INTEGRATION.md](vmm/docs/INTEGRATION.md)

## Repository layout

```
vmm/     the Tarit VMM (microVM monitor) - its own cargo workspace
orch/    taritd, the orchestrator and PaaS control plane - its own cargo workspace
proto/   tarit-proto, the shared UDS wire protocol crate (KVM-free)
```

`vmm/` and `orch/` are independent cargo workspaces so the VMM can be built,
tested, and consumed on its own. Both depend on `proto/` for the wire types.

## Documentation

- Tarit VMM (standalone): [vmm/README.md](vmm/README.md),
  [vmm/docs/STANDALONE.md](vmm/docs/STANDALONE.md)
- Bring your own orchestrator: [vmm/docs/INTEGRATION.md](vmm/docs/INTEGRATION.md)
- VMM build + CLI + UDS API: [vmm/docs/BUILD-AND-API.md](vmm/docs/BUILD-AND-API.md)
- Orchestrator quickstart + config: [orch/docs/QUICKSTART.md](orch/docs/QUICKSTART.md),
  [orch/docs/CONFIGURATION.md](orch/docs/CONFIGURATION.md)
- Orchestrator architecture + API: [orch/docs/ARCHITECTURE.md](orch/docs/ARCHITECTURE.md),
  [orch/docs/API.md](orch/docs/API.md)
- Resilience and scale (tested scenarios): [orch/docs/RESILIENCE.md](orch/docs/RESILIENCE.md)
- Usage stats and audit trail: [orch/docs/USAGE-AND-AUDIT.md](orch/docs/USAGE-AND-AUDIT.md)
- Contributing: [CONTRIBUTING.md](CONTRIBUTING.md)

## Platform support

- Host: x86_64 Linux with KVM. Development also works on macOS for building and
  cross-checking; running microVMs needs KVM.
- Not yet implemented: aarch64 guests, virtio-balloon.

## Self-hosting

Self-hosting has been tested on AWS and GCP. Azure support is coming soon.

## License

Tarit is licensed under **AGPL-3.0-or-later**. See [LICENSE](LICENSE).
