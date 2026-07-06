# Bare-metal benchmarks

Date: 2026-07-02, 2026-07-02T07:21:23Z to 2026-07-02T07:44:03Z UTC.

Host: AWS EC2 c8i.metal-48xl in us-east-1, a c8i bare-metal instance launched from a current Ubuntu AMI. Hardware was bare metal: 192 vCPUs, 377 GB RAM, Intel Xeon 6975P-C, no hypervisor flag in lscpu, /dev/kvm present. Public and private IPs are intentionally omitted.

The instance was not terminated by this run. The 3-node taritd cluster was skipped because the single-node warm-pool run exposed a 5 second exec fallback and the priority was to finish and record items 1-5 before the shutdown window.

## Results

| metric | bare-metal (this run) | prior nested (c8i.xlarge) | notes |
|---|---:|---:|---|
| VMM cold create return, minimal kernel + vsock | p50 7.971 ms / p95 10.148 ms | p50 24 ms | 9 runs, 5 ms poll, `/tmp/vmlinux.minimal.vsock` + `/tmp/vsock-rootfs.ext4`. |
| VMM cold create to first exec, minimal kernel + vsock | p50 218.657 ms / p95 220.846 ms | p50 340 ms | Better than nested, but not the projected 34 ms. |
| VMM restore to running, UFFD full snapshot | p50 2.934 ms / p95 4.176 ms | about 0.84 ms | 100 restores from a warmed `node -v` snapshot. `ci/restore-roundtrip.sh` also confirmed running restore in 3.976 ms and 65536 UFFD pages served. |
| VMM restore to `node -v` TTI | p50 82.919 ms / p95 86.431 ms | p50 81 ms / p95 84 ms | 100/100 correct, `/tmp/bench-node-rootfs.ext4`, node v20.18.1. |
| VMM full snapshot, 256 MiB | p50 60.368 ms / p95 87.919 ms | about 117 ms | Five full snapshots. File size 268444242 bytes. A warmed node base snapshot took 88.369 ms. |
| Idle running VM CPU | 0.0000% over 5 s | 0% | Sampled from `/proc/<pid>/stat` after guest was ready. |
| VMM RSS, 256 MiB VM | 46.6 MiB | about 45 MiB | VmRSS 47672 KiB. |
| Running exec round-trip, vsock | p50 0.597 ms / p95 1.080 ms | about 9 ms | 50 `true` execs against an already-running VM. |
| taritd warm-pool create only, `node -v` | p50 12.304 ms / p95 15.053 ms | about 15 ms create-only | 20/20 warm-pool handouts, no cold starts. |
| taritd warm-pool create to `node -v` TTI | p50 5124.695 ms / p95 5249.390 ms | sequential p50 363 ms / p95 647 ms, best warm 85 ms | TTI is dominated by a 5 s UDS exec failure and one-shot fallback in taritd, not by create. |
| taritd warm-pool create only, `echo ok` | p50 8.401 ms / p95 9.012 ms | no direct nested echo baseline | 20/20 warm-pool handouts, no cold starts. |
| taritd warm-pool create to `echo ok` TTI | p50 5120.239 ms / p95 5130.257 ms | no direct nested echo baseline | Same 5 s exec fallback as `node -v`. |

## Commands used

```sh
# Host preflight and requested prep. The remote .git metadata was incomplete, so
# the fetch/reset path failed before build. The source was then refreshed from
# local HEAD with git archive, shown below.
ssh -i ~/.ssh/<key>.pem -o BatchMode=yes -o StrictHostKeyChecking=no \
  ubuntu@<kvm-host> \
  'date -u; lscpu | grep -E "Model name|CPU\(s\):|Thread\(s\)|Core\(s\)|Socket\(s\)"; lscpu | grep Hypervisor || true; test -e /dev/kvm && echo KVM_PRESENT'

ssh -i ~/.ssh/<key>.pem -o BatchMode=yes -o StrictHostKeyChecking=no \
  ubuntu@<kvm-host> \
  'mkdir -p $HOME/tarit'

git -C ~/tarit archive --format=tar HEAD | \
  ssh -i ~/.ssh/<key>.pem -o BatchMode=yes -o StrictHostKeyChecking=no \
  ubuntu@<kvm-host> 'tar -xf - -C $HOME/tarit'

ssh -i ~/.ssh/<key>.pem -o BatchMode=yes -o StrictHostKeyChecking=no \
  ubuntu@<kvm-host> \
  'cd $HOME/tarit/vmm && cargo build --release --features boot; cd $HOME/tarit/orch && cargo build --release -p taritd -p tarit-bench'

ssh -i ~/.ssh/<key>.pem -o BatchMode=yes -o StrictHostKeyChecking=no \
  ubuntu@<kvm-host> \
  'sudo e2fsck -fy /tmp/vsock-rootfs.ext4; sudo e2fsck -fy /tmp/bench-node-rootfs.ext4'

# VMM benchmark harness written to $HOME/metal-bench/vmm_bench.py.
ssh -i ~/.ssh/<key>.pem -o BatchMode=yes -o StrictHostKeyChecking=no \
  ubuntu@<kvm-host> \
  'sudo python3 $HOME/metal-bench/vmm_bench.py'

# Required restore-roundtrip script.
ssh -i ~/.ssh/<key>.pem -o BatchMode=yes -o StrictHostKeyChecking=no \
  ubuntu@<kvm-host> \
  'cd $HOME/tarit/vmm && sudo env VMM=$HOME/tarit/vmm/target/release/vmm KERNEL=/tmp/vmlinux.microvm ROOTFS=/tmp/bench-node-rootfs.ext4 bash ci/restore-roundtrip.sh'

# taritd warm-pool harness written to $HOME/metal-bench/orch_bench_small.py.
ssh -i ~/.ssh/<key>.pem -o BatchMode=yes -o StrictHostKeyChecking=no \
  ubuntu@<kvm-host> \
  'sudo python3 $HOME/metal-bench/orch_bench_small.py'
```

The raw result files were left on the host at `$HOME/metal-bench/vmm-results.json` and `$HOME/metal-bench/orch-results.json`, and copied locally as untracked scratch files during documentation.
