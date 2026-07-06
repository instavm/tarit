# 07 — Phase 1: virtio-blk + virtio-net + I/O loop (M7)

*Goal: real virtio-blk (file-backed, in-VMM backend) + virtio-net
(tap-backed) + the event-manager epoll I/O loop (PRD Phase 1, §7, §8).*

## What I did

### 1. `virtio/blk.rs` — real request parsing + validation

The virtio-blk request header (virtio 1.x §5.2.5) is 16 bytes:
`type (u32) + reserved (u32) + sector (u64)`. Added:
- `BlkReqHeader::from_bytes(&[u8]) -> Option<Self>` — parses the header,
  returns None if too short (the malformed-guest path).
- `req_type` module — the standard `IN`/`OUT`/`FLUSH`/`GET_ID`/`DISCARD`/
  `WRITE_ZEROES` constants.
- `validate_req(&header, data_len, sectors) -> Result<u64, BlkParseError>` —
  the host-agnostic validation: rejects unknown request types, rejects
  sector+offset out-of-bounds, rejects u64 overflow on `sector*512`, and
  short-circuits FLUSH/GET_ID (no bounds check needed). Returns the byte
  offset into the backing file.
- `status` module — the status byte the device writes (OK / IO_ERR / UNSUPP).

Nine tests cover: header round-trip, header-too-short, read at the last
sector, read past the end rejected, WRITE_ZEROES treated as write, unknown
type rejected, FLUSH/GET_ID skip bounds check, sector-offset overflow, and
Persist round-trip.

### 2. `virtio/net.rs` — real device state

`VirtioNet { mac, tap_name, features }` with `DEFAULT_MAC` (a
locally-administered unicast, Firecracker-style: `02:00:00:00:00:01`),
`features` module (the virtio-net feature bits: CSUM, GUEST_CSUM, MAC,
EVENT_IDX), and `new(tap_name, Option<mac>)`. Four tests: default MAC is
local+unicast, new uses provided or default MAC, Persist round-trip,
features include MAC.

### 3. `io_loop.rs` — event-manager I/O loop scaffold

`IoLoop` with `EventSource { fd, token }` registration. Linux: epoll_fd
field (full `epoll_create1` + `epoll_ctl` wiring lands in M7's real backend;
the structure is here). Non-Linux: stub. This is the skeleton the
event-manager thread will fill in.

## What worked

- **Pure-Rust validation, host-gated servicing.** The same M3/M4 pattern:
  `validate_req` is host-agnostic arithmetic (sector→offset, bounds check),
  so all 9 block tests run on macOS. The actual `pread`/`pwrite` against the
  backing file will be a thin Linux-gated shell that calls `validate_req`
  first. No KVM needed to test the correctness-critical parsing.
- **`is_none_or`** (Rust 1.95) replaced a clunky `map_or(true, ...)`. The
  clippy lint caught the idiom.

## What went wrong

### F1. Clippy: `map_or(true, |x| !pred)` → `is_none_or(|x| pred)`

```
error: use `is_none_or` instead
   | if header.sector.checked_add(n).map_or(true, |end| end > sectors) {
   |                       ^^^^^^^^^ help: use `is_none_or`
```

Rust 1.95 added `Option::is_none_or`. My `checked_add(...).map_or(true,
|end| end > sectors)` was exactly the pattern it replaces. Switched to
`.is_none_or(|end| end > sectors)`.

### F2. Two `mut` warnings on `save()` callers

`VirtioBlk::save` and `VirtioNet::save` take `&self`, so the test's
`let mut n = ...` was unused-mut. Removed `mut`.

### F3. fmt: `assert_eq!` line too long → multi-line

`cargo fmt` wrapped the long `assert_eq!(validate_req(...), Err(...))`.
Trivial.

## What I learned

- **virtio-blk's header is deceptively simple (16 bytes) but the bounds
  math is full of overflow traps.** `sector * 512` can overflow u64 for a
  malicious guest; `sector + n_sectors` can overflow too. `checked_mul` /
  `checked_add` everywhere, with a dedicated `Overflow` error variant. A
  guest that sends `sector = u64::MAX / 4` should get `IO_ERR`, not crash
  the VMM.
- **FLUSH and GET_ID are special-cased** (no data buffer → no bounds check).
  Forgetting this rejects legitimate requests. The test
  `flush_and_get_id_skip_bounds_check` guards it.
- **The MAC's first byte encodes unicast/multicast (low bit) and
  local/global (next bit).** `02` = local unicast, which is what you want
  for an assigned-by-the-host NIC. The `default_mac_is_local_unicast` test
  asserts the bit pattern, not the literal byte — more durable.

## Commands to reproduce

```sh
./ci/check.sh   # all 5 green; 29 device tests (M0:6 + M4:10 + M7:13)
```

## Next

`08-isolation.md` — Phase 2: `vmm-net` (per-VM netns + nftables default-deny
egress) + `vmm-jailer` (chroot + namespaces + cgroups + seccomp).
