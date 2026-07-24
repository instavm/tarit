# Bring your own orchestrator

This document describes how to drive Tarit VMM from a third-party control plane without using `taritd`. The wire contract is the `tarit-proto` crate in the monorepo `proto/` directory.

## Architecture

Run one `vmm serve --socket <uds>` process per microVM:

```sh
vmm serve --socket /run/tarit/vm-123.sock
```

The VMM owns exactly one microVM. It has no VM id field in the CLI or API. Your orchestrator maps its own VM id to a process id, socket path, rootfs, tap, jail, cgroup, and snapshot paths.

The orchestrator owns:

- Process lifecycle for each `vmm serve` instance.
- Placement and scheduling.
- Kernel, rootfs, volume, overlay, and snapshot paths.
- TAP creation and host networking.
- Network namespace, jail, cgroup, uid, and gid setup.
- UDS permissions and any higher-level auth.
- Multi-VM state, names, ids, and cleanup.

From the repository root, `sudo make guest` prepares the release kernel at
`guest-assets/vmlinux` and an agent-enabled rootfs at
`guest-assets/rootfs.ext4`. Deploy those files to stable host paths readable by
the VMM. The release kernel is an ELF `vmlinux`; bzImage is only an alternate
loader input for user-supplied kernels.

The VMM owns:

- KVM VM creation for one microVM.
- virtio-mmio block, net, rng, and vsock wiring used by the create path.
- Snapshot, restore, pause, suspend, resume, stop, status, exec, PTY attach, and egress update operations for that one VM.

`taritd` is one orchestrator built on this contract. The contract is real and load-bearing, not a private implementation detail.

## Wire protocol

Connect to the Unix domain socket. For normal requests, send exactly one JSON `ApiRequest` frame and read exactly one JSON `ApiResponse` frame:

```text
[4-byte big-endian u32 length][JSON body]
```

Each non-PTY connection handles one request and one response. The server rejects JSON frames larger than 16 MiB. `attach_pty` is different: after the request frame, the same connection switches to PTY stream framing and does not return an `ApiResponse`.

All request JSON uses an `op` field in snake_case. There are no VM ids.

## Request JSON

### `create`

Boot the single VM. This is the API equivalent of creating a live VM under `vmm serve`. The API create path uses the full boot path. There is no `full_boot` field in `VmConfig`.

```json
{
  "op": "create",
  "config": {
    "kernel": {
      "path": "/var/lib/tarit/vmlinux",
      "cmdline": "root=/dev/vda console=ttyS0 reboot=k panic=1 nokaslr",
      "initramfs": null
    },
    "memory": { "size_mib": 512 },
    "vcpus": { "count": 1 },
    "volumes": [
      { "path": "/var/lib/tarit/rootfs.ext4", "read_only": false, "overlay": null },
      { "path": "disks/data-base.ext4", "read_only": true, "overlay": "run/vm0-data.cow" }
    ],
    "net": [
      {
        "tap": "tap0",
        "guest_mac": "02:00:00:00:00:02",
        "guest_ip": "172.16.0.2",
        "port_forwards": [
          { "host_port": 8080, "guest_port": 80, "proto": "tcp" }
        ]
      }
    ]
  }
}
```

`volumes` and `net` default to empty lists. `initramfs`, `overlay`, `guest_mac`, and `guest_ip` may be `null` or omitted when using `tarit-proto` types. `port_forwards` defaults to an empty list. `proto` defaults to `tcp` when omitted.

### `pause`

```json
{ "op": "pause" }
```

### `suspend`

```json
{ "op": "suspend" }
```

### `resume`

```json
{ "op": "resume" }
```

### `snapshot`

```json
{ "op": "snapshot", "diff": false }
```

Set `diff` to `true` to request an incremental snapshot when the VMM has a previous snapshot and dirty logging is active.

### `restore`

```json
{ "op": "restore", "snapshot_path": "/path/to/vm.snap", "overlay": null }
```

`overlay` is optional. Use it when restoring a clone with a private CoW overlay.

### `stop`

```json
{ "op": "stop" }
```

### `exec`

```json
{ "op": "exec", "command": "uname -a", "timeout_ms": 5000 }
```

`timeout_ms` defaults to `0` in the wire type. Guest exec requires a guest image that runs the VMM guest agent for the vsock exec path.

### `attach_pty`

```json
{ "op": "attach_pty", "cols": 120, "rows": 40, "shell": "/bin/sh" }
```

After this request, the connection becomes a PTY stream. No JSON `ApiResponse` is sent.

### `update_egress`

```json
{
  "op": "update_egress",
  "allowlist": ["10.0.0.0/8:443/tcp", "8.8.8.8/32:53/udp"],
  "allow_existing": true
}
```

Rules are `cidr:port/proto` or bare `cidr`. `allow_existing` defaults to `false`. Enforcement is intended for a per-VM network namespace started with `serve --netns`; without a netns, the VMM validates and reports rule counts without applying host-wide nftables changes.

### `status`

```json
{ "op": "status" }
```

## Response JSON

All non-PTY responses are internally tagged with a `status` field:

```json
{ "status": "ok" }
```

```json
{ "status": "snapshot", "path": "/tmp/vmm-123-456.snap" }
```

```json
{ "status": "restored" }
```

```json
{
  "status": "exec",
  "exit_code": 0,
  "stdout": "hello\n",
  "stderr": "",
  "duration_ms": 12
}
```

```json
{ "status": "egress_updated", "rules_applied": 2 }
```

```json
{
  "status": "vm_status",
  "state": "running",
  "uptime_ms": 1234,
  "vcpus": 1,
  "mem_mib": 512,
  "volumes": 1,
  "nets": 1,
  "kernel": "/var/lib/tarit/vmlinux",
  "vcpu_alive": true
}
```

```json
{ "status": "err", "msg": "error message" }
```

`state` is one of `created`, `running`, `paused`, `suspended`, or `stopped`.

## Use `tarit-proto`

Prefer depending on `tarit-proto` instead of hand-writing structs. It defines `ApiRequest`, `ApiResponse`, `VmConfig`, `VmStatus`, `PtyStreamFrame`, constants for PTY frame types, and framing helpers.

For a crate checked out next to `proto/` in this monorepo:

```toml
[dependencies]
tarit-proto = { path = "../proto" }
serde_json = "1"
```

Adjust the path for your repository layout. Once published, use the crates.io version instead of a path dependency.

Minimal client:

```rust
use std::error::Error;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

use tarit_proto::{
    ApiRequest, ApiResponse, KernelConfig, MemoryConfig, VcpuConfig, VmConfig, VmSpec,
    VolumeConfig,
};

fn call(socket_path: &str, req: &ApiRequest) -> Result<ApiResponse, Box<dyn Error>> {
    let mut stream = UnixStream::connect(socket_path)?;
    let body = serde_json::to_vec(req)?;
    let len: u32 = body.len().try_into()?;

    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(&body)?;
    stream.flush()?;

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let resp_len = u32::from_be_bytes(len_buf) as usize;
    let mut resp = vec![0u8; resp_len];
    stream.read_exact(&mut resp)?;

    Ok(serde_json::from_slice(&resp)?)
}

fn main() -> Result<(), Box<dyn Error>> {
    let socket = "/run/tarit/vm-123.sock";

    let config = VmConfig {
        kernel: KernelConfig {
            path: "/var/lib/tarit/vmlinux".into(),
            cmdline: "root=/dev/vda console=ttyS0 reboot=k panic=1 nokaslr".into(),
            initramfs: None,
        },
        memory: MemoryConfig { size_mib: 512 },
        vcpus: VcpuConfig { count: 1 },
        volumes: vec![VolumeConfig {
            path: "/var/lib/tarit/rootfs.ext4".into(),
            read_only: false,
            overlay: None,
        }],
        net: vec![],
    };

    let create = ApiRequest::Create(VmSpec { config });
    let create_resp = call(socket, &create)?;
    if !matches!(create_resp, ApiResponse::Ok) {
        eprintln!("create response: {create_resp:#?}");
        return Ok(());
    }

    let exec_resp = call(
        socket,
        &ApiRequest::Exec {
            command: "uname -a".into(),
            timeout_ms: 5000,
        },
    )?;
    println!("{exec_resp:#?}");

    Ok(())
}
```

## PTY streaming

`attach_pty` starts as a normal framed JSON request:

```json
{ "op": "attach_pty", "cols": 120, "rows": 40, "shell": "/bin/sh" }
```

After the request frame, the same UDS connection switches to the PTY frame format from `proto/src/pty.rs`:

```text
[1-byte type][4-byte big-endian u32 length][payload]
```

Frame type constants:

| Constant | Value | Payload |
|---|---:|---|
| `TYPE_DATA` | `0` | Raw terminal bytes. Host to guest data uses this type. Guest output also uses this type. |
| `TYPE_RESIZE` | `1` | JSON resize payload: `{ "cols": 120, "rows": 40 }`. |
| `TYPE_EXIT` | `2` | PTY exit. The shared payload shape is `{ "exit_code": 0 }` when present. |
| `TYPE_ERROR` | `3` | UTF-8 error message. |
| `TYPE_START` | `4` | JSON start payload: `{ "cols": 120, "rows": 40, "shell": "/bin/sh" }`. The VMM uses this on the guest-side vsock leg. UDS clients normally send the `attach_pty` request instead. |

Use `tarit_proto::write_frame`, `tarit_proto::read_frame`, and `tarit_proto::write_json_frame` if you implement a Rust PTY client. PTY frames have a 16 MiB maximum payload length.

## Operational notes

- The VMM does not implement control-plane auth. Use Unix socket permissions, process supervision, and your own API layer.
- Track external VM ids in your orchestrator. The VMM protocol intentionally has no ids.
- One socket maps to one VMM process and one microVM.
- Start `vmm serve` with `--jail`, `--netns`, `--cgroup`, and `--cpuset` when your control plane needs confinement.
- Use `status` for health checks and `stop` for graceful teardown before killing the process.
- For the complete CLI and API reference, see [BUILD-AND-API.md](BUILD-AND-API.md).
