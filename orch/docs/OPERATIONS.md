# Operations guide

This guide covers building, running, clustering, load balancing, deployment helpers, security, and troubleshooting for `taritd`.

## Requirements

- Linux host with KVM for actually running microVMs.
- Rust stable toolchain.
- The rust-vmm based `vmm` binary from the sibling VMM repository.
- The release ELF `vmlinux` and optional rootfs image readable by `taritd`.
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

Prepare and install the kernel and rootfs:

```sh
cd /path/to/tarit
sudo make guest
sudo install -d -m 0755 /var/lib/taritd
sudo install -m 0644 guest-assets/vmlinux guest-assets/rootfs.ext4 /var/lib/taritd/
```

`make guest` verifies the pinned release kernel checksum and falls back to the
same checksum-pinned source build if the artifact is unavailable.

## Run one node

```sh
cd /path/to/tarit/orch

export TARIT_API_KEY='replace-with-a-long-random-token'
export TARIT_LISTEN='0.0.0.0:8080'
export TARIT_HOST_ID="$(hostname)"
export TARIT_RPC_ADDR='http://127.0.0.1:8080'
export TARIT_VMM_BIN='/path/to/tarit/vmm/target/release/vmm'
export TARIT_KERNEL='/var/lib/taritd/vmlinux'
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
export TARIT_KERNEL='/var/lib/taritd/vmlinux'
export TARIT_ROOTFS='/var/lib/taritd/rootfs.ext4'
export TARIT_MAX_VMS='32'
export TARIT_MAX_MEMORY_MIB='65536'
export RUST_LOG='taritd=info,tower_http=info'
```

Each node must differ:

The `:8443` origins below assume a private TLS proxy that denies every route
except `/internal/v1/*` and forwards those requests to the node's HTTP listener
on `:8080`. The proxy must support WebSocket upgrades and preserve method, path,
query, body, and `X-Tarit-*` headers exactly because `taritd` validates them as
part of the request HMAC. Its certificate SAN must match the advertised
hostname and chain to the WebPKI roots built into the Rustls clients; a custom
peer CA cannot currently be configured. For an isolated development cluster
without a compatible proxy, use the private `http://...:8080` origin and set
`TARIT_ALLOW_INSECURE_PEER_HTTP=1` explicitly.

| Node | Required differences |
| --- | --- |
| A | `TARIT_HOST_ID=node-a`, `TARIT_LISTEN=0.0.0.0:8080`, `TARIT_RPC_ADDR=https://node-a.peer.example.com:8443`, `TARIT_SOCKET_DIR=$HOME/.taritd/node-a/sockets`, `TARIT_DB=$HOME/.taritd/node-a/fleet.db` |
| B | `TARIT_HOST_ID=node-b`, `TARIT_LISTEN=0.0.0.0:8080`, `TARIT_RPC_ADDR=https://node-b.peer.example.com:8443`, `TARIT_SOCKET_DIR=$HOME/.taritd/node-b/sockets`, `TARIT_DB=$HOME/.taritd/node-b/fleet.db` |
| C | `TARIT_HOST_ID=node-c`, `TARIT_LISTEN=0.0.0.0:8080`, `TARIT_RPC_ADDR=https://node-c.peer.example.com:8443`, `TARIT_SOCKET_DIR=$HOME/.taritd/node-c/sockets`, `TARIT_DB=$HOME/.taritd/node-c/fleet.db` |

Start `./target/release/taritd` on each node. Within one heartbeat interval, every node should show in:

```sh
curl -sf -H "X-API-Key: $TARIT_API_KEY" http://10.0.1.10:8080/v1/cluster
```

Cluster startup checks:

1. `TARIT_PEER_SECRET` is the same strong value on every node. The process refuses missing or weak values when `TARIT_DATABASE_URL` is set.
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

In fleet mode, asynchronous execution records are stored in PostgreSQL, so
`GET /v1/executions/{id}` can be polled through any healthy API node.

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
- Shutdown and user-boot publication share a boot gate. The global order is
  `terminal transition gate` → `boot gate` → `running`/`warm` → `booting`;
  shutdown releases the boot gate before its separate `running` → `warm`
  teardown phase. No synchronous supervisor lock is held across an await.
  Create and restore first establish a boot entry, durable local `Creating`
  record, and (when configured) routable fleet ownership while holding the boot
  gate; only then may scheduler, image, or VMM work start.
- A user lifecycle is explicitly `Creating` → `Publishing` → `Running` or a
  terminal `Stopped`/`Error` transition. Publishing advances fleet ownership,
  SQLite, cache visibility, and supervisor running ownership in order. A failed
  step retains the VMM, network, reservation, and lifecycle state; a later
  stop/delete/stop-all retries convergence before teardown. Terminal records
  retain fleet ownership and capacity until SQLite, fleet clear, cache commit,
  and reservation release each acknowledge.
- Warm handout is possible only when the create request exactly matches a warm class shape and boot config.
- If the pool drains, creates cold boot instead of failing.
- Replenishment limits concurrent warm spawns by `replenish_concurrency`; it
  sleeps for 150 ms after an at-capacity attempt, rather than spinning while
  the host is overloaded.
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
7. Persists the slot map and each allocation's egress allowlist/default-deny policy in
   version-2 `TARIT_NET_STATE` (default `<TARIT_DB>.net.json`) and recovers both on
   restart. Version-1 state with any live allocation is rejected; only an empty or
   entirely stale version-1 file is safely migrated.
8. Adds a per-slot nftables masquerade rule tagged with an `taritd` comment.
9. Installs per-tap forward guards before bringing the tap up: guest traffic may
   leave only through the detected uplink and cannot target the `172.16.0.0/16`
   VM pool; established and related return traffic remains allowed.
10. Appends a Linux `ip=` kernel command-line fragment so the guest configures `eth0`.

Before loading configuration, opening a database, resolving images, or looking
up VMs, `taritd` enumerates strict `insta<N>` names with structured `ip -j
link` output and lowers them. A containment, VM-list, or recovery error is
fatal: no supervisor or HTTP listener is published. Network-disabled startup
is allowed only when that preflight found no Tarit TAP and there are no local
live VM records requiring recovery. It then reconciles the persisted map with
live local VM records, validates the
required nft base-chain hooks, and atomically inserts top-of-chain forward and
input drop quarantines for every recovered TAP. It removes Tarit-owned stale
policy for all their slots and programs and verifies every netdev IPv4/ARP,
source, VM-pool lateral, uplink, host-input, masquerade, and persisted egress
policy while all TAPs remain contained.

`taritd_nat` forward and input chains are closed Tarit ownership domains:
every rule relevant to a recovered TAP must have a recognized Tarit comment and
the exact managed rule shape. Operator or ambiguous rules are preserved but
make recovery fail closed; no TAP is activated behind an earlier ambiguous
accept. The completed allocator state is persisted before quarantines release
using a mode-0600 temp file, write/flush/file sync, atomic rename, and parent
directory sync.
Live egress updates likewise install a top-of-chain quarantine, replace all
egress rules in one nft transaction, verify the effective stateful-return,
allow, and final default-deny ordering, durably persist, and only then release.
Updates are serialized across the host-owned nft/state transaction.

A containment or reconciliation failure cleans partial policy and keeps every
TAP down or quarantined. If quarantine installation fails, `taritd` also
best-effort lowers/deletes every strict Tarit TAP and disables IPv4 and IPv6
forwarding before returning an aggregated fatal error. If link deletion and
both forwarding disables all fail, no stronger in-process kernel invariant can
be guaranteed; `taritd` remains unavailable and reports every failed
containment action rather than claiming containment. Stop/delete teardown is
retryable and fails the operation if the strict TAP cannot be contained and
deleted, exact policy cleanup fails, or durable slot release is ambiguous.
Tarit retains the allocation and policy for retry; operators must resolve the
reported containment error before treating the VM or its network capacity as
stopped/freed. After a post-rename state-directory sync failure, the running
process refuses further provisioning until it is restarted and the persisted
state is inspected/reconciled.

Malformed or ambiguous `TARIT_NET_STATE` (an out-of-range slot, mismatched
slot/TAP identity, duplicate slot, duplicate VM ID, or nil VM owner) is never
pruned or rewritten. Startup first contains every strict Tarit TAP, then fails
before nft recovery or slot release. Preserve the state file and resolve the
duplicate/corrupt ownership with manual host inspection; do not reuse or delete
the affected slot until its TAP is confirmed absent and its exact managed policy
has been cleaned.

Requirements:

- Linux `ip`, `nft`, and `sysctl` commands available.
- Run as root or grant enough capabilities for tap and nftables changes.
- Ensure host firewall policy allows intended forwarding.

## Rootfs mode

Every rootfs base is opened immutably and attached through a private per-VM CoW overlay, so guests never share writable filesystem state. `TARIT_ROOTFS_READONLY=true` additionally requests read-only guest mount semantics by rewriting the common `root=/dev/vda rw` fragment to `root=/dev/vda ro`.

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
- Use a long random `TARIT_PEER_SECRET` for peers and rotate it with a coordinated restart.
- Do not expose `/internal/v1/*` publicly.
- Use TLS for public clients, usually at the load balancer.
- Peer requests use a replay-protected HMAC; the shared key is never transmitted.
- A separate internal listener with mandatory mTLS and host-session fencing is still required before hostile multi-tenant production use.
- Keep `TARIT_RPC_ADDR` values private and stable.
- Use PostgreSQL TLS and CA validation where available.
- Restrict VMM, kernel, rootfs, socket, and SQLite paths to trusted local directories.
- If `TARIT_ENABLE_NET=true`, audit nftables and host forwarding policy.
- Keep base images immutable; taritd always places guest writes in a private per-VM CoW overlay.

## Troubleshooting

| Symptom | Likely cause | What to check |
| --- | --- | --- |
| Startup fails with `configure at least one API key ...` | No API key configured. | Set `TARIT_API_KEY`, `TARIT_API_KEYS`, or `[api_keys]` in `TARIT_CONFIG`. |
| Startup fails with peer secret error | Cluster mode without a strong `TARIT_PEER_SECRET`, or an explicit secret shorter than 32 characters. | Set a strong `TARIT_PEER_SECRET` on every node. |
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
