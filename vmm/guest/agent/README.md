# VMM guest agent

`vmm-agent` is a tiny guest agent for the microVM. It serves exec over serial
(`/dev/ttyS0`) and vsock port 1024, plus interactive PTY sessions over vsock
port 1022.

For serial exec, the host writes one line to the 8250 serial port (`/dev/ttyS0`):

```text
VMM_EXEC:<command>\n
```

The agent sets the serial device to raw mode, reads commands forever, runs each command with `/bin/sh -c`, merges stdout and stderr, and replies on the same serial port:

```text
VMM_EXEC_START
<combined command output>
VMM_EXEC_EXIT=<exit-code>
```

Blank lines and non-`VMM_EXEC:` lines are ignored. Failed commands do not stop the agent.

For vsock exec, the agent still announces `VMM_AGENT_READY`, then advertises
`VMM_VSOCK_EXEC_PROTO=2`. Hosts that understand protocol v2 switch to chunked
frames (`VEX2`) with a request id plus separate stdout, stderr, and exit
messages; older hosts ignore the capability line and keep using the legacy
newline-delimited `VMM_EXEC:` transport.

For PTY sessions, the VMM connects to guest vsock port 1022, sends a START
frame (`{"cols":N,"rows":N,"shell":...}`), and the agent allocates a fresh PTY
with `openpty(3)` for that session. DATA, RESIZE, EXIT, and ERROR frames use the
same 1-byte type + 4-byte big-endian length framing as the VMM API stream.

## Privilege boundary and compatibility

The exec bridge accepts only guest source ports 1–1023 directed to host port
1024. The agent binds a free privileged source port before connecting, skipping
PTY port 1022; reconnect scans the remaining reserved ports and never falls back
to an ephemeral source. The PTY listener also uses privileged port 1022 and accepts only host CID 2,
rejecting guest-local clients.

This protects against guest processes lacking `CAP_NET_BIND_SERVICE` in the
initial user namespace. It does **not** authenticate against guest root, a
process holding that capability in the initial user namespace, or a compromised
guest kernel. Those callers can impersonate the agent; root can also access
agent memory and descriptors. The Linux 6.12.96
[vsock bind implementation](https://github.com/gregkh/linux/blob/v6.12.96/net/vmw_vsock/af_vsock.c)
checks `capable(CAP_NET_BIND_SERVICE)` for reserved ports, and
[`capable`](https://github.com/gregkh/linux/blob/v6.12.96/kernel/capability.c)
checks the initial user namespace. User-namespace root alone is insufficient.

**Deploy the rebuilt agent and VMM together.** An old agent's ephemeral exec
source is rejected by the new VMM, and the old PTY port 1025 is no longer used.
There is no insecure compatibility fallback for vsock. Serial exec remains
available and has its existing trusted-console boundary. Exec v2 and PTY frame
formats are unchanged; this is an intentional transport pairing change.

Command children receive EOF on stdin for serial and framed exec, default and
unblocked SIGPIPE, and no inherited descriptors above stderr. PTY children keep
their terminal stdin but receive the same signal and descriptor cleanup. Agent
control sockets, accepted PTY sockets, listeners and serial descriptors are
close-on-exec as an additional guard. Cleanup uses `close_range` on modern
Linux and enumerates open descriptors on older kernels; it does not scan the
potentially large descriptor limit.

## Build

On the Linux guest/rootfs build host:

```sh
cd guest/agent
make
# equivalent:
gcc -static -O2 -o vmm-agent vmm-agent.c -lutil
```

For a syntax-only check on hosts where static Linux linking is unavailable:

```sh
make syntax-check
# Linux: execute serial, framed-exec and PTY child state regressions
make test
```

## Bake into a systemd ext4 rootfs

Run as root on Linux with loop-device support:

```sh
sudo guest/agent/bake-agent.sh /path/to/rootfs.ext4 guest/agent/vmm-agent
```

The script mounts the ext4 image with a loop device, installs the agent at `/usr/sbin/vmm-agent`, creates and enables `/etc/systemd/system/vmm-agent.service`, and masks `serial-getty@ttyS0.service` so getty does not compete for the serial port.
