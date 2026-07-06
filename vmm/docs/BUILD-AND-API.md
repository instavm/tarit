# VMM Build & API/CLI Documentation

## Building the Binary

### Prerequisites

- **Rust 1.95+** (stable)
- **Linux x86_64 host with KVM** (`/dev/kvm`) for running VMs
- For OCI image pulling: `skopeo`, `umoci`, `e2fsprogs` (`sudo apt install skopeo umoci e2fsprogs`)

### Build Commands

```sh
# Development build (debug)
cargo build --workspace

# Production build with KVM boot support
cargo build --release --features boot

# Cross-compile check from macOS (type-check KVM code without running)
CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=true \
  cargo check --workspace --target x86_64-unknown-linux-gnu \
  --features vmm-core/kvm
```

### Feature Flags

| Feature | Purpose |
|---|---|
| `boot` | Enable the `vmm` binary full boot path. This forwards to the KVM-enabled core/API path. |
| `vmm-core/kvm` | Enable KVM ioctl wrappers for workspace cross-checks and core tests. |

### Running Tests

```sh
# Unit tests (no KVM needed, runs on macOS too)
cargo test --workspace

# KVM smoke tests (Linux+KVM only)
sudo cargo test -p vmm-memory-backend --features kvm -- --include-ignored

# E2E integration tests (Linux+KVM + guest/bzImage)
sudo cargo test -p vmm-integration --features kvm -- --include-ignored

# Comprehensive test (44 feature checks)
sudo cargo test -p vmm-integration --features kvm --test comprehensive_e2e -- --include-ignored --nocapture

# Virtio-blk E2E (5 tests: read/write/flush/RO/OOB)
sudo cargo test -p vmm-integration --test virtio_blk_e2e -- --include-ignored

# Clippy + fmt
cargo clippy --workspace --all-targets --features vmm-core/kvm -- -D warnings
cargo fmt --all -- --check
```

---

## CLI Reference

### `vmm run`: Boot a fresh VM

```sh
vmm run --kernel <PATH> [OPTIONS]
```

| Flag | Default | Description |
|---|---|---|
| `--kernel <PATH>` | (required) | Path to bzImage or vmlinux |
| `--cmdline <CMDLINE>` | Firecracker-style default | Kernel command line |
| `--initramfs <PATH>` | none | Path to initramfs image |
| `--mem <MIB>` | 256 | Guest memory size in MiB |
| `--vcpus <N>` | 1 | Number of vCPUs |
| `--rootfs <PATH>` | none | Attach a boot rootfs as `/dev/vda` read-only |
| `--volume <PATH[:ro\|rw]>` | none | Attach a storage volume (repeatable) |
| `--overlay <PATH>` | none | Attach a private CoW overlay for each `--volume` |
| `--net <SPEC>` | none | Attach virtio-net to a pre-created TAP (`tap=<name>[,mac=...]`) |
| `--full-boot` | off | Enable IRQCHIP + PIT (needed to reach userspace init) |
| `--jail <DIR>` | none | Run inside a jail rooted at `DIR` |

**Examples:**
```sh
# Fast boot (kernel HLTs: for benchmarks)
vmm run --kernel guest/bzImage --mem 256

# Full boot with rootfs
vmm run --kernel guest/bzImage --rootfs rootfs.ext4 --full-boot \
  --cmdline "root=/dev/vda console=ttyS0 reboot=k panic=1 nokaslr"

# With initramfs
vmm run --kernel guest/bzImage --initramfs guest/initramfs.cpio.gz --mem 512
```

### `vmm serve`: Start the API server

```sh
vmm serve [--socket <PATH>]
```

| Flag | Default | Description |
|---|---|---|
| `--socket <PATH>` | `/run/vmm.sock` | Unix socket path |
| `--jail <DIR>` | none | Run the served VM path inside a jail |
| `--netns <PATH>` | none | Enter a network namespace when jailed |
| `--cgroup <PATH>` | none | Apply cgroup v2 limits under this cgroup path |
| `--cgroup-memory-max <BYTES>` | none | Set `memory.max` |
| `--cgroup-cpu-max <QUOTA/PERIOD\|MILLICPU>` | none | Set `cpu.max` |
| `--cgroup-pids-max <N>` | none | Set `pids.max` |
| `--cpuset <CPUS>` | none | Set `cpuset.cpus` |

**Example:**
```sh
vmm serve --socket /tmp/vmm.sock
```

### `vmm restore`: Restore from snapshot

```sh
vmm restore --snapshot <PATH>
```

### `vmm snapshot`: Snapshot a running VM (via API)

```sh
vmm snapshot [--diff] [--socket <PATH>]
```

### `vmm status`: Show VM status

```sh
vmm status [--socket <PATH>]
```

### `vmm exec`: Execute a command in a guest

```sh
vmm exec <COMMAND> [--timeout <MS>] [--socket <PATH>]
```

### `vmm attach-pty`: Attach an interactive PTY

```sh
vmm attach-pty [--shell <SHELL>] [--socket <PATH>]
```

### `vmm stop`: Stop a VM

```sh
vmm stop [--socket <PATH>]
```

### `vmm gc`: Remove orphaned VMM scratch files

```sh
vmm gc [--dir /tmp] [--max-age <SECS>]
```

Removes only old VMM scratch names (`vmm-live.snap`,
`.vmm-suspend-<pid>-<ts>.snap`, `vmm-ov-<pid>-<ts>-*.cow`) that are not open by
any process on Linux. User-requested snapshot files and caller-owned overlays
are preserved.

### `vmm pause` / `vmm resume`: Pause/resume a VM

```sh
vmm pause [--socket <PATH>]
vmm resume [--socket <PATH>]
```

### `vmm suspend`: Pause and release resident guest RAM

```sh
vmm suspend [--socket <PATH>]
vmm resume [--socket <PATH>]
```

### `vmm update-egress`: Update egress policy on a live VM

```sh
vmm update-egress --allow <RULE>... [--allow-existing] [--socket <PATH>]
```

**Rule format:** `cidr:port/proto` (e.g., `10.0.0.0/8:443/tcp`) or bare `cidr`.

```sh
vmm update-egress --allow 10.0.0.0/8:443/tcp --allow 8.8.8.8/32:53/udp
```

### `vmm pull`: Pull an OCI image and convert to ext4

```sh
vmm pull <IMAGE_REF> --output <PATH> [--size <MIB>] [--auth <PATH>] [--agent <PATH>]
```

```sh
vmm pull docker://ubuntu:22.04 --output ubuntu.ext4 --size 1024
vmm pull ghcr.io/owner/repo:tag --output app.ext4 --auth ~/.docker/auth.json \
  --agent guest/agent/vmm-agent
```

With `--agent`, the pull path installs the guest exec agent at
`/usr/sbin/vmm-agent` and points `/sbin/init` at it when the image has no init.

### Global Flags

| Flag | Description |
|---|---|
| `-v` | Verbose logging (info level) |
| `-vv` | Debug logging |
| `--socket <PATH>` | Default API socket for API commands (default: `/run/vmm.sock`) |

---

## API Reference

### Protocol

The API server listens on a Unix domain socket and accepts **length-prefixed JSON**:
```
[4-byte big-endian length][JSON body]
```

Each connection handles one request → one response, except `attach_pty`, which
switches the connection to stream framing.

### Requests

All requests are JSON objects with an `op` field (snake_case):

#### `create`: Boot a new VM

```json
{
  "op": "create",
  "config": {
    "kernel": {
      "path": "guest/vmlinux.minimal",
      "cmdline": "console=ttyS0 quiet loglevel=0 reboot=k panic=-1 nomodule pci=off root=/dev/vda rw init=/usr/sbin/vmm-agent",
      "initramfs": null
    },
    "memory": { "size_mib": 256 },
    "vcpus": { "count": 1 },
    "volumes": [
      { "path": "build/ubuntu-agent.ext4", "read_only": false }
    ],
    "net": []
  }
}
```

The rootfs in this example is produced by `vmm pull --agent`, which installs the
exec agent at `/usr/sbin/vmm-agent` so the VM can service `exec` requests.

#### `stop`: Stop a VM
```json
{ "op": "stop" }
```

#### `pause`: Pause a VM
```json
{ "op": "pause" }
```

#### `resume`: Resume a paused VM
```json
{ "op": "resume" }
```

#### `suspend`: Pause and release resident guest RAM
```json
{ "op": "suspend" }
```

#### `snapshot`: Create a snapshot
```json
{ "op": "snapshot", "diff": false }
```

#### `restore`: Restore from a snapshot file
```json
{ "op": "restore", "snapshot_path": "/tmp/vmm-vm-1.snap", "overlay": "/path/to/clone.cow" }
```

#### `exec`: Execute a command in the guest
```json
{ "op": "exec", "command": "echo hello", "timeout_ms": 5000 }
```

#### `attach_pty`: Attach an interactive PTY stream
```json
{ "op": "attach_pty", "cols": 120, "rows": 40, "shell": "/bin/sh" }
```

#### `update_egress`: Update egress policy on a live VM
```json
{
  "op": "update_egress",
  "allowlist": ["10.0.0.0/8:443/tcp", "8.8.8.8/32:53/udp"],
  "allow_existing": true
}
```

#### `status`: Return VM health and configuration
```json
{ "op": "status" }
```

### Responses

All responses are JSON objects with a `status` field:

```json
{ "status": "ok" }
{ "status": "snapshot", "path": "/tmp/vmm-vm-1.snap" }
{ "status": "restored" }
{ "status": "exec", "exit_code": 0, "stdout": "hello\n", "stderr": "", "duration_ms": 15 }
{ "status": "egress_updated", "rules_applied": 2 }
{ "status": "vm_status", "state": "running", "uptime_ms": 1234, "vcpus": 1, "mem_mib": 256, "volumes": 1, "nets": 1, "kernel": "guest/bzImage", "vcpu_alive": true }
{ "status": "err", "msg": "VM not found" }
```

`attach_pty` returns no JSON response. After the request, the connection uses
the PTY frame protocol described in `docs/ssh-pty.md`.

### Client Example (Python)

```python
import socket, struct, json

def recv_exact(sock, n):
    chunks = []
    while n:
        chunk = sock.recv(n)
        if not chunk:
            raise RuntimeError("socket closed")
        chunks.append(chunk)
        n -= len(chunk)
    return b"".join(chunks)

def vmm_request(socket_path, request):
    body = json.dumps(request).encode()
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(socket_path)
    s.sendall(struct.pack(">I", len(body)) + body)
    resp_len = struct.unpack(">I", recv_exact(s, 4))[0]
    resp = json.loads(recv_exact(s, resp_len))
    s.close()
    return resp

# Boot a VM from an agent-baked rootfs created with `vmm pull --agent`.
print(vmm_request("build/run/vmm.sock", {
    "op": "create",
    "config": {
        "kernel": {
            "path": "guest/vmlinux.minimal",
            "cmdline": "console=ttyS0 quiet loglevel=0 reboot=k panic=-1 nomodule pci=off root=/dev/vda rw init=/usr/sbin/vmm-agent",
            "initramfs": None,
        },
        "memory": {"size_mib": 256},
        "vcpus": {"count": 1},
        "volumes": [{"path": "build/ubuntu-agent.ext4", "read_only": False}],
        "net": []
    }
}))

# Execute a command through the guest agent.
print(vmm_request("build/run/vmm.sock", {"op": "exec", "command": "uname -r", "timeout_ms": 5000}))

# VM status
print(vmm_request("build/run/vmm.sock", {"op": "status"}))

# Snapshot
print(vmm_request("build/run/vmm.sock", {"op": "snapshot", "diff": False}))

# Stop
print(vmm_request("build/run/vmm.sock", {"op": "stop"}))
```

### Client Example (curl)

The API uses a Unix socket, not HTTP. Use `socat` or `nc`:
```sh
echo -ne '\x00\x00\x00\x0f{"op":"status"}' | socat - UNIX-CONNECT:/tmp/vmm.sock
```

---

## Architecture

```
crates/
  vmm-core/           KVM VM/vCPU, run loop, CPU templates, controller,
                      live snapshot, security, clone, OCI, UFFD, guest channel
  vmm-memory-backend/ Guest memory (mmap), dirty bitmap, KVM registration,
                      dirty-log ioctl, UFFD lazy restore
  vmm-loader/         Kernel load (bzImage/ELF), E820 map, zero page, cmdline
  vmm-devices/        MMIO bus, virtio-mmio transport, virtio-blk (backend +
                      transport + vqueue walker), virtio-net, virtio-rng, serial
  vmm-snapshot/       CRC state file, diff snapshots, clone plans, live
                      snapshot convergence, snapshot format
  vmm-net/            TAP creation, nftables egress compiler, DNS-aware
                      allowlists, port forwarding, live egress update, rate limiter
  vmm-jailer/         Jailer config, seccomp profiles, cgroup limits, real
                      execution (chroot + namespaces + privilege drop)
  vmm-migration/      Migration state machine, negotiation, transport config
  vmm-api/            Length-prefixed JSON over UDS, request/response types,
                      dispatch
  vmm-integration/    E2E tests (boot, snapshot, egress, virtio-blk, comprehensive)
src/                  The vmm binary (CLI + wiring)
docs/                 Build docs, feature status, perf analysis, journal, remaining work
ci/                   CI scripts (check.sh, kvm-runner-bootstrap.sh, perf-gates.sh)
guest/                Guest kernel configs + bzImage (gitignored)
```

## Security Model

- **Seccomp confinement**: the jailer seccomp profile blocks process network syscalls (socket, connect, bind, sendto, recvfrom)
- **VM-to-VM isolation**: each VM gets its own netns, no bridge between VMs
- **Host-enforced egress**: nftables default-deny + allowlist, guest cannot alter
- **Jailer**: chroot + mount namespace + privilege drop + seccomp + cgroup v2 limits
- **KVM isolation**: guest "physical" memory is host userspace pages, guest can't read host memory
