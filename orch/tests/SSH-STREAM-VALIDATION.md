# SSH stream correctness regression

Run `python3 vmm/guest/agent/test-stream-exec.py` on Linux with a C compiler and
libutil development headers. It compiles the actual guest agent stream handler
into a temporary, unprivileged socketpair harness. It verifies binary input and
output larger than the pipe buffers, EOF-driven completion, separate stderr,
nonzero exit status, scripts over 4KiB, cancellation of a HUP/TERM-ignoring child,
and unchanged interactive PTY CRLF behavior. It does not boot a VM or test vsock.

For a booted isolated test VM, install the matching agent and VMM from this PR,
register an SSH key, and run the same assertions through OpenSSH:

```
python3 -c 'import sys; sys.stdout.buffer.write(bytes(range(256))*2048)' > input.bin
ssh -T UUID@HOST 'cat; printf "stderr\n" >&2; exit 7' < input.bin > output.bin 2> errors.txt
# Exit must be 7, cmp input.bin output.bin must pass, errors.txt must be stderr + LF.
ssh -tt UUID@HOST 'printf "terminal\n"; exit 3'
```

Pipe mode uses a new `AttachExec` RPC and `EXEC_START` guest frame. Both the VMM
and guest agent must be upgraded. An older VMM rejects the RPC; an older guest
rejects the distinct start frame before running the command. There is no PTY
fallback. Existing buffered API exec and WebSocket PTYs retain their protocol.
SSH exec with a requested PTY intentionally retains terminal semantics.

The listener admits at most 256 connections, including at most 32 authenticating
connections. The 15s absolute authentication deadline includes identification,
key exchange and key lookup. Permits remain owned by the actual russh handler
until teardown. Each connection admits at most 16 session channels; stream PTY
limits still apply globally, per tenant and per VM. Failed-key durable audits
are globally bounded to a 20-event burst and one event per second; additional
denials are rejected without enqueueing audit rows. Successful audits identify
the SSH fingerprint rather than an empty key ID. These fixed limits bound
resources but do not guarantee availability against every distributed flood.
