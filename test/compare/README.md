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
for the harness's own page-cache writeback.

Debugging aids: `VMBENCH_TARIT_VERBOSE=1|2` raises tarit's log level,
`VMBENCH_GDB=1` runs tarit under gdb and dumps every thread's stack when a VM
goes exec-dead, and `VMBENCH_STRACE=openat` straces tarit.

Publish results with the commit, host CPU and kernel, guest kernel, rootfs
identity, and the raw JSON.
