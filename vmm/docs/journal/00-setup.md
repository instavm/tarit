# 00 — Setup

*Goal: a working Linux+KVM environment for building and integration-testing
the VMM, plus a Rust workspace skeleton.*

## The first wall: no KVM on macOS

The very first thing I checked was whether I could run the thing at all. The
PRD is explicit (§3, §12.0): KVM is required, and "KVM-capable Linux hosts."
This dev machine is **macOS on Apple Silicon (M3 Pro)**. Verified:

```
$ ls /dev/kvm
ls: /dev/kvm: No such file or directory
```

`kvm-ioctls` / `kvm-bindings` have no HVF backend — they are KVM-only. So
nothing in this VMM can run on this host directly. The approved plan: install
**Colima + QEMU** to bring up a Linux VM on the Mac for KVM builds and
integration tests.

## Toolchain on the host

```
$ rustc --version
rustc 1.95.0 (59807616e 2026-04-14)
$ cargo --version
cargo 1.95.0 (f2d3ce0bd 2026-03-21)
$ git --version
git version 2.50.1 (Apple Git-155)
```

Rust 1.95+ is the stable toolchain the PRD requires.

## What I did

### 1. Installed Colima + QEMU via Homebrew

```
brew install colima qemu lima-additional-guestagents
```

`lima-additional-guestagents` is required because I forced `--arch x86_64` on
an arm64 host — without it, Colima can't find the x86_64 guest agent binary
and fails with:

```
guest agent binary could not be found for Linux-x86_64
```

### 2. Started an x86_64 Linux VM

```
colima start vmm-kvm --cpu 4 --memory 8 --disk 50 --vm-type qemu --arch x86_64 --runtime none
```

`--runtime none` because we don't need docker, just the Linux VM. (Without
it, Colima tries to start docker and aborts the whole start if the docker
context switch fails — leaving the VM in a half-started "not running" state
even though it's actually up. Took me a `colima delete --force` and recreate
to recover.)

### 3. The VM booted, and `/dev/kvm` existed

```
$ limactl shell colima-vmm-kvm uname -a
Linux colima-vmm-kvm 6.8.0-117-generic ... x86_64 x86_64 x86_64 GNU/Linux
$ ls -l /dev/kvm
crw-rw---- 1 root kvm 10, 232 ... /dev/kvm
$ grep -cE 'vmx|svm' /proc/cpuinfo
4
```

Linux 6.8, `/dev/kvm` present, `vmx` in CPUinfo. Looks great on paper.

## What went wrong

### F1. `colima ssh` reports "colima not running" even when the VM is up

After the "READY" line, `colima ssh vmm-kvm -- ...` failed with
`colima not running`. Root cause: Colima's status check goes through the
docker runtime, and with `--runtime none` the status is always "not running"
from Colima's perspective. Workaround: bypass Colima and call lima directly
with `LIMA_HOME="$HOME/.colima/_lima" limactl shell colima-vmm-kvm ...`.
That worked.

### F2. **The VM is TCG-emulated, not nested-virt — KVM is unusably slow**

The smoking gun is the qemu command line:

```
qemu-system-x86_64 ... -machine q35,vmport=off -accel tcg,thread=multi ...
```

`-accel tcg` means **pure software emulation**, not HVF (Apple's
Hypervisor.framework) and definitely not nested KVM. On an M3 Pro emulating
x86_64, this is *glacial*. Apt update + rustup install inside the VM ran
past 10 minutes without producing output, and eventually the SSH daemon
itself started timing out handshakes:

```
$ ssh ... 'echo ALIVE'
kex_exchange_identification: read: Connection reset by peer
```

The guest agent died (`connection refused` on its socket), and the VM was
effectively wedged. This is exactly the caveat I flagged in the plan:
**"KVM-in-KVM on M-series Macs via QEMU runs under TCG; functional CI may
still need a real Linux host."** Confirmed by direct experiment.

Even if apt and rustup had finished, **nested KVM under TCG does not work**.
TCG is pure emulation; there's no hardware virtualization to expose to the
guest. `/dev/kvm` exists in the guest only because QEMU's `-cpu max` exposes
the `vmx` flag, but opening `/dev/kvm` and trying to actually accelerate a
guest would fail. The PRD's perf numbers (cold boot <125 ms, etc.) would be
meaningless under TCG anyway — you'd be measuring emulation overhead.

### F3. The Colima VM is a dead end for this project

I tore it down: `colima delete vmm-kvm --force`. The brew install of Colima +
QEMU + lima-additional-guestagents (~1.3 GB on disk) stays, in case a future
contributor on an Intel Mac wants to try (where HVF *can* expose nested
virt). On this M3 Pro it's not viable.

## The pivot: cross-compile type-checking from macOS

The KVM-behind-a-feature design (landed in M0) means I don't need to *run*
KVM to keep developing — I need to **type-check** the KVM code paths before
each commit, and defer real execution to a real Linux+KVM host the user
provides later.

### Setup

```
rustup target add x86_64-unknown-linux-gnu
```

`.cargo/config.toml` sets a placeholder linker (`x86_64-unknown-linux-gnu-gcc`)
that we never actually invoke — `cargo check` doesn't link, so the linker
binary is never called. To make `cargo check` not even look for it, the
check script sets `CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=true` (the
`true` command, which exists and returns 0; cargo only runs it for actual
links, which `check` skips).

### The dev loop (now in `ci/check.sh`)

```sh
./ci/check.sh
```

Expanded:

```sh
cargo check --workspace --all-targets                          # native (macOS)
cargo test --workspace                                         # 28 unit tests (macOS)
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=true \
  cargo check --workspace --target x86_64-unknown-linux-gnu --features vmm-core/kvm
```

All five steps pass. The last one is the key: it type-checks every
`cfg(target_os = "linux")` block and every `#[cfg(feature = "kvm")]` block
against the *real* `kvm-ioctls` / `kvm-bindings` crates, so I catch API
mismatches in the KVM code before each commit even though I can't run it.

### What this does NOT give me

- It does not run any KVM integration test (boot, snapshot, migration).
- It does not prove the perf targets.
- It does not catch runtime issues (kernel panics, dirty-ring races, UFFD
  semantics).

Those need a real Linux+KVM host — either bare metal (Intel/AMD `.metal` or
a Graviton), an EC2 nested-virt C8i/M8i/R8i instance (PRD §12.0), or an
Intel Mac with HVF. The plan is: I keep coding on macOS with the cross-check
as the safety net, and the user points me at a real Linux+KVM box when we
hit M6 (the boot spike) and beyond.

## What I learned

- **Read the qemu command line before trusting a VM.** "It booted and
  `/dev/kvm` exists" is not the same as "KVM works." `-accel tcg` was right
  there in `ps`; I should have checked it before trying to install anything.
- **M-series Macs cannot do nested x86_64 KVM.** This isn't a Colima bug;
  it's the hardware. HVF can accelerate an arm64 guest, but for x86_64
  guests we fall back to TCG, and TCG can't expose nested virt. Intel Macs
  *can* (HVF exposes vmx), but Apple Silicon can't.
- **The "KVM behind a feature" convention from M0 is what makes the pivot
  painless.** Because every crate gates KVM behind `cfg`, I lost zero code
  and zero tests by abandoning the Colima path. The cross-check keeps the
  KVM code honest.
- **`cargo check --target` without a real linker works** because `check`
  doesn't link — you can type-check any target from any host with just the
  rust-std component for that target installed. `CARGO_TARGET_*_LINKER=true`
  is a clean way to silence the "linker not found" error for check-only
  workflows.

## Next

M0 (repo init) is already committed. See [01-repo-init.md](01-repo-init.md)
for that — including the full failure log (19 compile/test failures before
it went green). Then we proceed to M1 (fork vmm-reference) and M2 (loader).
