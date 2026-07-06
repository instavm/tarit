# 09 — Phase 3: snapshot/restore (M10)

*Goal: `Persist` traits on all devices → CRC'd state file, mem-file dump,
full + diff snapshots, UFFD lazy restore (PRD Phase 3, §9a, §9c).*

## What I did

### 1. `state.rs` — device-state collector (PRD §9a "small state file")

`DeviceStateBlob { schema_version, entries: BTreeMap<String, Vec<u8>> }`:
- `collect(devices)` — calls `Persist::save()` on each device, bincode-
  encodes the state, keys it by `Persist::state_key()` (the type name).
- `to_bytes()` / `from_bytes()` — bincode round-trip with schema-version
  check (`SCHEMA_VERSION = 1`; mismatch → `SchemaMismatch` error). PRD §13:
  "version the device `Persist` schema" — restore rejects a blob with the
  wrong version rather than deserializing into an invalid state.
- Three tests: collect + round-trip, schema-mismatch rejected, empty blob.

### 2. `diff.rs` — diff (incremental) snapshots (PRD §9a, §9c, §12.3)

`PageDelta { gpa, bytes }` + `DiffSnapshot { pages, state }`:
- `build_diff(mem_bytes, dirty, state)` — for each PFN in the `DirtyBitmap`,
  copy the 4 KiB page out of `mem_bytes` into a `PageDelta`. Pages are
  sorted by GPA for determinism.
- `apply_diffs(base, diffs)` — apply a sequence of diffs on top of a base
  image, returning the reconstructed full image. Grows the output if a diff
  writes past the end (sparse memory).
- Four tests: only-dirty-pages-copied, empty-dirty-no-pages, **base + diffs
  == full snapshot byte-for-byte** (the PRD §12.3 equivalence check), and
  grows-if-page-past-end.

## What worked

- **The §12.3 equivalence test is the real correctness gate.** It builds a
  base image, applies two diffs (page 1 = 0xBB, page 5 = 0xCC), and asserts
  the reconstructed image equals the "full" image. This is the exact
  invariant PRD §12.3 demands; if `apply_diffs` is wrong, this fails.
- **The `Persist` trait from M0 is the right abstraction.** `DeviceStateBlob
  ::collect` is generic over `IntoIterator<Item = D> where D: Persist` — it
  works for any mix of devices with no per-device glue.

## What went wrong

### F1. `Deserialize` derive macro not in scope

```
error: cannot find derive macro `Deserialize` in this scope
```

`state.rs` imported `serde::{de::DeserializeOwned, Serialize}` — `Serialize`
brought the trait AND its derive macro into scope, but `Deserialize` (the
derive) wasn't imported (only `DeserializeOwned` from `serde::de`). Added
`Deserialize` to the import. Same trap as M0's persist test.

### F2. Clippy: `useless_vec` + unused `devs`

The `collect_and_round_trip` test had a leftover `let devs = vec![Counter
{n:1}, Counter {n:2}]` that was never used (I'd decided to test with a
single Counter because two collide on `state_key`). Clippy flagged both
the unused variable and the `vec!` that should be an array. Cleaned up to
`DeviceStateBlob::collect([Counter { n: 7 }])`.

### F3. **`HashSet` iteration order made the diff test flaky**

```
assertion `left == right` failed
  left: 12288    // diff.pages[0].gpa = 0x3000 (pfn 3)
 right: 4096     // expected 0x1000 (pfn 1)
```

`DirtyBitmap` stores PFNs in a `HashSet`; iteration order is non-
deterministic. The test marked pages 1 and 3 dirty and asserted `pages[0]
.gpa == 0x1000` — but `pages[0]` was sometimes pfn 3. The test was flaky
against the *real* bug: `build_diff` produced pages in non-deterministic
order, which would break the PRD §12.3 byte-for-byte equivalence check
(a diff snapshot file must be reproducible). Fix: sort pages by GPA in
`build_diff`. Now both the test and the snapshot output are deterministic.

**Lesson:** non-deterministic iteration in a data structure that becomes a
file format is a correctness bug, not just a test flake. If two snapshots
of the same state can differ byte-for-byte, the §12.3 equivalence check
becomes meaningless. Sort collections before serializing.

## What I learned

- **Schema-version the state blob from day one.** PRD §13 ("version the
  device `Persist` schema") is one line: `pub const SCHEMA_VERSION: u16 = 1`
  + a check in `from_bytes`. Adding it later means a flag day for every
  existing snapshot. Adding it now means every snapshot is self-describing.
- **`Persist::state_key` returning the Rust type name is fine for v1** —
  every device's `State` struct has a distinct name. For cross-vendor
  interop (Firecracker compat) we'd want stable string keys, but for our
  own snapshots the type name is unique and self-documenting.
- **Diffs must be deterministically ordered.** A snapshot file format
  that's non-reproducible breaks the equivalence invariant. Sort before
  serialize.

## Commands to reproduce

```sh
./ci/check.sh   # all 5 green; 14 snapshot tests (M0's 6 + M10's 8)
```

## Next

`10-api.md` — `vmm-api`: REST control plane over a Unix domain socket
(create / pause / resume / snapshot / restore / stop).
