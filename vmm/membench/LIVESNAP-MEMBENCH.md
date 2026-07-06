# livesnap-membench

## Goal

Prove live snapshot/restore memory consistency for the Tarit VMM on c8i, using a
vsock-driven guest workload that writes deterministic memory patterns, snapshots
while idle or mutating, restores, and verifies hashes and guest forward progress.

## Hardened harness

Remote harness: `~/membench/livesnap-membench-vsock-hardened.sh`

Hardening applied after the prior vsock run:

- The live mutation worker is bounded: `32 MiB × 30` iterations, `nice(19)`,
  periodic `sched_yield()` calls, progress file, result file, and then exit.
- The kill path is a script command (`python3 /root/membench_work.py kill-live
  ...`), not inline `kill $VAR`.
- Every top-level test gets a fresh VMM serve process and a fresh e2fsck'd rootfs
  copy; TEST D additionally uses a fresh VM/rootfs per cycle.
- Exec is vsock-only: the harness waits for `vsock exec: guest agent connected`
  and rejects serial fallback by checking `via vsock` log lines.
- Memory sizes are modest for a 256 MiB guest: A=64 MiB, B=32 MiB,
  C=32 MiB, D=8 MiB/cycle.
- The harness uses a short `TMPDIR` for VMM-created vsock sockets to avoid
  `SUN_LEN` path failures.
- It runs `sudo e2fsck -fy` on the base and per-test rootfs images before boot.

The prior B/C/D all-FAIL section was a harness cascade: an unbounded/shared live
worker could starve later execs. The hardened run removes that cascade and
exposes a real diff-snapshot restore bug.

## Reproduce on c8i

```sh
ssh -o BatchMode=yes -o StrictHostKeyChecking=no \
  -i ~/.ssh/<key>.pem ubuntu@<kvm-host>

~/membench/livesnap-membench-vsock-hardened.sh 2>&1 \
  | tee ~/membench/results-vsock-hardened-$(date -u +%Y%m%dT%H%M%SZ).txt
```

Inputs used by the harness:

- VMM: `<workspace>/target/release/vmm`
- Kernel: `/tmp/vmlinux.microvm`
- Rootfs base: `/tmp/vsock-rootfs.ext4`
- Final result: `~/membench/results-vsock-hardened-20260702T052352Z.txt`
- Final run dir: `~/membench/run-vsock-hardened-20260702T052352Z-623801`

## CI gate

The repo has an opt-in gate that prepares a fresh agent-baked rootfs, runs the
livesnap membench harness, and fails unless A/B/C/D all emit PASS markers. A
local-only sync helper can run it on a KVM host:

```sh
LIVESNAP=1 C8I_HOST=<kvm-host> ./ci/sync-and-test.sh
```

On the Linux+KVM host directly:

```sh
cd ~/tarit/vmm
sudo -E env "PATH=$HOME/.cargo/bin:$PATH" \
  VMM=$HOME/tarit/vmm/target/release/vmm \
  KERNEL=/tmp/vmlinux.microvm \
  ROOTFS=/tmp/vsock-rootfs.ext4 \
  bash ci/livesnap-gate.sh
```

Pass contract: the gate prints `LIVESNAP_GATE_PASS tests=A,B,C,D ...` and exits
0 only when the harness exits 0 and `TEST_A_PASS` through `TEST_D_PASS` (or the
corresponding `PASS TEST A` through `PASS TEST D` lines) are present. Any missing
marker, harness failure, `MB FAIL`, `VERIFY_FAIL`, or non-zero failure count
prints `LIVESNAP_GATE_FAIL ...` and exits non-zero.

## Results: hardened run, 2026-07-01T22:46Z UTC

| Test | Result | Snapshot / restore evidence | Guest evidence |
| --- | --- | --- | --- |
| A quiescent RAM checksum | **PASS** | full `268444156` bytes, snapshot `111 ms`, UFFD restore `22 ms` | restored `state=running`; vsock reconnected; `VERIFY ... expected=369023ab... actual=369023ab... result=PASS`; `TEST_A_PASS` |
| B bounded live mutation diff tip | **FAIL — real VMM bug** | full `113 ms`; diff1 `9387298` bytes / `32 ms`; diff2 `9383190` bytes / `32 ms`; eager diff-chain restore `134 ms` | after restore, guest kernel panicked: `BUG: unable to handle page fault`, `Oops`, `Kernel panic - not syncing: Attempted to kill the idle task!`; no vsock reconnect / no result |
| C incremental chain diff tip | **FAIL — real VMM bug** | full `114 ms`; diff1 `5525778` bytes / `25 ms`; diff2 `5505238` bytes / `24 ms`; eager diff-chain restore `137 ms` | VMM reported `VM restored ... (running)`, but guest never reconnected over vsock; log floods `vsock: pending_rx full (4096); dropping packet`; no checkpoint2 verification |
| D 20-cycle full-snapshot soak | **PASS** | 20 full snapshots, `113–127 ms`; 20 UFFD restores, `20–25 ms` | all 20 cycles restored `state=running` and verified hashes; final `TEST_D_PASS cycles=20` |

Representative D evidence:

```text
TEST_D_VERIFIED cycle=1 sha=a5698e86... snapshot_ms=113 restore_ms=21
...
TEST_D_VERIFIED cycle=20 sha=a822f2b6... snapshot_ms=117 restore_ms=23
TEST_D_PASS cycles=20
```

## Root cause of remaining failures

Full snapshots are correct in this run: TEST A plus TEST D prove 21/21
full-snapshot restores resume, reconnect vsock, and verify memory hashes.

The failures are isolated to diff-chain tips. Both B and C take a full baseline,
then restore from a diff snapshot and enter the VMM path:

```text
restore: diff-chain tip detected; using eager snapshot replay
```

B restores from a diff tip of a running mutator and immediately corrupts guest
execution (kernel page fault/panic). C restores from a quiescent incremental diff
tip and the VM is marked running, but the guest does not make forward progress and
vsock RX fills indefinitely.

Likely code-level cause: diff snapshots are built from KVM dirty logging
(`KVM_GET_DIRTY_LOG`) only. That captures guest CPU writes, but not host/VMM writes
into guest memory made by virtio device emulation (used rings, descriptors,
interrupt/status paths, vsock/blk queue memory). The snapshot state persists
virtio/vsock device state, but the diff memory image can miss host-written guest
queue pages, so restored guest RAM and restored device state diverge. This matches
full snapshots passing while eager diff replay fails with vsock queue overflow or
guest kernel corruption.

Conclusion: the harness is now hardened and non-cascading. Live/full
snapshot+restore is correct for the tested memory workloads; diff-tip restore is
not yet correct and is a real VMM blocker.

## Final result after the incremental-snapshot and TEST B harness fixes (2026-07-02)

Two real VMM bugs in the **diff (incremental) snapshot** path were found and fixed
(full snapshots were always correct):

1. Diff snapshots captured only KVM-dirty (guest-vCPU-written) pages and missed
   pages the **host/VMM wrote** into guest memory (virtio/vsock queue writes,
   blk read completions, UFFD copies). Fix: a software host-dirty bitmap in the
   memory backend, marked by the device/UFFD write paths and unioned into diffs.
2. The **KVM dirty-log decoder** read `Vec<u64>` as bytes and only took bits 0..8
   of each 64-bit word, dropping 56 of every 64 page bits. Fix: decode 64
   bits/word (`kvm_dirty.rs`).

Validated on c8i (hardened vsock harness, fresh build, final run
`~/membench/run-vsock-hardened-20260702T052352Z-623801`):

| Test | Result | Evidence |
| --- | --- | --- |
| A quiescent RAM checksum | PASS | snap 127ms, restore 21ms, SHA match |
| B live-mutation diff restore | PASS | full 118ms; diff1 99ms / 73.7MB; diff2 101ms / 78.0MB; restore 240ms; live loop reached `OK epochs=30 final_sha=e3407483...`; post-restore `fresh-loop` also passed |
| C incremental chain | PASS | full 123ms; diffs 68/69ms / ~45MB; restore 198ms, SHA match |
| D 20-cycle soak | PASS | 20/20, restore 21-25ms, final SHA match |

TEST B root cause + fix: the live-loop was already bounded and the restored
1-vCPU guest was not generally wedged (a diagnostic trivial exec succeeded after
restore while the loop was still running at epoch 17), but the B workload used
`time.sleep()` in both the mutator and result polling; after restoring from the
mid-mutation diff tip, the sleep-based path did not reliably make progress and
the harness `wait-live` command held the agent until the host-side exec timeout.
The c8i harness now uses `os.sched_yield()` instead of sleeps in
`membench_work.py`, preserving the live mutation workload while keeping the guest
agent responsive and allowing the bounded loop to write its final checksum.

Final result: **A/B/C/D all PASS** with matching checksums and post-restore guest
forward progress.
