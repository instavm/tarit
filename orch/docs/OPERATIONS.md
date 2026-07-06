# Operations guide

This guide covers building, running, clustering, load balancing, deployment helpers, security, and troubleshooting for `taritd`.

## Requirements

- Linux host with KVM for actually running microVMs.
- Rust stable toolchain.
- The rust-vmm based `vmm` binary from the sibling VMM repository.
- Guest kernel and optional rootfs image readable by `taritd`.
- `umoci`, `skopeo`, and `e2fsck` on hosts that run `taritd image build`.
- PostgreSQL for distributed cluster mode.
- `CAP_NET_ADMIN` or root only if `TARIT_ENABLE_NET=true`.

## Build

Build the orchestrator:

```sh
cd /path/to/tarit/orch
cargo build --release -p taritd
```

Build the VMM binary used by `TARIT_VMM_BIN`:

```sh
cd /path/to/tarit/vmm
cargo build --release --features "vmm-core/kvm vmm-core/boot vmm-memory-backend/kvm"
```

The deploy script uses the command above when the `vmm` binary is not found on `PATH`.

## Run one node

```sh
cd /path/to/tarit/orch

export TARIT_API_KEY='replace-with-a-long-random-token'
export TARIT_LISTEN='0.0.0.0:8080'
export TARIT_HOST_ID="$(hostname)"
export TARIT_RPC_ADDR='http://127.0.0.1:8080'
export TARIT_VMM_BIN='/path/to/tarit/vmm/target/release/vmm'
export TARIT_KERNEL='/var/lib/taritd/vmlinux.microvm'
export TARIT_ROOTFS='/var/lib/taritd/rootfs.ext4'
export TARIT_SOCKET_DIR="$HOME/.taritd/sockets"
export TARIT_DB="$HOME/.taritd/fleet.db"
export TARIT_IMAGES_DIR="$HOME/.taritd/images"
export RUST_LOG='taritd=info,tower_http=info'

./target/release/taritd
```

Health and API checks:

```sh
curl -sf http://127.0.0.1:8080/health
curl -sf -H "X-API-Key: $TARIT_API_KEY" http://127.0.0.1:8080/v1/cluster
```

## Image registry operations

Build immutable rootfs images from OCI refs on the host that has the VMM binary
and OCI tooling:

```sh
taritd image build --oci node:20-slim --name node20
taritd image ls
```

The build command writes a temporary ext4 under `TARIT_IMAGES_DIR`, runs:

```sh
$TARIT_VMM_BIN pull --agent node:20-slim <out.ext4>
e2fsck -fy <out.ext4>
```

then atomically moves the file into place and registers `name:tag`, rootfs path,
size, source ref, and creation time in `TARIT_DB`. Create VMs from a registered
image:

```sh
taritd vm create --image node20 --vcpus 1 --memory-mib 256
taritd exec <vm-id> 'node -v'
```

Remove unused records/files with `taritd image rm node20`. Garbage collection is
age and pattern gated and skips images referenced by active local VMs or
warm-pool classes:

```sh
taritd image gc --older-than-days 7 --dry-run
taritd image gc --older-than-days 30 --pattern 'node:*'
```

## Run a three-node cluster

All nodes share:

```sh
export TARIT_API_KEY='replace-with-a-long-random-token'
export TARIT_PEER_SECRET='replace-with-a-long-random-peer-secret'
export TARIT_DATABASE_URL='postgres://taritd:password@postgres.example:5432/taritd?sslmode=require'
export TARIT_VMM_BIN='/opt/taritd/bin/vmm'
export TARIT_KERNEL='/var/lib/taritd/vmlinux.microvm'
export TARIT_ROOTFS='/var/lib/taritd/rootfs.ext4'
export TARIT_MAX_VMS='32'
export TARIT_MAX_MEMORY_MIB='65536'
export RUST_LOG='taritd=info,tower_http=info'
```

Each node must differ:

| Node | Required differences |
| --- | --- |
| A | `TARIT_HOST_ID=node-a`, `TARIT_LISTEN=0.0.0.0:8080`, `TARIT_RPC_ADDR=http://10.0.1.10:8080`, `TARIT_SOCKET_DIR=$HOME/.taritd/node-a/sockets`, `TARIT_DB=$HOME/.taritd/node-a/fleet.db` |
| B | `TARIT_HOST_ID=node-b`, `TARIT_LISTEN=0.0.0.0:8080`, `TARIT_RPC_ADDR=http://10.0.1.11:8080`, `TARIT_SOCKET_DIR=$HOME/.taritd/node-b/sockets`, `TARIT_DB=$HOME/.taritd/node-b/fleet.db` |
| C | `TARIT_HOST_ID=node-c`, `TARIT_LISTEN=0.0.0.0:8080`, `TARIT_RPC_ADDR=http://10.0.1.12:8080`, `TARIT_SOCKET_DIR=$HOME/.taritd/node-c/sockets`, `TARIT_DB=$HOME/.taritd/node-c/fleet.db` |

Start `./target/release/taritd` on each node. Within one heartbeat interval, every node should show in:

```sh
curl -sf -H "X-API-Key: $TARIT_API_KEY" http://10.0.1.10:8080/v1/cluster
```

Cluster startup checks:

1. `TARIT_PEER_SECRET` is not the default `dev-peer-secret`. The process refuses that default when `TARIT_DATABASE_URL` is set.
2. `TARIT_RPC_ADDR` values are reachable from all peers.
3. All nodes can connect to PostgreSQL with the same database URL.
4. Each node has its own socket directory and SQLite DB path.
5. Kernel, rootfs, and VMM binary paths are valid on every node.

## Load balancer

Use `/health` for target health checks. It does not require authentication and always returns `{"status":"ok"}` when the HTTP server is alive.

Recommended public routing:

| Path | Exposure |
| --- | --- |
| `/health` | Public or load balancer only. |
| `/v1/*` | Public through TLS and `X-API-Key`. |
| `/openapi.yaml`, `/docs` | Optional public exposure. Useful during development, consider restricting in production. |
| `/internal/v1/*` | Private only. Never route from the public listener. |

The current binary mounts public and internal routes on the same listener. In production, isolate internal access with network policy, security groups, firewall rules, or a sidecar/proxy that blocks `/internal/v1/*` from public networks.

Execution polling caveat: `POST /v1/execute_async` creates the execution record on whichever API node accepted the request. `GET /v1/executions/{id}` is not forwarded through the fleet. If clients use an external load balancer, enable stickiness for execution polling or have the client poll the same node.

## Warm pool operations

Enable the warm pool with environment overrides:

```sh
export TARIT_WARM_POOL=true
export TARIT_WARM_POOL_TARGET=8
export TARIT_CPU_OVERCOMMIT=4.0
export TARIT_WARM_POOL_ROOTFS='/var/lib/taritd/rootfs.ext4'
```

Or use `TARIT_CONFIG`:

```toml
[warm_pool]
enabled = true
cpu_overcommit = 4.0
replenish_concurrency = 4

[[warm_pool.class]]
vcpus = 1
memory_mib = 256
target = 8
restore = true
rootfs = "/var/lib/taritd/rootfs.ext4"
# or use the local image registry:
# image = "node20"
```

Operational behavior:

- Warm VMs reserve scheduler slots, so assigned plus warm VMs do not exceed `TARIT_MAX_VMS`.
- Warm handout is possible only when the create request exactly matches a warm class shape and boot config.
- If the pool drains, creates cold boot instead of failing.
- Replenishment loops every 150 ms and limits concurrent warm spawns by `replenish_concurrency`.
- Set `restore = true` on a warm class to cold-boot one golden VM, snapshot it, and replenish the rest of that shape by restoring clones.

## Networking

By default `TARIT_ENABLE_NET=false` and VMs are created without host networking.

When enabled, `taritd`:

1. Detects the default route interface using `ip route get 8.8.8.8`.
2. Enables IPv4 forwarding with `sysctl -qw net.ipv4.ip_forward=1`.
3. Creates an nftables table `taritd_nat` for per-VM masquerade rules.
4. Allocates one reusable slot per VM from the 172.16.0.0/16 `/30` pool.
5. Creates one tap per VM named `insta<N>` where `N` is the slot.
6. Gives each VM a deterministic private `/30`: host `.1`, guest `.2`.
7. Persists the slot map in `TARIT_NET_STATE` (default `<TARIT_DB>.net.json`) and recovers it on restart.
8. Adds a per-slot nftables masquerade rule tagged with an `taritd` comment.
9. Appends a Linux `ip=` kernel command-line fragment so the guest configures `eth0`.

On startup, `taritd` reconciles the persisted map with live local VM records and
runs an age-gated sweep for stale `insta<N>` taps and orphaned `taritd` nftables
rules. Stop/delete teardown is idempotent and best-effort for tap deletion, nft
cleanup, and slot freeing.

Requirements:

- Linux `ip`, `nft`, and `sysctl` commands available.
- Run as root or grant enough capabilities for tap and nftables changes.
- Ensure host firewall policy allows intended forwarding.

## Rootfs mode

`TARIT_ROOTFS_READONLY=true` makes `taritd` attach the rootfs as read-only and rewrites the common `root=/dev/vda rw` fragment to `root=/dev/vda ro`. Use this when many VMs share one immutable base image. If false, a rootfs should be single-owner or otherwise safely cloned, because writable sharing can corrupt filesystems.

## PostgreSQL and RDS

The fleet store creates tables automatically. For AWS RDS, `deploy/provision-rds.sh` provisions an isolated PostgreSQL 16 `db.t4g.micro`, writes credentials to `$HOME/.taritd/cp-rds.env`, downloads the RDS global CA bundle, and sets:

```sh
export TARIT_DATABASE_URL='postgres://...?...sslmode=require'
export TARIT_RDS_CA_FILE="$HOME/.taritd/rds-global-bundle.pem"
```

`TARIT_RDS_CA_FILE` is loaded in addition to webpki and native roots.

## Deploy scripts

| Script | Purpose | Important inputs |
| --- | --- | --- |
| `deploy/c8i-deploy.sh` | Rsync this repo to an EC2 c8i host, build `taritd`, build or reuse `vmm`, start `taritd`, and run `tests/e2e_c8i.sh`. | `C8I_HOST`, `C8I_USER`, `C8I_KEY`, `REMOTE_DIR`, `VMM_DIR`, `TARIT_API_KEY`. |
| `deploy/provision-rds.sh` | Create a new RDS PostgreSQL fleet store and credentials env file. | `AWS_REGION`, `TARIT_CP_VPC_ID`, `TARIT_CP_C8I_SG`, `TARIT_CP_DB_PASSWORD`. |
| `deploy/open-api-port.sh` | Open TCP API port on the configured EC2 security group. | `C8I_SG`, `TARIT_PORT`, `TARIT_PUBLIC_CIDR`, `AWS_REGION`. |

The deploy script uses `pkill -f 'target/release/taritd'` on the remote host. Use caution on shared hosts.

## Benchmarks

`tarit-bench` exercises create, execute, poll, and delete flows and writes reports under `./bench-results` by default.

Key options:

```sh
cargo run --release -p tarit-bench -- all \
  --url http://127.0.0.1:8080 \
  --api-key "$TARIT_API_KEY" \
  --iterations 100 \
  --concurrency 100 \
  --command 'node -v' \
  --memory-mib 256 \
  --vcpus 1
```

Modes are `sequential`, `staggered`, `burst`, or `all`.

## Security checklist

- Use a long random `TARIT_API_KEY`.
- Use a different long random `TARIT_PEER_SECRET` for peers.
- Do not expose `/internal/v1/*` publicly.
- Use TLS for public clients, usually at the load balancer.
- Prefer mTLS for peer traffic in production. Peer auth uses a shared header secret; add mTLS at the network or proxy layer when required.
- Keep `TARIT_RPC_ADDR` values private and stable.
- Use PostgreSQL TLS and CA validation where available.
- Restrict VMM, kernel, rootfs, socket, and SQLite paths to trusted local directories.
- If `TARIT_ENABLE_NET=true`, audit nftables and host forwarding policy.
- Avoid writable rootfs sharing. Use `TARIT_ROOTFS_READONLY=true` for shared base images.

## Troubleshooting

| Symptom | Likely cause | What to check |
| --- | --- | --- |
| Startup fails with `configure at least one API key ...` | No API key configured. | Set `TARIT_API_KEY`, `TARIT_API_KEYS`, or `[api_keys]` in `TARIT_CONFIG`. |
| Startup fails with peer secret error | Cluster mode with default `dev-peer-secret`. | Set a strong `TARIT_PEER_SECRET` on every node. |
| `401 unauthorized` on `/v1/*` | Missing or wrong `X-API-Key`. | Check client header and environment value. |
| `401` on `/internal/v1/*` | Missing or wrong `X-Peer-Secret`. | Ensure all nodes share the same peer secret. |
| Create returns 429 | All visible nodes are full for the admission window. Response carries `Retry-After`. | Check `/v1/cluster`, `TARIT_MAX_VMS`, max vCPUs, max memory, and stale heartbeats. |
| Create returns 409 | A VM with the same explicit `id` already exists in the fleet. | Use a fresh id or look up the existing VM. |
| Peer placement never happens | No PostgreSQL fleet or no healthy peers. | Check `TARIT_DATABASE_URL`, heartbeats, `rpc_addr`, and peer reachability. |
| A node shows `up: false` | Heartbeat older than about 15 seconds or unhealthy row. | Check node process, PostgreSQL connectivity, and clock skew. |
| Owner operations fail with peer HTTP errors | `fleet_vms` points to a host that is down or unreachable. | Check owner `rpc_addr`, firewalls, and node status. |
| Restore fails on another node | Snapshot path is node-local. | Pass `host_id` returned by snapshot or use external shared storage. |
| Execution polling returns 404 through LB | `GET /v1/executions/{id}` hit a different node. | Enable sticky routing or poll the accepting node. |
| `spawn vmm` fails | Wrong `TARIT_VMM_BIN` or missing execute bit. | Verify path and permissions. |
| `wait for socket` times out | VMM child failed to create UDS. | Check VMM logs, kernel/rootfs paths, and KVM availability. |
| Network provisioning fails | Missing privileges or `ip`/`nft`. | Run with required capabilities and verify host networking tools. |
