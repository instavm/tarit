# VMM guest agent

`vmm-agent` is a tiny guest agent for the microVM. It serves exec over serial
(`/dev/ttyS0`) and vsock port 1024, plus interactive PTY sessions over vsock
port 1025.

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

For PTY sessions, the VMM connects to guest vsock port 1025, sends a START
frame (`{"cols":N,"rows":N,"shell":...}`), and the agent allocates a fresh PTY
with `openpty(3)` for that session. DATA, RESIZE, EXIT, and ERROR frames use the
same 1-byte type + 4-byte big-endian length framing as the VMM API stream.

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
```

## Bake into a systemd ext4 rootfs

Run as root on Linux with loop-device support:

```sh
sudo guest/agent/bake-agent.sh /path/to/rootfs.ext4 guest/agent/vmm-agent
```

The script mounts the ext4 image with a loop device, installs the agent at `/usr/sbin/vmm-agent`, creates and enables `/etc/systemd/system/vmm-agent.service`, and masks `serial-getty@ttyS0.service` so getty does not compete for the serial port.

## Worker recovery

On Linux the original agent process supervises three independent workers:
vsock exec, vsock PTY, and serial fallback. A dead worker is restarted even
while another worker is executing a command or reading a partial request.
Restarts and fork failures back off from 100 ms to 5 seconds; ten seconds of
stable service resets the backoff. In-flight commands or PTY sessions are not
replayed. A reconnect after a worker failure starts a new request/session.
When running as PID 1, the supervisor also reaps adopted command orphans.

Run `make test` for process-level crash/restart, orphan isolation, fork failure,
backoff, and cleanup regressions. The supervision tests use ordinary POSIX
children and do not require KVM or vsock. A live guest test must install the
rebuilt agent into a private rootfs fixture before exercising worker recovery.
