# Standalone operation

This guide runs Tarit VMM with no `taritd` and no external control plane. The model is still one VMM process per microVM. There is no VM id flag.

See [BUILD-AND-API.md](BUILD-AND-API.md) for the full CLI reference.

## Prerequisites

- x86_64 Linux with KVM available at `/dev/kvm`.
- Rust 1.95 or newer to build the VMM.
- A guest Linux kernel image, usually `bzImage` or `vmlinux`.
- An ext4 rootfs image for userspace boot.
- Root or the needed capabilities for KVM, TAP devices, network namespaces, cgroups, and jailing.
- For OCI image conversion: `skopeo`, `umoci`, and `e2fsprogs`.

Build the binary on the Linux host:

```sh
cd vmm
cargo build --release -p vmm --features boot
```

## One-shot boot with `vmm run`

`vmm run` boots a VM from CLI flags and runs it in the foreground. It does not create a control socket. Use `vmm serve` if you need API control.

Fast boot mode is the default. It is useful for kernel-load and HLT-loop checks:

```sh
sudo target/release/vmm run --kernel guest/bzImage --mem 256
```

For a normal Linux userspace boot from an ext4 rootfs, use `--full-boot`:

```sh
sudo target/release/vmm run --kernel guest/bzImage \
  --rootfs guest/rootfs.ext4 \
  --mem 512 \
  --vcpus 1 \
  --full-boot \
  --cmdline "root=/dev/vda console=ttyS0 reboot=k panic=1 nokaslr"
```

Attach a data volume with a private CoW overlay. `--overlay` applies to `--volume`, not to `--rootfs`. If any overlays are specified, provide one overlay for each `--volume`.

```sh
mkdir -p run
sudo target/release/vmm run --kernel guest/bzImage \
  --rootfs guest/rootfs.ext4 \
  --volume disks/data-base.ext4:ro \
  --overlay run/vm0-data.cow \
  --full-boot \
  --cmdline "root=/dev/vda console=ttyS0 reboot=k panic=1 nokaslr"
```

Attach virtio-net to a host TAP. The orchestrator or operator is responsible for host networking and guest IP configuration.

```sh
sudo ip tuntap add dev tap0 mode tap
sudo ip addr add 172.16.0.1/24 dev tap0
sudo ip link set tap0 up
sudo target/release/vmm run --kernel guest/bzImage \
  --rootfs guest/rootfs.ext4 \
  --net tap=tap0,mac=02:00:00:00:00:02 \
  --full-boot \
  --cmdline "root=/dev/vda console=ttyS0 reboot=k panic=1 nokaslr"
```

### `run` flags

| Flag | Default | Meaning |
|---|---:|---|
| `--kernel <PATH>` | required | Kernel image, bzImage or vmlinux. |
| `--cmdline <CMDLINE>` | loader default | Kernel command line. |
| `--initramfs <PATH>` | none | Optional initramfs image. |
| `--mem <MIB>` | `256` | Guest memory in MiB. |
| `--vcpus <N>` | `1` | Number of vCPUs. |
| `--rootfs <PATH>` | none | Rootfs image attached as the first block device. |
| `--volume <PATH[:ro|rw]>` | none | Additional data volume. Repeatable. |
| `--overlay <PATH>` | none | Private sparse CoW overlay for each `--volume`. Repeatable, requires `--volume`. |
| `--net <SPEC>` | none | One virtio-net device. Format: `tap=<name>[,mac=aa:bb:cc:dd:ee:ff]`. |
| `--full-boot` | off | Enable IRQCHIP and PIT for normal userspace boot. |
| `--jail <CHROOT_DIR>` | none | Apply jailer confinement before boot. |
| `--uid <UID>` | `1000` | UID to drop to when jailed. |
| `--gid <GID>` | `1000` | GID to drop to when jailed. |

## API-server workflow with `vmm serve`

`vmm serve` starts one API server for one VM. It blocks in the foreground and listens on a Unix domain socket.

```sh
sudo target/release/vmm serve --socket /tmp/vmm.sock
```

There is no `vmm create` CLI subcommand. Create the VM by sending an API `create` request, then use the CLI API subcommands against the same socket.

```sh
python3 - <<'PY'
import json, socket, struct

SOCK = "/tmp/vmm.sock"

def recvn(sock, n):
    data = b""
    while len(data) < n:
        chunk = sock.recv(n - len(data))
        if not chunk:
            raise EOFError("socket closed")
        data += chunk
    return data

req = {
    "op": "create",
    "config": {
        "kernel": {
            "path": "guest/bzImage",
            "cmdline": "root=/dev/vda console=ttyS0 reboot=k panic=1 nokaslr",
            "initramfs": None,
        },
        "memory": {"size_mib": 512},
        "vcpus": {"count": 1},
        "volumes": [{"path": "guest/rootfs.ext4", "read_only": False, "overlay": None}],
        "net": [],
    },
}
body = json.dumps(req).encode()
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
    s.connect(SOCK)
    s.sendall(struct.pack(">I", len(body)) + body)
    size = struct.unpack(">I", recvn(s, 4))[0]
    print(recvn(s, size).decode())
PY
```

Drive the running VM through the socket. The global `--socket` flag selects the target `vmm serve` process.

```sh
target/release/vmm --socket /tmp/vmm.sock status
target/release/vmm --socket /tmp/vmm.sock exec "uname -a" --timeout 5000
target/release/vmm --socket /tmp/vmm.sock attach-pty --shell /bin/sh
target/release/vmm --socket /tmp/vmm.sock pause
target/release/vmm --socket /tmp/vmm.sock resume
target/release/vmm --socket /tmp/vmm.sock suspend
target/release/vmm --socket /tmp/vmm.sock resume
target/release/vmm --socket /tmp/vmm.sock snapshot
target/release/vmm --socket /tmp/vmm.sock snapshot --diff
target/release/vmm --socket /tmp/vmm.sock update-egress \
  --allow 10.0.0.0/8:443/tcp \
  --allow 8.8.8.8/32:53/udp \
  --allow-existing
target/release/vmm --socket /tmp/vmm.sock stop
```

Restore inside a `vmm serve` process by sending the API `restore` request. `overlay` is optional.

```sh
python3 - <<'PY'
import json, socket, struct

SOCK = "/tmp/vmm.sock"
SNAPSHOT = "/path/to/snapshot.snap"

def recvn(sock, n):
    data = b""
    while len(data) < n:
        chunk = sock.recv(n - len(data))
        if not chunk:
            raise EOFError("socket closed")
        data += chunk
    return data

req = {"op": "restore", "snapshot_path": SNAPSHOT, "overlay": None}
body = json.dumps(req).encode()
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
    s.connect(SOCK)
    s.sendall(struct.pack(">I", len(body)) + body)
    size = struct.unpack(">I", recvn(s, 4))[0]
    print(recvn(s, size).decode())
PY
```

To restore as a separate foreground VMM process, use the local restore command instead:

```sh
sudo target/release/vmm restore --snapshot /path/to/snapshot.snap
```

### `serve` flags

`--socket` is a global flag used by `serve` and by API client subcommands.

| Flag | Default | Meaning |
|---|---:|---|
| `--socket <PATH>` | `/run/vmm.sock` | Unix socket path. |
| `--jail <CHROOT_DIR>` | none | Apply jailer confinement before serving. |
| `--uid <UID>` | `1000` | UID to drop to when jailed. |
| `--gid <GID>` | `1000` | GID to drop to when jailed. |
| `--netns <PATH>` | none | Network namespace path to enter when jailed. |
| `--cgroup <PATH>` | none | cgroup v2 path for the served VMM process. |
| `--cgroup-memory-max <BYTES>` | none | Set `memory.max`. Accepts bytes or K/M/G/T suffixes. Requires `--cgroup`. |
| `--cgroup-cpu-max <QUOTA/PERIOD|MILLICPU>` | none | Set `cpu.max`. Accepts `1000m`, `max`, `QUOTA/PERIOD`, or `QUOTA PERIOD`. Requires `--cgroup`. |
| `--cgroup-pids-max <N>` | none | Set `pids.max`. Requires `--cgroup`. |
| `--cpuset <CPUS>` | none | Set `cpuset.cpus`, for example `0-3` or `0,2`. Requires `--cgroup`. |

## OCI images

`vmm pull` pulls an OCI image and converts it to an ext4 disk image. With `--agent`, the VMM guest exec agent is installed as `/usr/sbin/vmm-agent` and can be used as the image init when needed.

```sh
mkdir -p images
target/release/vmm pull docker://alpine:3.20 \
  --output images/alpine.ext4 \
  --size 1024 \
  --agent guest/agent/vmm-agent

sudo target/release/vmm run --kernel guest/bzImage \
  --rootfs images/alpine.ext4 \
  --full-boot \
  --cmdline "root=/dev/vda console=ttyS0 reboot=k panic=1 nokaslr"
```

Private registries can pass an auth file:

```sh
target/release/vmm pull docker://ghcr.io/owner/image:tag \
  --output images/app.ext4 \
  --size 2048 \
  --auth "$HOME/.docker/config.json" \
  --agent guest/agent/vmm-agent
```

### `pull` flags

| Flag | Default | Meaning |
|---|---:|---|
| `<IMAGE_REF>` | required | OCI reference, for example `docker://ubuntu:22.04`. |
| `--output <PATH>` | required | Output ext4 disk image path. |
| `--size <MIB>` | `1024` | Disk image size in MiB. |
| `--auth <PATH>` | none | Auth file for private registries. |
| `--agent <PATH>` | none | Compiled guest exec agent to inject into the image. |

## Jailer and cgroup confinement

Use `serve` confinement when an external supervisor launches one VMM process per microVM. When jailed, the socket path is interpreted inside the chroot. The supervisor must make the socket reachable outside the jail and provide `/dev/kvm` inside the jail.

```sh
sudo target/release/vmm serve \
  --socket /run/vmm.sock \
  --jail /srv/tarit/vm0-jail \
  --uid 1000 \
  --gid 1000 \
  --netns /var/run/netns/vm0 \
  --cgroup /sys/fs/cgroup/tarit/vm0 \
  --cgroup-memory-max 512M \
  --cgroup-cpu-max 1000m \
  --cgroup-pids-max 64 \
  --cpuset 0-1
```

Live egress updates are intended to be enforced inside a per-VM network namespace. Without `--netns`, `update-egress` validates and reports rule counts without applying host-wide nftables changes.
