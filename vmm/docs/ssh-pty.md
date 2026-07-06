# Interactive PTY over vsock

The VMM can attach an interactive pseudo-terminal to a running guest. It is used
by the orchestrator to offer two access paths into a sandbox with no in-guest
sshd: a WebSocket PTY and an SSH gateway. Both terminate at the same guest agent.

## Pieces

- **Guest agent** (`guest/agent/vmm-agent.c`): forks a dedicated PTY server that
  listens on **vsock port 1025** (host-initiated connect, one connection per PTY
  session). On a new connection it reads a `START` frame, `openpty()`s, forks a
  login shell (`setsid` + `TIOCSCTTY` + `dup2`), and relays. Resize is
  `ioctl(TIOCSWINSZ)`. This is separate from the exec server (vsock port 1024)
  and the serial fallback, so interactive shells never disturb `exec`.
- **Host channel** (`crates/vmm-core/src/vsock_pty.rs`): opens the vsock stream
  to the guest PTY port, writes the `START` frame, then byte-relays between the
  API connection and the guest, waking the virtio-vsock pump after host->guest
  writes. The relay runs off the vCPU thread.
- **API op** (`crates/vmm-api`): `ApiRequest::AttachPty { cols, rows, shell }`.
  The `serve` loop spawns a dedicated thread for the attach so other control ops
  (exec/status/stop) keep working while a PTY is open.
- **CLI**: `vmm attach-pty --socket <path> [--shell S]` puts the local terminal
  in raw mode and drives the stream, for manual testing.

## Stream frame protocol

After the client sends one length-prefixed `AttachPty` JSON request, the
connection switches to STREAM mode. Frames are `[1 byte type][4 byte BE len][payload]`,
identical on the API (orchestrator <-> VMM) and vsock (VMM <-> guest) legs so the
VMM is a near-pure relay:

| type | name   | direction        | payload                         |
|-----:|--------|------------------|---------------------------------|
| 0    | DATA   | both             | raw PTY bytes                   |
| 1    | RESIZE | toward guest     | JSON `{"cols":N,"rows":N}`       |
| 2    | EXIT   | from guest       | JSON `{"exit_code":N}`, then EOF |
| 3    | ERROR  | either           | UTF-8 message, then EOF          |
| 4    | START  | VMM -> guest     | JSON `{"cols","rows","shell"}`   |

The orchestrator (`taritd`) translates this 1:1 to its WebSocket framing
(binary = DATA, text JSON = RESIZE/EXIT) and to russh SSH channels
(channel data = DATA, window-change = RESIZE, exit = EXIT).

## Validate on KVM

```sh
sudo bash ci/pty-validate.sh   # on a Linux/KVM host; bakes the agent, boots, drives AttachPty
```

It confirms an interactive shell, a resize (`stty size` reflects it), faithful
command output, and a clean exit code.
