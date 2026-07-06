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

### `vmm run` (`start`): Boot a fresh VM

```sh
vmm run --kernel <PATH> [OPTIONS]
```

| Flag | Default | Description |
|---|---|---|
| `--kernel <PATH>` | (required) | Path to bzImage or vmlinux |
| `--cmdline <CMDLINE>` | loader default | Kernel command line |
| `--initramfs <PATH>` | none | Path to initramfs image |
| `--mem <MIB>` | 256 | Guest memory size in MiB |
| `--vcpus <N>` | 1 | Number of vCPUs |
| `--rootfs <PATH>` | none | Attach a boot rootfs as `/dev/vda` read-write |
| `--volume <PATH[:ro\|rw]>` | none | Attach a storage volume (repeatable) |
| `--overlay <PATH>` | none | Attach a private CoW overlay for each `--volume` |
| `--net <SPEC>` | none | Attach virtio-net to a pre-created TAP (`tap=<name>[,mac=...]`) |
| `--full-boot` | off | Enable IRQCHIP + PIT (needed to reach userspace init) |
| `--jail <DIR>` | none | Run inside a jail rooted at `DIR` |
| `--uid <UID>` | 1000 | UID to drop to when jailed |
| `--gid <GID>` | 1000 | GID to drop to when jailed |

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

### `vmm create`: Boot a VM inside `vmm serve` (via API)

```sh
vmm create --kernel <PATH> [OPTIONS] [--socket <PATH>]
```

This sends an API `create` request to an existing `vmm serve` socket.

| Flag | Default | Description |
|---|---|---|
| `--kernel <PATH>` | (required) | Path to bzImage or vmlinux |
| `--cmdline <CMDLINE>` | loader default, with `root=/dev/vda rw` prepended when `--rootfs` is set | Kernel command line |
| `--initramfs <PATH>` | none | Path to initramfs image |
| `--mem <MIB>` | 256 | Guest memory size in MiB |
| `--vcpus <N>` | 1 | Number of vCPUs |
| `--rootfs <PATH>` | none | Attach a boot rootfs as `/dev/vda` read-write |
| `--volume <PATH[:ro\|rw]>` | none | Attach a storage volume (repeatable) |
| `--overlay <PATH>` | none | Attach a private CoW overlay for each `--volume` |

### `vmm serve` (`server`): Start the API server

```sh
vmm serve [OPTIONS] [--socket <PATH>]
```

| Flag | Default | Description |
|---|---|---|
| `--socket <PATH>` | `/run/vmm.sock` | Unix socket path |
| `--jail <DIR>` | none | Run the served VM path inside a jail |
| `--uid <UID>` | 1000 | UID to drop to when jailed |
| `--gid <GID>` | 1000 | GID to drop to when jailed |
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

`--cgroup-memory-max`, `--cgroup-cpu-max`, `--cgroup-pids-max`, and `--cpuset`
require `--cgroup`. `--cgroup-memory-max` accepts bytes or K/M/G/T suffixes.
`--cgroup-cpu-max` accepts `max`, `1000m`, `QUOTA/PERIOD`, or `QUOTA PERIOD`.

### `vmm restore` (`load`): Restore from snapshot

```sh
vmm restore --snapshot <PATH> [OPTIONS]
```

| Flag | Default | Description |
|---|---|---|
| `--snapshot <PATH>` | (required) | Path to the snapshot file |
| `--jail <DIR>` | none | Run inside a jail rooted at `DIR` |
| `--uid <UID>` | 1000 | UID to drop to when jailed |
| `--gid <GID>` | 1000 | GID to drop to when jailed |

### `vmm snapshot` (`snap`): Snapshot a running VM (via API)

```sh
vmm snapshot [--diff] [--socket <PATH>]
```

| Flag | Default | Description |
|---|---|---|
| `--diff` | off | Create a diff snapshot |

### `vmm status` (`info`): Show VM status

```sh
vmm status [--socket <PATH>]
```

### `vmm exec` (`run-in`): Execute a command in a guest

```sh
vmm exec <COMMAND> [--timeout <MS>] [--socket <PATH>]
```

| Flag | Default | Description |
|---|---|---|
| `<COMMAND>` | (required) | Command to execute |
| `--timeout <MS>` | 5000 | Timeout in milliseconds. `0` selects the built-in 30 second timeout |

### `vmm attach-pty`: Attach an interactive PTY

```sh
vmm attach-pty [--shell <SHELL>] [--socket <PATH>]
```

| Flag | Default | Description |
|---|---|---|
| `--shell <SHELL>` | guest default | Shell path to exec in the guest |

### `vmm stop` (`kill`): Stop a VM

```sh
vmm stop [--socket <PATH>]
```

### `vmm gc`: Remove orphaned VMM scratch files

```sh
vmm gc [--dir /tmp] [--max-age <SECS>]
```

| Flag | Default | Description |
|---|---|---|
| `--dir <DIR>` | `/tmp` | Directory to sweep |
| `--max-age <SECS>` | 3600 | Minimum scratch-file age before removal |

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

### `vmm update-egress` (`egress`): Update egress policy on a live VM

```sh
vmm update-egress --allow <RULE>... [--allow-existing] [--socket <PATH>]
```

| Flag | Default | Description |
|---|---|---|
| `--allow <RULE>` | none | Allowlist rule. Repeatable |
| `--allow-existing` | off | Allow existing connections to persist |

**Rule format:** `cidr:port/proto` (e.g., `10.0.0.0/8:443/tcp`) or bare `cidr`.

```sh
vmm update-egress --allow 10.0.0.0/8:443/tcp --allow 8.8.8.8/32:53/udp
```

### `vmm pull` (`oci-pull`): Pull an OCI image and convert to ext4

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

| Flag | Default | Description |
|---|---|---|
| `<IMAGE_REF>` | (required) | OCI image reference |
| `--output <PATH>` | (required) | Output disk image path |
| `--size <MIB>` | 1024 | Disk image size in MiB |
| `--auth <PATH>` | none | Auth file path for private registries |
| `--agent <PATH>` | none | Compiled guest exec agent to inject |

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

The maximum accepted frame body is 16 MiB. The server creates a missing socket
parent directory with mode `0700`, sets the socket node to mode `0600`, and
removes a stale socket node only when that path is a socket. On Linux, API peers
must be root or the same effective UID as the server.

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

`create` has one field, `config`. `config` contains:

| Field | Required | Description |
|---|---:|---|
| `kernel.path` | yes | Kernel image path |
| `kernel.cmdline` | yes | Kernel command line |
| `kernel.initramfs` | no | Initramfs path, or `null` |
| `memory.size_mib` | yes | Guest memory in MiB |
| `vcpus.count` | yes | Number of vCPUs |
| `volumes` | no | Volume list. Defaults to `[]` |
| `volumes[].path` | yes | Disk image path |
| `volumes[].read_only` | yes | Open the disk read-only when no overlay is set |
| `volumes[].overlay` | no | CoW overlay path, or `null` |
| `net` | no | Network device list. Defaults to `[]` |
| `net[].tap` | yes | Host TAP name |
| `net[].guest_mac` | no | Guest MAC, or `null` |
| `net[].guest_ip` | no | Guest IP, or `null` |
| `net[].port_forwards` | no | Port forward list. Defaults to `[]` |
| `net[].port_forwards[].host_port` | yes | Host port |
| `net[].port_forwards[].guest_port` | yes | Guest port |
| `net[].port_forwards[].proto` | no | Protocol. Defaults to `tcp` |

Optional fields may be `null` or omitted when using the shared wire types.

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
Set `diff` to `true` to request a diff snapshot.

#### `restore`: Restore from a snapshot file
```json
{ "op": "restore", "snapshot_path": "/tmp/vmm-vm-1.snap", "overlay": null }
```
`overlay` is optional. Use it for a private CoW overlay on restore.

#### `exec`: Execute a command in the guest
```json
{ "op": "exec", "command": "echo hello", "timeout_ms": 5000 }
```
`timeout_ms` defaults to `0` in the wire type. The controller treats `0` as
the built-in 30 second timeout.

#### `attach_pty`: Attach an interactive PTY stream
```json
{ "op": "attach_pty", "cols": 120, "rows": 40, "shell": "/bin/sh" }
```
`cols` and `rows` are required. `shell` is optional.

#### `update_egress`: Update egress policy on a live VM
```json
{
  "op": "update_egress",
  "allowlist": ["10.0.0.0/8:443/tcp", "8.8.8.8/32:53/udp"],
  "allow_existing": true
}
```
Rules are `cidr:port/proto`, `cidr:port`, or bare `cidr`. The default protocol
for `cidr:port` is `tcp`. Bare `cidr` allows any protocol and port.
`allow_existing` defaults to `false`. When `vmm serve` entered a network
namespace with `--netns`, the handler applies the rules. Otherwise it validates
and reports the rule count without applying host-wide rules.

#### `status`: Return VM health and configuration
```json
{ "op": "status" }
```

### Responses

All responses are JSON objects with a `status` field:

| Status | Fields | Used by |
|---|---|---|
| `ok` | none | `create`, `pause`, `suspend`, `resume`, `stop` |
| `snapshot` | `path` | `snapshot` |
| `restored` | none | `restore` |
| `exec` | `exit_code`, `stdout`, `stderr`, `duration_ms` | `exec` |
| `egress_updated` | `rules_applied` | `update_egress` |
| `vm_status` | `state`, `uptime_ms`, `vcpus`, `mem_mib`, `volumes`, `nets`, `kernel`, `vcpu_alive` | `status` |
| `err` | `msg` | Any non-PTY request |

```json
{ "status": "ok" }
{ "status": "snapshot", "path": "/tmp/vmm-vm-1.snap" }
{ "status": "restored" }
{ "status": "exec", "exit_code": 0, "stdout": "hello\n", "stderr": "", "duration_ms": 15 }
{ "status": "egress_updated", "rules_applied": 2 }
{ "status": "vm_status", "state": "running", "uptime_ms": 1234, "vcpus": 1, "mem_mib": 256, "volumes": 1, "nets": 1, "kernel": "guest/bzImage", "vcpu_alive": true }
{ "status": "err", "msg": "VM not found" }
```

The guest agent merges stderr into stdout, so `exec` responses carry the
combined output in `stdout` and the `stderr` field is currently always an
empty string.

`state` is one of `created`, `running`, `paused`, `suspended`, or `stopped`.
Bad JSON returns `{"status":"err","msg":"bad request: ..."}`. A handler panic
is caught and returned as `{"status":"err","msg":"internal error: ..."}`.

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
docs/                 Build and API reference, design choices, integration, benchmarks
ci/                   CI scripts (check.sh, kvm-runner-bootstrap.sh, perf-gates.sh)
guest/                Guest kernel configs + bzImage (gitignored)
```

## Security Model

- **Seccomp confinement**: the jailer seccomp profile blocks process network syscalls (socket, connect, bind, sendto, recvfrom)
- **VM-to-VM isolation**: each VM gets its own netns, no bridge between VMs
- **Host-enforced egress**: nftables default-deny + allowlist, guest cannot alter
- **Jailer**: chroot + mount namespace + privilege drop + seccomp + cgroup v2 limits
- **KVM isolation**: guest "physical" memory is host userspace pages, guest can't read host memory
