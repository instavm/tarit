# Tarit

Tarit runs untrusted code in KVM microVMs. It is built for AI agents and
other workloads that create many short-lived sandboxes, run commands in
them, checkpoint and restore them, and fork them.

It has two parts, developed together in this repository:

- **`vmm/`**: the Tarit VMM, a microVM monitor built on
  [rust-vmm](https://github.com/rust-vmm) crates. One process runs one VM.
  It works on its own or under any orchestrator.
- **`orch/`**: `taritd`, which runs VMs on one host or across a fleet:
  placement, warm pools, networking, snapshots, SSH/PTY access, usage
  records and an audit trail.

They talk over a Unix socket protocol whose types live in **`proto/`**
(`tarit-proto`), so another control plane can drive the VMM with the same
types.

## What it does

- **Isolation.** Each sandbox is a KVM guest with its own kernel. The VMM
  runs each thread under its own seccomp profile. With taritd's VM jail
  configured, each VMM also gets its own uid, PID namespace, network
  namespace and cgroup. See [SECURITY.md](SECURITY.md) for what this does
  and does not cover.
- **Fast creates.** taritd keeps warm pools of VMs restored from a golden
  snapshot, one for each registered image and for each VM shape in use, so
  most creates return in a few milliseconds. An image can carry a command
  its golden runs first, so the first command in a sandbox finds its
  runtime already in memory.
- **Checkpoints.** A snapshot captures memory, device state and the
  writable disk. A checkpoint after the first pauses the guest only for
  memory changed since the previous one, and still restores on its own.
  Live snapshots keep the guest running until a short final stop.
- **Fork.** A running VM can be forked. Children share its memory until
  they write to it, and on a reflink filesystem its disk too.
- **Images.** Any OCI or Docker image can become a VM root filesystem,
  without root (`taritd image build`, `vmm pull`).
- **Network.** With taritd's networking on, each VM gets its own TAP and
  address, and outbound traffic is denied unless allowed.
- **Access.** Commands run over vsock; there are also PTYs, an SSH
  gateway and HTTP port shares.

## Performance evidence

Latency depends on the host kernel, CPU, storage, guest image, workload and
commit. `vmm/ci/perf-gates.sh` measures VMM lifecycle latency,
`test/bench.sh` measures end-to-end time to first command, `test/compare/`
runs the same workloads against Firecracker, and `test/soak/api_soak.py`
holds many sandboxes busy and checks what they hold. A published result
should name the commit, the host and guest configuration, the artifacts,
the iteration count and percentile method, and include the raw report.
Numbers without those are not release evidence.

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
make build             # compile vmm, taritd, and the guest agent as your regular user
sudo make install      # copy the prebuilt artifacts into system locations
sudo make guest        # one-time: fetch a verified guest kernel + pull an Ubuntu rootfs
```

`make guest` downloads the pinned, checksum-verified guest kernel and pulls the
OCI rootfs once. If the release artifact is unavailable, it builds the same
kernel from checksum-pinned source. It writes `guest-assets/vmlinux` and
`guest-assets/rootfs.ext4`, so later starts do not repeat image conversion.
The [guest kernel documentation](vmm/guest/README.md) lists the pins, build,
and release checks. Boot one, run a command in it, tear it down:

```sh
sudo vmm serve --socket /tmp/vm.sock &
sudo vmm --socket /tmp/vm.sock create --kernel guest-assets/vmlinux --rootfs guest-assets/rootfs.ext4
sleep 12                                             # let the guest boot and dial the agent
sudo vmm --socket /tmp/vm.sock exec "uname -a"
sudo vmm --socket /tmp/vm.sock stop
```

For a kernel-only install, run `sudo vmm kernel install`. Interactive
`vmm run` and `vmm create` commands offer to install that pinned kernel when
`--kernel` is omitted. Non-interactive commands require `--kernel` or a prior
install.

Only want the hypervisor? Run `make vmm` as your regular user, then
`sudo make install-vmm` to install just `vmm`. Install targets only copy prebuilt
artifacts and fail with build guidance if an artifact is missing; they never run
Cargo or the guest compiler under sudo.

### Layer 2: snapshot, suspend, restore

Drive the same socket to capture and move VM state. A full snapshot writes memory
plus device state; `--diff` writes only dirty pages. Suspend releases resident
guest RAM; resume brings it back. Restore boots a fresh VMM from a snapshot.
Snapshots include the architectural and host-advertised KVM paravirtual MSR
state used by the guest. Restore validates that state against the destination
before changing a vCPU and rejects incompatible hosts without a partial
restore. Suspend parks the block, network, and vsock workers after stopping the
vCPUs; resume waits for those workers to leave the parked state before vCPUs
run again.

```sh
sudo vmm --socket /tmp/vm.sock snapshot              # full snapshot, prints the .snap path
sudo vmm --socket /tmp/vm.sock snapshot --diff       # incremental (dirty pages only)
sudo vmm --socket /tmp/vm.sock snapshot --live       # live: guest keeps running
sudo vmm --socket /tmp/vm.sock suspend               # release resident guest RAM
sudo vmm --socket /tmp/vm.sock resume
sudo vmm --socket /tmp/vm2.sock restore --snapshot /path/to.snap  # restore in a new VMM process that serves its API
```

Tarit also does **live snapshots** (`snapshot --live`): pre-copy rounds run
while the guest executes, then a final stop copies only the residual dirty
pages and captures vCPU + device state. The final stop targets sub-millisecond
downtime (500µs by default); dirty pages are tracked from both KVM's dirty log
(vCPU writes) and a software tracker covering virtio DMA, and device I/O
threads are quiesced for the final stop so the image stays coherent. One
caveat: a guest that dirties memory faster than it can be copied never
converges, so after a bounded number of rounds (or timeout) the snapshot
forces the final stop and downtime grows with the residual — the reported
`downtime` in the result is always the real measured blackout. Orchestrated
full snapshots similarly use a disk-consistency pause. They use
FICLONE/reflink when available; the correctness-preserving sparse-copy
fallback is not a low-latency path, so production latency gates require
reflink-capable storage. See
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
- Not yet implemented: aarch64 guests.

## Self-hosting

Self-hosting has been tested on AWS and GCP. Azure support is coming soon.

## License

Tarit is licensed under **AGPL-3.0-or-later**. See [LICENSE](LICENSE).
