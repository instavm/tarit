# Tarit test and benchmark suite

End-to-end tests and benchmarks for the whole platform (the `vmm` hypervisor and
the `taritd` orchestrator, talking over `tarit-proto`). These exercise real
microVMs, so they need a Linux host with KVM.

```
test/
  lib/preflight.sh   shared guards + helpers (KVM, root, deps, virt, build, fixtures)
  e2e.sh             single-node feature suite (both binaries) on one KVM host
  bench.sh           ComputeSDK-style Time-To-Interactive benchmark
  cluster.sh         ephemeral multi-node EC2 cluster + leader failover (needs AWS)
```

The individual feature scripts that `e2e.sh` runs live in `orch/tests/` (they are
the building blocks); `test/` is the documented entry point that adds preflight
guards and aggregation.

## Requirements

microVMs boot under KVM, so **`e2e.sh` and `bench.sh` need a Linux host with
`/dev/kvm`**:

- bare-metal Linux (best; gives the headline benchmark numbers), or
- a VM with nested virtualization enabled: AWS `*.metal` or c8i with nested virt,
  GCP `--enable-nested-virtualization`, or QEMU/KVM with host CPU passthrough.

They will **not** run on macOS/Windows, most containers, or cloud VMs without
nested virtualization. The runners detect this and tell you exactly what is
missing rather than failing deep in a boot. Nested virt works but pays a ~10x
KVM-exit tax, so the runner prints a warning and the numbers are directional.

Also required: a Rust toolchain (`cargo`), `curl`, `python3`, and root (microVM
networking, the jailer, and the OCI unpack need it). `cluster.sh` instead runs on
a workstation and needs the AWS CLI with credentials plus the cluster SSH key.

## Guest assets (kernel + rootfs)

The tests need a guest kernel (virtio-blk + virtio-vsock + serial) and a rootfs
with the exec agent baked in. Build both once from the repo root:

```sh
sudo make guest        # writes guest-assets/vmlinux and guest-assets/rootfs.ext4
```

The command verifies the pinned kernel SHA-256. If the release download fails,
it builds the same kernel from checksum-pinned source. The generated rootfs
includes `curl`, `ip`, `ping`, `nc`, `ss`, and `sysctl`, which the network
suites execute inside the guest.

Point the runners at them with `TARIT_KERNEL` and `TARIT_ROOTFS` (they default to
`/tmp/vmlinux.microvm` and `/tmp/vsock-rootfs.ext4`).

`vmm/ci/kvm-runner-bootstrap.sh` requires `VMM_TEST_KERNEL` and
`VMM_TEST_ROOTFS` to name readable paths inside the Colima VM. It checks both
before starting ignored KVM tests.

## 1. Single-node e2e

Runs the whole public surface of both binaries on one host and aggregates
pass/fail:

```sh
sudo TARIT_KERNEL=guest-assets/vmlinux TARIT_ROOTFS=guest-assets/rootfs.ext4 \
  test/e2e.sh
# or a subset:
sudo test/e2e.sh smoke cli ssh-pty
```

| suite         | what it proves |
| ------------- | -------------- |
| `smoke`       | taritd <-> vmm wire compat via `tarit-proto`: create + exec + PTY |
| `cli`         | the whole flow through the `taritd` CLI |
| `ssh-pty`     | SSH gateway + WebSocket PTY end to end |
| `image`       | build a golden rootfs from an OCI image |
| `warmpool`    | restore-from-golden warm-pool refill |
| `multitenant` | multi-tenant auth, RBAC, and tenant isolation |
| `net`         | per-VM tap/IP/nftables lifecycle |
| `lifecycle`   | graceful drain/reaper + 429 backpressure |
| `cpu-refill`  | CPU-isolated warm-pool refill |

A clean run ends with `RESULT: E2E_PASS`.

## 2. Benchmark (ComputeSDK-style TTI)

Time-To-Interactive measured per iteration from `create()` to the first
`node -v` that returns exit 0 (the ComputeSDK metric), in sequential, staggered,
and burst modes:

```sh
sudo test/bench.sh                  # warm-pool TTI (builds a node rootfs if needed)
sudo MODE=cold N=50 test/bench.sh   # cold-boot each VM instead of restore-from-golden
```

Results print as a table and are written under `bench-results/`.

## 3. Multi-node cluster (spins up + destroys EC2)

Provisions an ephemeral N-node cluster, forms it against a self-contained
Postgres, exercises create/exec across the cluster, kills the leader to prove
failover, brings it back, then destroys everything:

```sh
AMI=<kvm-capable-ami> SUBNET=<subnet-id> VPC=<vpc-id> KEYNAME=<key-pair> \
  ARTIFACT_HOST=<built-host-ip> NODES=4 test/cluster.sh
```

Runs from a workstation with AWS credentials and the cluster SSH key (`SSH_KEY`,
default `~/.ssh/<KEYNAME>.pem`). It does not need local KVM. The account-specific
inputs are required env vars: `AMI` (a KVM-capable image, e.g. an Ubuntu c8i
image with nested virt), `SUBNET`, `VPC`, `KEYNAME` (your EC2 key pair), and
`ARTIFACT_HOST` (a host with the repo built, to copy binaries and the kernel from).

**Safety.** The tests may run in an AWS account that is shared with other
workloads, so `cluster.sh` is surgical:

- every instance is tagged `Project=tarit-e2e` and `TaritE2ERun=<unique id>`;
- a per-run security group carries intra-cluster traffic (the shared SG is never
  modified);
- teardown (which runs on any exit, including timeout or Ctrl-C) terminates
  **only** instances whose `TaritE2ERun` tag matches this exact run, re-verifying
  each instance id before terminating, then deletes the per-run security group.
  It never matches by AMI, type, or name, and never touches any other instance.

A clean run ends with `RESULT: CLUSTER_E2E_PASS`.
