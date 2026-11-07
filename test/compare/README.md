# Tarit vs Firecracker

`vmbench.py` runs the same guest under both VMMs and measures each through
its real control path: tarit through the `vmm serve` UDS API, Firecracker
through its HTTP API and hybrid vsock. Both boot the same kernel, rootfs,
command line, vCPU count and memory size, and both run the same guest agent
(`vmm-agent`, exec protocol v2), so exec latency compares like for like.

## Scenarios

| Scenario   | Measures |
|------------|----------|
| `cold`     | spawn VMM, boot, first successful exec; VMM PSS |
| `exec`     | round trip of `true`, 4 KiB of output, and `python3` on a running VM |
| `snapshot` | full snapshot of a booted VM, then restore to first exec, repeatedly |
| `chain`    | the agent loop: restore the latest checkpoint, run a step that adds state, take an incremental snapshot, repeat; every step checks all earlier state survived |
| `fanout`   | fork N clones at once from a stopped parent's snapshot; each must see the parent's disk marker and no sibling's file |
| `burst`    | boot N VMs at once; time until every one has run an exec |
| `reliab`   | rounds of concurrent boots; counts VMs whose first exec never succeeds |

For `chain`, Firecracker takes a `Diff` snapshot and merges it into the
previous memory file with `rebase-snap`, because it cannot load a diff chain.
The in-place merge is its cheapest option and overwrites older checkpoints.

For `fanout`, Firecracker clones reopen the disk and vsock paths baked into
the snapshot, so each clone gets its own copy of the parent's disk (timed as
disk prep) and runs in an unprivileged user and mount namespace that
bind-mounts its directory over those paths (`unshare -Urm`).

## Running

```sh
python3 test/compare/vmbench.py \
  --tarit vmm/target/release/vmm \
  --firecracker /path/to/firecracker-v1.17.0-x86_64 \
  --kernel guest-assets/vmlinux --rootfs guest-assets/rootfs.ext4 \
  --work /var/tmp/vmbench --out results.json \
  --scenarios cold,exec,snapshot,chain,burst
```

The rootfs must use `vmm-agent` as init. Socket paths are created under
`/run/user/$UID/vmbench`. Firecracker's `rebase-snap` must sit next to its
binary. Each disk copy is written back before timing so that neither VMM pays
for the harness's own page-cache writeback. Snapshot files that `vmm snapshot` hands back belong to
the caller; the harness removes them after each scenario.

Both VMMs give the guest the same durability. tarit always passes a guest flush
to the host; Firecracker's default drive `cache_type` (`Unsafe`) ignores it, so
a guest fsync there is not durable. Both harnesses therefore run Firecracker
with `cache_type: Writeback` and record that in the results file
(`firecracker_cache_type`); set `VMBENCH_FC_CACHE=Unsafe` to measure the
default instead. Snapshots are compared the same way: tarit syncs a snapshot
to disk before returning it, while Firecracker leaves its snapshot files in
the page cache, where a host crash loses them. The harnesses sync
Firecracker's snapshot files inside the timed window
(`firecracker_snapshots_synced`; `VMBENCH_FC_SYNC_SNAPSHOTS=0` measures
without). To compare disk work fairly, keep `--work` and `TMPDIR` on a
filesystem with reflink (XFS or btrfs) so that Firecracker's per-VM disk copies
are clones, as tarit's are.

Debugging aids: `VMBENCH_TARIT_VERBOSE=1|2` raises tarit's log level,
`VMBENCH_GDB=1` runs tarit under gdb and dumps every thread's stack when a VM
goes exec-dead, and `VMBENCH_STRACE=openat` straces tarit with timestamps and
per-call durations (`VMBENCH_STRACE_K=1` adds stack traces).

Publish results with the commit, host CPU and kernel, guest kernel, rootfs
identity, and the raw JSON.

## Agent workloads (`agentbench.py`)

`agentbench.py` runs agent-sandbox workloads on both VMMs and checks that
they behave correctly, not just quickly. The scenarios follow how hosted agent
sandboxes are used: coding sessions that install packages and run tests,
checkpoint and resume of long-lived sandboxes, N-way forks for parallel
attempts, bursts of short code-interpreter calls, package-manager disk work,
and sustained load.

| Scenario  | What runs | What must hold |
|-----------|-----------|----------------|
| `session` | HTTPS egress, `pip install`, `git clone`, a project's test suite, a C build, a server in the guest, 100 short commands | every step succeeds; the host reaches the guest's server |
| `resume`  | 10 cycles of snapshot, stop, restore with a stateful process, an HTTP server and disk changes | in-memory and disk state carry over, the server stays reachable both ways, the guest clock is within 1 s of the host |
| `fork`    | 8 clones of one warmed snapshot started at once, each moved to its own address | clones see the parent's disk but not each other's writes; boot ID, kernel RNG output and new UUIDs differ per clone; each clone is reachable at its own address |
| `burst`   | 10 sandboxes restored at once, 30 commands each | every command returns the right answer; nothing is left behind afterwards |
| `disk`    | 20k small files, tar round trip, a 256 MiB write with fsync, a cache drop, a generated C program | contents survive the cache drop and the round trip; the program is correct |
| `net`     | three 256 MiB downloads from a static server on the host gateway (median throughput) | every download's SHA-256 matches the served file |
| `soak`    | an exec every few milliseconds for 2 minutes with a pause and resume every 5 s | no failures; VMM file descriptors, threads and RSS stay flat |

Networking is real and needs no root. The harness creates a user and network
namespace (the lab), gives it internet access through `slirp4netns`, and
NATs every VM's TAP through it. Slot N uses `tapN`, host `172.16.N.1/24`,
guest `172.16.N.2`; guests resolve names through slirp's resolver via
`tarit.dns=` on the kernel command line. A small HTTP server in the lab
(`agentbench_http.py`) records which guest called, which proves guest-to-host
traffic per clone.

Both VMMs use the same image, built from
[`agent-image/Dockerfile`](agent-image/Dockerfile) (Python, git, a C toolchain)
and converted with tarit's converter:

```sh
docker build -t tarit-agentbench:1 test/compare/agent-image
target/release/vmm pull docker-daemon:tarit-agentbench:1 --output /var/tmp/agentbench/agent.ext4 \
  --size 3072 --agent guest/agent/vmm-agent
python3 test/compare/agentbench.py \
  --tarit vmm/target/release/vmm \
  --firecracker /path/to/firecracker-v1.17.0-x86_64 \
  --kernel guest-assets/vmlinux --rootfs /var/tmp/agentbench/agent.ext4 \
  --out agentbench.json
```

Requirements: `slirp4netns`, `nft`, `nsenter`/`unshare`, and unprivileged
user namespaces. Each VMM is driven the way its documentation describes:
tarit restores clones with network overrides, `repair_guest_network`, and
copy-on-write disk templates; Firecracker restores with `network_overrides`
and `vsock_override`, gets a private copy of the disk (timed as disk prep),
and has its guest address changed from inside the guest.
