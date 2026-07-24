# Tarit VMM

Tarit VMM is a minimal rust-vmm based microVM monitor for x86_64 Linux/KVM. It boots one microVM per `vmm` process, exposes a Unix domain socket control protocol, and can be used directly, under the Tarit orchestrator (`taritd`), or under any orchestrator that speaks the protocol in `proto/`.

The VMM provides direct kernel boot, virtio-mmio block and net devices, full
Linux userspace boot, snapshots, suspend and resume, guest exec and PTY, and OCI
image conversion. The release kernel is an uncompressed ELF `vmlinux`. The
loader also accepts user-supplied `bzImage` kernels. The VMM is not an HTTP
service. Its control API is length-prefixed JSON over a Unix domain socket.

It is built from rust-vmm components including kvm-ioctls, vm-memory, linux-loader, virtio-queue, vm-superio, seccompiler, and vmm-sys-util.

## Quickstart

On an x86_64 Linux host with `/dev/kvm`, run these commands from the repository
root:

```sh
make vmm
sudo make guest
sudo install -m 0755 vmm/target/release/vmm /usr/local/bin/vmm
sudo vmm run --kernel guest-assets/vmlinux --rootfs guest-assets/rootfs.ext4 --mem 512 --vcpus 1 --full-boot \
  --cmdline "root=/dev/vda console=ttyS0 reboot=k panic=1 nokaslr"
# API mode, one socket per microVM process:
sudo vmm serve --socket /tmp/vmm.sock
```

## Capabilities

- Direct x86_64 Linux/KVM boot with MMIO-only virtio devices and no PCI.
- One-shot foreground boot with `vmm run`, including `--full-boot` for normal userspace boot.
- Single-VM UDS API server with `vmm serve`.
- Snapshot, diff snapshot, restore, suspend, and resume.
- Restore with optional CoW overlays for clone-style workflows.
- Guest exec and interactive PTY through the guest agent and vsock path.
- Host TAP backed virtio-net plus live egress rule updates.
- Jailer, cgroup, network namespace, uid, gid, and cpuset confinement flags.
- OCI image pull and ext4 conversion with optional guest agent injection.

## Control model

- One `vmm` process owns at most one microVM. Commands and API requests do not take `--id` or VM id fields.
- `vmm run` boots a VM in the foreground from CLI flags.
- `vmm serve --socket <uds>` starts a single-VM API server. The caller can use `vmm create` or send a framed `create` request, then control that VM with `exec`, `attach_pty`, `snapshot`, `pause`, `suspend`, `resume`, `status`, `update_egress`, and `stop` requests.
- A higher-level orchestrator owns VM identity, placement, socket paths, tap creation, cgroups, jails, auth, scheduling, and multi-VM lifecycle.
- `taritd` is one orchestrator for this protocol. It is not required to run Tarit VMM.

## Documentation

- [Standalone operation](docs/STANDALONE.md): run microVMs directly with no orchestrator.
- [Bring your own orchestrator](docs/INTEGRATION.md): wire protocol, JSON operations, PTY stream framing, and a Rust client using `tarit-proto`.
- [Build and API reference](docs/BUILD-AND-API.md): build commands, complete CLI reference, API reference, and architecture notes.

## Current limits

- Running guests requires x86_64 Linux with KVM. macOS is useful for development and non-KVM checks only.
- aarch64 guest boot is not implemented.
- virtio-balloon is not implemented.
- The API is local UDS framing, not REST or HTTP.
