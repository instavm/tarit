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

**Deploy the rebuilt agent and VMM as a paired rollout.** An old agent's
ephemeral exec source is rejected by the new VMM, and the old PTY port 1025 is
no longer used. There is no insecure compatibility fallback for vsock. Serial
exec retains its existing trusted-console boundary. Exec v2 and PTY frame
formats are unchanged; this is an intentional transport pairing change.

The empty-command readiness probe now requires a nonce-bound
`VMM_PROBE_OK:<nonce>:TRANSPORT=2` response from the running agent. A legacy or
unsupported reply fails with `AGENT_TRANSPORT_INCOMPATIBLE`; the orchestrator
aborts readiness immediately instead of publishing that guest as ready. Direct
VMM PTY attachment performs the same check before connecting. The agent sends
the v2 reply first and then the original nonce response so older VMM readiness
probes still work; **this does not make the new agent's PTY listener compatible
with an old VMM**. A stale reply with a different nonce cannot pass readiness.

Rollout order:

1. Build the new agent and VMM, bake the agent into every rootfs/OCI image
   variant, and test a cold boot with that exact pair. Keep old production
   images and VMM binaries available for the old pool during the drain.
2. Stop admitting new VMs on old hosts and drain their running VMs, warm pools,
   and golden templates. Do not restart the new agent in a live guest managed
   by an old VMM: old-host PTY/SSH still targets port 1025.
3. Replace the host VMM and configured images together on drained hosts. Cold
   boot new golden sources and regenerate warm pools and snapshots from the
   new pair before re-enabling admission. Exercise empty-command readiness,
   framed exec, PTY/SSH, snapshot capture, and restored readiness/PTY.
4. Existing hibernation, fork, golden, and warm snapshots contain the **old
   running agent in RAM**. Updating their backing disk or installing a new
   binary does not upgrade that process. Keep these snapshots on a compatible
   old pool long enough to resume and migrate application data, then cold boot
   a new-pair guest and capture a replacement. If no old pool remains, reject
   those restores and rebuild from application data; do not relabel old
   snapshots as compatible or bypass the gate. A new-host restore of an old
   agent must fail readiness with `AGENT_TRANSPORT_INCOMPATIBLE`.

For a machine-checkable preflight, call the VMM `Exec` API with an empty command
(the normal orchestrator create/resume readiness path does this). Success means
the currently running agent answered transport v2, including after restore;
checking a binary version or disk marker alone cannot prove RAM compatibility.
The gate diagnoses pairing and does not authenticate against guest root.

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

## Worker recovery

On Linux the original agent process supervises three independent workers:
vsock exec, vsock PTY, and serial fallback. A dead worker is restarted even
while another worker is executing a command or reading a partial request.
Restarts and fork failures back off from 100 ms to 5 seconds; ten seconds of
stable service resets the backoff. In-flight commands or PTY sessions are not
replayed. A reconnect after a worker failure starts a new request/session.
When running as PID 1, the supervisor also reaps adopted command orphans.
Between events it sleeps in `sigtimedwait` until a child exits or a restart
falls due, so an idle guest gets no wakeups from it. Workers and the commands
they run start with the signal mask the agent started with. When it is not
PID 1 the supervisor also waits for SIGTERM and SIGINT and then stops its
workers; as PID 1 it leaves both at their default, which the kernel ignores for
init, so neither can make init exit and panic the guest.

Run `make test` for process-level crash/restart, orphan isolation, fork failure,
backoff, idle wakeup, signal mask and cleanup regressions. The supervision tests
use ordinary POSIX children and do not require KVM or vsock. A live guest test
must install the rebuilt agent into a private rootfs fixture before exercising
worker recovery.
