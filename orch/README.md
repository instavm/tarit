# Tarit orchestrator (`taritd`)

`taritd` is the Tarit orchestrator and PaaS control plane for microVM sandboxes. Each sandbox is one rust-vmm based `vmm` process launched as `vmm serve --socket <uds>`. The `tarit-vmm-client` crate talks to that process over a Unix domain socket using 4 byte big-endian length-prefixed JSON.

In cluster mode `taritd` runs as a multi-node control plane. Any node can accept public API traffic, create sandboxes locally or on peers, and forward lifecycle operations to the owning host. A transparent L4/L7 load balancer can sit in front of all nodes.

## What runs where

- `taritd`: Axum/Tokio HTTP API, placement, peer forwarding, local VM supervision, warm pool, fleet heartbeat, and optional autoscaling loop.
- `vmm`: one child process per sandbox, controlled by `taritd` through a node-local UDS.
- SQLite: node-local records for VMs, executions, and a cached host roster.
- PostgreSQL, optional: authoritative distributed fleet registry for node membership, VM ownership, and autoscaler leader election.

Single-host mode is selected by omitting `TARIT_DATABASE_URL`. Cluster mode is selected by setting `TARIT_DATABASE_URL` and the same strong `TARIT_PEER_SECRET` on every node.

## Quickstart: single node

Build `taritd` in this repository and the `vmm` binary from the sibling VMM repository:

```sh
cd /path/to/tarit/orch
cargo build --release -p taritd

cd /path/to/tarit/vmm
cargo build --release --features "vmm-core/kvm vmm-core/boot vmm-memory-backend/kvm"
```

Start one Linux/KVM host:

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

./target/release/taritd
```

Smoke test:

```sh
curl -sf http://127.0.0.1:8080/health
curl -sf -H "X-API-Key: $TARIT_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"memory_mib":256,"vcpus":1}' \
  http://127.0.0.1:8080/v1/vms
```

The same `taritd` binary also includes an ops CLI. Bare `taritd` still runs the
daemon; `taritd serve` is the explicit daemon form. Client commands use
`--base-url`/`TARIT_BASE_URL` and `--api-key`/`TARIT_API_KEY`:

```sh
taritd health
taritd image build --oci node:20-slim --name node20
taritd image ls
taritd vm create --vcpus 1 --memory-mib 256
taritd vm create --image node20 --vcpus 1 --memory-mib 256
taritd vm list
taritd metrics
```

## Quickstart: three-node cluster with shared PostgreSQL

Create one PostgreSQL database reachable from all nodes. The schema is created automatically on startup. Every node must use the same `TARIT_DATABASE_URL`, same strong `TARIT_PEER_SECRET`, and distinct host identity, listen address, advertised RPC address, socket directory, and SQLite path.

Node A:

```sh
export TARIT_API_KEY='replace-with-a-long-random-token'
export TARIT_PEER_SECRET='replace-with-a-long-random-peer-secret'
export TARIT_DATABASE_URL='postgres://taritd:password@postgres.example:5432/taritd?sslmode=require'
export TARIT_HOST_ID='node-a'
export TARIT_LISTEN='0.0.0.0:8080'
export TARIT_RPC_ADDR='http://10.0.1.10:8080'
export TARIT_SOCKET_DIR="$HOME/.taritd/node-a/sockets"
export TARIT_DB="$HOME/.taritd/node-a/fleet.db"
./target/release/taritd
```

Node B and C use the same API key, peer secret, database URL, kernel, rootfs, and VMM binary, but change:

```sh
# node-b
export TARIT_HOST_ID='node-b'
export TARIT_LISTEN='0.0.0.0:8080'
export TARIT_RPC_ADDR='http://10.0.1.11:8080'
export TARIT_SOCKET_DIR="$HOME/.taritd/node-b/sockets"
export TARIT_DB="$HOME/.taritd/node-b/fleet.db"

# node-c
export TARIT_HOST_ID='node-c'
export TARIT_LISTEN='0.0.0.0:8080'
export TARIT_RPC_ADDR='http://10.0.1.12:8080'
export TARIT_SOCKET_DIR="$HOME/.taritd/node-c/sockets"
export TARIT_DB="$HOME/.taritd/node-c/fleet.db"
```

Put a load balancer in front of `/health` on each node. Public API clients use `X-API-Key`; each key resolves to a tenant and role. Peer calls still use the shared `X-Peer-Secret`; keep them on a private network and add mTLS at the network or proxy layer when required.

## Placement and routing at a glance

- Create is exhaustive: try local warm/cold capacity first, then every healthy peer with capacity, best-first by least sandbox count and most free memory.
- A 429 on create means the whole visible cluster stayed full for `TARIT_ADMISSION_TIMEOUT_MS`; the response includes `Retry-After`.
- VM ownership comes from PostgreSQL `fleet_vms` in cluster mode. Gossip is not used.
- Get, delete, pause, resume, snapshot, execute, and egress update resolve the VM owner and forward to `/internal/v1/*` on that node.
- Snapshot files are node-local. Snapshot responses include `host_id`; restore routes to that host. No cross-node snapshot file copy occurs.

## API at a glance

Unauthenticated endpoints:

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/health` | Load balancer health check, returns `{"status":"ok"}`. |
| GET | `/metrics` | Prometheus text-format operational metrics (no API key). |
| GET | `/openapi.yaml` | Served OpenAPI document, with server URL rewritten from `Host`. |
| GET | `/docs` | Swagger UI for the served OpenAPI document. |

Public endpoints require `X-API-Key`. User keys are tenant-scoped; admin keys can call admin-only routes such as `/v1/cluster`.

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/v1/vms` | List VM records visible to the caller's tenant. |
| POST | `/v1/vms` | Create a tenant-owned VM locally or on any peer with capacity, subject to that tenant's VM quota. |
| GET | `/v1/vms/{id}` | Resolve owner and return the VM record. |
| GET | `/v1/vms/{id}/status` | Resolve owner and return live VMM runtime status. |
| DELETE | `/v1/vms/{id}` | Resolve owner, stop the VM, mark it stopped locally, clear fleet ownership. |
| POST | `/v1/vms/{id}/pause` | Resolve owner and pause. |
| POST | `/v1/vms/{id}/resume` | Resolve owner and resume. |
| POST | `/v1/vms/{id}/snapshot` | Resolve owner and snapshot, returning `path` and `host_id`. |
| POST | `/v1/restore` | Restore a node-local snapshot, optionally routed by `host_id`. |
| POST | `/v1/execute` | Resolve owner and run a command synchronously, returning the finished record. |
| POST | `/v1/execute_async` | Resolve owner and run a command asynchronously. |
| GET | `/v1/executions/{id}` | Get an execution record stored on the API node that accepted it. |
| PATCH | `/v1/egress/vm/{id}` | Resolve owner and update live egress policy. |
| POST/GET/DELETE | `/v1/ssh-keys[/{key_id}]` | Register, list, and deactivate caller-scoped OpenSSH public keys. |
| POST/GET/DELETE | `/v1/vms/{id}/pty/sessions[/{pty_id}]` | Manage local PTY session records for a VM. |
| POST | `/v1/vms/{id}/pty/sessions/{pty_id}/resize` | Update a PTY session's recorded dimensions. |
| WS | `/v1/vms/{id}/pty/{pty_id}/connect?token=<connect_token>` | Bridge WebSocket PTY bytes to the owning local VMM stream. The token comes from the create-session response. |
| GET | `/v1/cluster` | Admin-only cluster capacity and health summary. |
| GET | `/v1/usage` | Per-key usage stats from the primary store (admin: all keys; user: own). |
| GET | `/v1/audit` | Per-key audit trail from the primary store (admin: all keys; user: own). |

Full request and response schemas are in [docs/API.md](docs/API.md).

## CLI reference

The client subcommands share three global flags: `--base-url <URL>` (env `TARIT_BASE_URL`, default `http://127.0.0.1:8080`), `--api-key <KEY>` (env `TARIT_API_KEY`), and `--json` to print raw JSON responses.

| Command | Purpose |
| --- | --- |
| `taritd` or `taritd serve` | Run the API daemon. |
| `taritd health` | Check API health. |
| `taritd cluster` | Show cluster capacity and health (admin key). |
| `taritd metrics` | Print Prometheus metrics. |
| `taritd vm create [--memory-mib <N>] [--vcpus <N>] [--rootfs <PATH> \| --image <NAME[:TAG]>]` | Create a VM. `--rootfs` and `--image` conflict. |
| `taritd vm list` | List VM records. |
| `taritd vm get <id>` | Show one VM record. |
| `taritd vm delete <id>` | Stop and remove a VM. |
| `taritd vm pause <id>`, `taritd vm resume <id>` | Pause or resume a VM. |
| `taritd vm snapshot <id> [--diff]` | Snapshot a VM; prints `path` and `host_id`. |
| `taritd restore <snapshot_path>` | Restore a VM from a snapshot file. |
| `taritd exec <id> <command...>` | Run a command in the VM over `POST /v1/execute`; exits with the command's exit code. |
| `taritd image build --oci <OCI_REF> --name <NAME[:TAG]>` | Build and register a rootfs image from an OCI image. |
| `taritd image ls` | List registered images. |
| `taritd image rm <NAME[:TAG]>` | Remove an unreferenced image. |
| `taritd image gc [--older-than-days <DAYS>] [--pattern <PATTERN>] [--dry-run]` | Remove unreferenced images older than a threshold (default 7 days). |
| `taritd ssh-key add <FILE\|->` | Register an OpenSSH public key from a file, or from stdin with `-`. |
| `taritd ssh-key list` | List active SSH keys. |
| `taritd ssh-key rm <key_id>` | Remove an SSH key. |
| `taritd pty <id> [--shell <S>]` | Attach an interactive PTY over WebSocket. |
| `taritd ssh <id> [--ssh-host <HOST>] [--ssh-port <PORT>] [extra ssh args...]` | Open the VM through the SSH gateway. Host and port default from `TARIT_SSH_GATEWAY_ADDR` (`127.0.0.1:2222`). |

## Environment variable reference

| Variable | Required | Default | Meaning |
| --- | --- | --- | --- |
| `TARIT_API_KEY` | Required if no multi-key config | none | Backward-compatible single public API key. It maps to tenant `default`, role `admin`, and unlimited VMs. Empty values are rejected. |
| `TARIT_API_KEYS` | Required if no `TARIT_API_KEY` or TOML keys | none | Comma-separated multi-key config: `key:tenant:role:max_vms`. Roles are `admin` or `user`; `max_vms=0` means unlimited. Example: `key1:tenantA:user:20,key2:tenantB:admin:0`. |
| `TARIT_LISTEN` | No | `0.0.0.0:8080` | Socket address the HTTP server binds. |
| `TARIT_HOST_ID` | No | `hostname` or `localhost` | Stable node identity used in local records and fleet ownership. |
| `TARIT_VMM_BIN` | No | `vmm` (looked up on `PATH`) | Path to the rust-vmm based `vmm` binary. |
| `TARIT_KERNEL` | No | `/tmp/vmlinux.microvm` | Default guest kernel path used when create omits `kernel_path`. |
| `TARIT_ROOTFS` | No | `/tmp/debian-rootfs.ext4` | Default rootfs path used when create omits `rootfs_path`. |
| `TARIT_SOCKET_DIR` | No | `~/.taritd/sockets` | Directory for one VMM UDS per VM. |
| `TARIT_DB` | No | `~/.taritd/fleet.db` | Node-local SQLite database path. |
| `TARIT_NET_STATE` | No | `<TARIT_DB>.net.json` | Persistent per-VM tap/IP slot map used when `TARIT_ENABLE_NET=true` so restarts reuse/free `/30` slots safely. |
| `TARIT_IMAGES_DIR` | No | `~/.taritd/images` | Directory for registered immutable rootfs images built by `taritd image build`. |
| `TARIT_MAX_VMS` | No | `32` | Max concurrent local VM slots, including warm pool reservations. |
| `TARIT_MAX_VCPUS` | No | `64`, or physical cores times `TARIT_CPU_OVERCOMMIT` when warm pool is enabled | Advertised local vCPU ceiling. |
| `TARIT_MAX_MEMORY_MIB` | No | `65536` | Advertised local memory ceiling in MiB. |
| `TARIT_PEER_SECRET` | Cluster: yes | random local-only value | Shared secret for `X-Peer-Secret`. Explicit values must be at least 32 characters and cannot use the development default. |
| `TARIT_DATABASE_URL` | No | unset | PostgreSQL fleet registry URL. Set to enable distributed cluster mode. |
| `TARIT_RDS_CA_FILE` | No | unset | Extra CA bundle loaded for PostgreSQL TLS, used by RDS deployments. |
| `TARIT_RPC_ADDR` | Cluster: yes | `http://<listen-ip>:<listen-port>` | Base URL advertised to peers in the fleet registry. Must be reachable by other nodes. |
| `TARIT_ENABLE_NET` | No | `false` | Enable per-VM tap plus `/30` addressing plus nftables NAT. Requires `CAP_NET_ADMIN`. |
| `TARIT_ROOTFS_READONLY` | No | `false` | Attach rootfs read-only and rewrite `root=/dev/vda rw` to `ro`. Use for shared immutable images. |
| `TARIT_ADMISSION_TIMEOUT_MS` | No | `60000` | How long create waits and retries when the cluster is full before returning 429. |
| `TARIT_REAP_ON_SHUTDOWN` | No | `true` | On SIGTERM/SIGINT, stop all local `vmm serve` children and remove their sockets/overlays after HTTP drain. Set `false` only for debugging. |
| `TARIT_CONFIG` | No | `~/.taritd/config.toml` | Optional TOML file for warm-pool configuration. Missing file is allowed. |
| `TARIT_WARM_POOL` | No | `false` | Enable warm-pool replenishment. Accepts `1` or `true`. |
| `TARIT_WARM_POOL_TARGET` | No | `8` for the default class | Override target count for the first warm-pool class. If watermarks are unset, effective hard/low/high watermarks derive from this target. |
| `TARIT_WARM_POOL_HARD_FLOOR` | No | derived from target | Override the emergency minimum for the first warm-pool class. Must be `<= low_watermark`. |
| `TARIT_WARM_POOL_LOW_WATERMARK` | No | derived from target | Refill the first warm-pool class only after depth drops below this watermark. Must be `<= target`. |
| `TARIT_WARM_POOL_HIGH_WATERMARK` | No | derived from target | Safety ceiling for the first warm-pool class. Must be `>= target`. |
| `TARIT_WARM_POOL_RESTORE` | No | `false` for the default class | Use restore-from-golden replenishment for the first warm-pool class. Accepts `1` or `true`; use TOML for multiple classes. |
| `TARIT_REFILL_CGROUP` | No | unset | Optional cgroup v2 directory for refill-spawned `vmm serve` children, e.g. `/sys/fs/cgroup/taritd-refill`. If unset or unavailable, refill continues without cgroup placement. |
| `TARIT_REFILL_CPU_WEIGHT` | No | `10` | Low cgroup v2 `cpu.weight` assigned to the refill cgroup; live-created VMs keep the default cgroup weight. |
| `TARIT_CPU_OVERCOMMIT` | No | `4.0` | Warm-pool CPU overcommit ratio. |
| `TARIT_WARM_POOL_ROOTFS` | No | unset | Override rootfs for the first warm-pool class. |
| `TARIT_WARM_POOL_IMAGE` | No | unset | Override rootfs for the first warm-pool class by registered image `name[:tag]`. |
| `TARIT_REGION` | No | `local` | Topology label included in autoscaler decisions. |
| `TARIT_ZONE` | No | same as `TARIT_REGION` | Topology label included in autoscaler decisions. |
| `TARIT_CLOUD` | No | `onprem` | Topology label included in autoscaler decisions. |
| `TARIT_AUTOSCALE` | No | `false` | Enable leader-elected autoscaler. Accepts `1` or `true`. |
| `TARIT_AUTOSCALE_MIN` | No | `1` | Minimum healthy node count. |
| `TARIT_AUTOSCALE_MAX` | No | `10` | Maximum healthy node count. |
| `TARIT_AUTOSCALE_OUT_FREE_VCPUS` | No | `2` | Scale out when aggregate free vCPUs falls below this. |
| `TARIT_AUTOSCALE_IN_FREE_VCPUS` | No | `64` | Scale in when aggregate free vCPUs is above this. |
| `TARIT_AUTOSCALE_PROVIDER_CMD` | No | unset | Shell command invoked by the autoscaler. The decision JSON is available as `$1` in the shell command. |
| `TARIT_SSH_GATEWAY` | No | `false` | Enable the embedded SSH gateway. Accepts `1` or `true`. |
| `TARIT_SSH_GATEWAY_ADDR` | No | `127.0.0.1:2222` | Socket address for `ssh <vm_id>@<host> -p <port>` PTY access. |
| `TARIT_SSH_GATEWAY_HOST_KEY` | No | `~/.taritd/ssh_host_ed25519` | OpenSSH Ed25519 host key path. Generated with `0600` permissions if missing. |

## SSH gateway

When enabled, the embedded gateway authenticates registered OpenSSH public keys
for supported algorithms (RSA client keys are not supported), treats the SSH
username as the target VM UUID, verifies that the key owner created/restored
that VM, and bridges the SSH channel to the same PTY stream used by the WebSocket
PTY API:

```sh
export TARIT_SSH_GATEWAY=1
export TARIT_SSH_GATEWAY_ADDR='0.0.0.0:2222'
./target/release/taritd

ssh -p 2222 <vm_id>@<taritd-host>
```

Optional warm-pool TOML shape:

```toml
[api_keys]
"key1" = { tenant = "tenantA", role = "user", max_vms = 20 }
"key2" = { tenant = "tenantB", role = "admin", max_vms = 0 } # unlimited

[warm_pool]
enabled = true
cpu_overcommit = 4.0
replenish_concurrency = 4
refill_cgroup = "/sys/fs/cgroup/taritd-refill"
refill_cpu_weight = 10

[[warm_pool.class]]
vcpus = 1
memory_mib = 256
hard_floor = 4
low_watermark = 6
target = 8
high_watermark = 10
restore = true
rootfs = "/var/lib/taritd/rootfs.ext4"
# or: image = "node20"
```

With `restore = true`, `taritd` cold-boots one ready golden VM for the class,
takes a snapshot, tears down the builder, then refills that class by restoring
clones from the golden. Each restored clone receives its own writable CoW
overlay; `restore = false` keeps the bounded cold-boot refill path. Refill starts
only below `low_watermark`, fills back to `target`, and uses the optional
low-weight refill cgroup when `TARIT_REFILL_CGROUP`/`refill_cgroup` is set.

## Image registry

`taritd image build --oci <ref> --name <name>[:tag]` uses
`$TARIT_VMM_BIN pull --agent <ref> <out.ext4>`, runs `e2fsck -fy`, stores the
rootfs under `TARIT_IMAGES_DIR`, and registers it in the node-local SQLite DB.
Create requests can use `--image <name>[:tag]` (or JSON `image`) instead of a raw
`rootfs_path`; warm-pool classes can set `image = "name[:tag]"`.

```sh
taritd image build --oci node:20-slim --name node20
taritd image ls
taritd vm create --image node20 --vcpus 1 --memory-mib 256
taritd image rm node20
taritd image gc --older-than-days 7 --dry-run
```

## More documentation

- [Quickstart](docs/QUICKSTART.md)
- [Configuration reference](docs/CONFIGURATION.md)
- [Architecture](docs/ARCHITECTURE.md)
- [API](docs/API.md)
- [Operations](docs/OPERATIONS.md)
- [Resilience and scale scenarios](docs/RESILIENCE.md)
- [Usage stats and audit trail](docs/USAGE-AND-AUDIT.md)
- [Autoscaling](docs/AUTOSCALING.md)
- [Warm pool replenishment](docs/REPLENISHMENT.md)
- [VM isolation](docs/ISOLATION.md)
