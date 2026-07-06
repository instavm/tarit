# 10 — API: control plane over UDS (M11)

*Goal: `vmm-api` — REST over a Unix Domain Socket (PRD §4). Operations:
create / pause / resume / snapshot / restore / stop.*

## What I did

Replaced the M0 `dispatch` stub with a real UDS server:
- `read_frame` / `write_frame` — 4-byte big-endian length-prefixed JSON
  framing (16 MiB cap to reject runaway frames).
- `dispatch(ApiRequest) -> ApiResponse` — pure function mapping each
  request variant to its response (Create→Ok{id}, Snapshot→Snapshot{path},
  Restore→Restored{id}, Pause/Resume/Stop→Ok{id}). The VMM wires this to its
  real state machine.
- `serve(socket_path)` — binds a `UnixListener`, loops accepting
  connections, reads a frame, dispatches, writes the response frame.

Five tests: dispatch_create echoes id, dispatch_snapshot returns a path,
dispatch_stop returns Ok, frame round-trip via Cursor, and an **end-to-end
test over a real Unix socket** (binds in tempdir, spawns a thread to accept,
connects a client, sends a Stop request, asserts the Ok{id} response).

## What worked

- **The end-to-end UDS test runs on macOS** (Unix sockets are POSIX, not
  Linux-only). It binds a real socket in `tempdir`, spawns a thread, and
  round-trips a request — proving the framing + dispatch + serialization
  all work, not just the dispatch function in isolation.
- **Pure `dispatch` + impure `serve`.** Splitting the dispatch logic from
  the I/O means the dispatch tests need no socket; the serve test proves
  the wiring. Easy to test, easy to swap the transport later (TCP, gRPC).

## What went wrong

### F1. `check.sh` timed out at 180s (false alarm)

First run of `check.sh` after M11 hit the 180s timeout. Investigation:
`cargo test -p vmm-api` ran fine in <1s. The actual culprit was `cargo fmt
--check` failing on a multi-line `Restore` arm that fmt wanted on one line;
`check.sh` exited non-zero at step 4 but the bash loop's `set -e` behavior
made it look like a hang. Running `cargo fmt --all` then `check.sh` again
passed cleanly. **Lesson:** a "timeout" can be a misleading signal when
`set -e` + a failing `--check` interact; check the actual failing step.

### F2. Dead-code warning on `IoLoop::epoll_fd` (Linux cross-check)

The M7 `io_loop` scaffold's `epoll_fd` field isn't read until the real
backend lands. Added `#[allow(dead_code)]` with the "Used when the real I/O
backend is wired" comment.

### F3. fmt: `Restore` arm multi-line → single-line

`cargo fmt` collapsed the multi-line `ApiResponse::Restored { id:
snapshot_path }` arm. Trivial.

## What I learned

- **UDS framing is trivial but easy to get wrong.** Length-prefix (4 bytes
  big-endian) + body is the simplest viable protocol. The 16 MiB cap is
  the only "safety" — a malicious client could otherwise stream 4 GiB.
  Real VMMs use protobuf or capmsg; for the scaffold, JSON+length-prefix
  is enough.
- **End-to-end tests over real sockets are worth it.** A `dispatch` unit
  test can't catch a framing bug (wrong endianness, off-by-one length).
  The `end_to_end_over_real_uds` test exercises the whole stack and runs
  in <1ms.

## Commands to reproduce

```sh
./ci/check.sh   # all 5 green; 5 api tests
```

## Next

`11-clones.md` — Phase 4 scaffold: suspend/resume + clones + CoW overlays.
