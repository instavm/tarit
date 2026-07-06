# 18 — CLI polish + OCI pipeline + jailer execution + comprehensive docs

*Goal: Polish CLI/API DX, implement remaining features, write docs.*

## What I did

### CLI Polish
- Added all missing subcommands: `stop`, `pause`, `resume`, `update-egress`, `pull`
- Global `--socket` flag (no need to repeat on every API command)
- Global `-v` / `-vv` verbosity flags
- Aliases: `start`, `load`, `server`, `snap`, `ls`, `run-in`, `kill`, `oci-pull`
- `vmm pull docker://ubuntu:22.04 --output ubuntu.ext4 --size 1024`
- Fixed duplicate alias panics (resume, pull)

### OCI Image Pipeline
- `vmm-core/oci.rs`: `pull_and_convert()` — skopeo pull + umoci unpack + mke2fs -d → ext4
- CLI: `vmm pull <image> --output <path> --size <mib> [--auth <path>]`
- Supports Docker Hub, GHCR, any OCI registry via skopeo

### Jailer Execution
- `vmm-jailer/executor.rs`: Real syscall execution
  - `setrlimit` (RLIMIT_NOFILE + RLIMIT_AS)
  - `setns` (enter pre-existing netns)
  - `unshare(CLONE_NEWNS)` (mount namespace)
  - `chroot` + `chdir("/")`
  - `prctl(PR_SET_NO_NEW_PRIVS)`
  - `setgid` + `setuid` (privilege drop)

### Live Snapshot Executor
- `vmm-core/live_snapshot.rs`: `live_snapshot()` function
  - Enables KVM dirty logging (register_with_dirty_logging)
  - Pre-copy loop: read_dirty_log → decide() → continue/stop
  - Final stop: pause vCPU → copy remaining dirty → resume
  - Returns LiveSnapshotResult (rounds, pages_copied, final_dirty, elapsed)

### Comprehensive E2E Test (44 checks)
- Boot, snapshot, restore, clone fan-out, egress policy, live egress update,
  egress diff, port forwarding, security policy (6 checks), rate limiter,
  DNS-aware egress, clock/PRNG restore, jailer config, OCI image ref,
  live snapshot convergence (5 edge cases: large/small/diverging/zero/max-rounds),
  diff snapshot equivalence, migration state machine (7 transitions + post-copy),
  API list, stop

### Documentation
- `docs/BUILD-AND-API.md`: Full build instructions, CLI reference, API reference
  with Python/curl examples, architecture diagram, security model
- `docs/remaining_work.md`: AWS instance access (IP, SSH key, sysctl settings),
  remaining tasks with priorities, test summary, performance numbers
- `docs/FEATURE-STATUS.md`: Updated feature audit

## What went wrong

### F1. Duplicate CLI aliases
`resume` was used as an alias for both `Restore` and `Resume`. `pull` was
both the command name and an alias. Clap panics on duplicate aliases.
Fixed: `Restore` alias → `load`, `Pull` alias → `oci-pull`.

### F2. vm-memory 0.18 trait resolution
`GuestMemoryMmap` (= `GuestRegionCollection<GuestRegionMmap>`) requires
both `Bytes` and `GuestMemoryBackend` traits in scope for `read_obj`/
`write_slice`. The `as _` import doesn't work because `GuestMemoryBackend`
has associated types. Fixed by using raw pointer access in the vqueue
walker (`region.as_ptr().add(gpa)`) — bypasses the trait resolution issue
entirely and is faster (no bounds checking in the hot path).

### F3. Avail ring offset (+2 vs +4)
The virtqueue walker read avail ring entries at offset +2 from the ring
base. The correct offset is +4 (skip flags(2) + idx(2)). This caused
the walker to read the idx field itself as the first descriptor index,
resulting in descriptor 1 being processed instead of descriptor 0.

### F4. QUEUE_READY missing from configure_queue
The test's `configure_queue()` function wrote all queue config registers
but forgot `QUEUE_READY=1`. Without this, the transport's `process_queue`
saw `q.ready=false` and returned immediately without processing any
requests.

### F5. Integer overflow in live_snapshot bandwidth estimate
`4096 * 1_000_000` as `i32` overflows. Fixed by using `u64` literals:
`(4096u64 * 1_000_000) / copy_us as u64 * 1000`.

## Results on c8i

- 243 tests pass (207 unit + 16 KVM + 11 integration + 3 lifecycle + 44 comprehensive + 5 virtio-blk)
- Clippy clean with kvm+boot
- Cold boot: 13ms p50
- CLI: 11 subcommands with aliases and help docs
- API: 10 operations over UDS
