# 08 — Phase 2: netns + egress + jailer + seccomp (M8/M9)

*Goal: host-enforced default-deny egress + process confinement
(PRD Phase 2, §8, §10).*

## What I did

### M8 — `vmm-net`

#### `nft_compiler.rs` — egress policy → nftables rules (PRD §8, §12.1)

`compile_to_nft(&policy, chain)` and `compile_table(&policy)` turn an
`EgressPolicy` into a list of `add table`/`add chain`/`add rule` nftables
statements. The chain is `inet vmm vmm_egress` with `hook output policy drop`
(default-deny). Each allow rule renders as
`ip daddr <cidr> <proto> dport <port> accept` (or `meta l4proto <proto>
accept` for proto-specific any-port, or bare `accept` for allow-all).
Five tests cover: deny-all emits only table+chain, tcp port rule, allow-all
bare accept, udp dport, full-table shape.

This is the PRD §12.1 "Egress policy compiler: allowlist spec → expected
nftables/eBPF program (table-driven)" deliverable, host-agnostic and unit-
testable without `nft` or root.

#### `netns.rs` — per-VM network namespace (PRD §8)

`NetNs { name, fd }` with `create()` + `enter()`. Linux: holds the
`/var/run/netns/<name>` bind-mount fd (full `unshare(CLONE_NEWNET)` + bind-
mount wiring lands with the real tap backend). Non-Linux: stub.

### M9 — `vmm-jailer`

#### `profile.rs` — seccomp profile set + audit (PRD §10, §12.6)

`VmmSeccompProfiles::minimal()` returns the vCPU + device thread profiles.
`audit_profile(&profile)` returns the list of *dangerous* syscalls in a
profile's allowlist (execve/fork/ptrace/mount/chroot/open/setuid/bpf/...).
The PRD §10 "tight syscall filter" intent made concrete: a profile passing
audit can't fork, exec, ptrace, mount, or open-by-path after seccomp is
installed. Five tests: minimal profiles non-empty (Linux), for_thread
dispatch, audit rejects execve/ptrace/mount, audit passes clean profile,
serialize round-trip (for the migration channel).

## What worked

- **String-building the nftables rules is genuinely testable.** The
  compiler is pure functions of `EgressRule` → nft match-expr strings. No
  `nft` binary, no root, no netns needed. The PRD §12.1 "table-driven"
  requirement is exactly this: golden expected-output per input.
- **The seccomp audit denylist is the PRD §10 intent encoded as data.**
  Listing "execve, ptrace, mount, ..." as denied names is more readable
  than BPF bytecode and catches the obvious mistakes (a profile that
  allows `execve` is a jail-break).

## What went wrong

### F1. Clippy: `useless_format` on a string with no args

```
error: useless use of `format!`
   | out.push(format!("add chain ... {{ }}"));
```

`format!` with no args is a pointless heap alloc; clippy wants a `&str`
literal. Fixed to `.into()` on the literal.

### F2. Dead-code warning on `NetNs::fd` (Linux cross-check)

The scaffold `fd` field isn't read until the real tap backend lands. Added
`#[allow(dead_code)]` with a "Used in M8's real tap backend" comment.

### F3. `minimal_profiles_are_nonempty` failed on macOS

The non-Linux `SeccompProfile::vcpu()` stub returns an empty allowlist
(seccomp is Linux-only), so asserting non-empty on macOS failed. Fixed by
gating the assertion: `#[cfg(target_os = "linux")]` asserts non-empty;
non-Linux just constructs the stub. **Lesson:** when a test's data is
platform-gated, the assertion has to be too. The `cfg` is the source of
truth for whether the data exists.

## What I learned

- **Default-deny as a chain policy, not a rule, is the nftables idiom.**
  `add chain ... policy drop` makes the drop implicit; allow rules just
  `accept`. A final `drop` rule would be redundant and reorder-sensitive.
- **A syscall denylist as data is more reviewable than a BPF filter.**
  The PRD §10 "tight syscall filter" requirement is hard to verify by
  reading bytecode; an `audit_profile` returning the list of dangerous
  syscalls in a profile is a code-review tool, not just a test.
- **Platform-gated data needs platform-gated tests.** A test asserting
  `profile.allow.len() > 0` is Linux-only; running it on macOS fails not
  because the code is wrong but because the data doesn't exist there.

## Commands to reproduce

```sh
./ci/check.sh   # all 5 green; 11 net tests + 5 jailer tests
```

## Next

`09-snapshot.md` — Phase 3: `Persist` traits on all devices, CRC'd state
file, mem-file dump, UFFD lazy restore.
