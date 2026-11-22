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

Build `taritd` and the `vmm` binary, both in this repository:

```sh
cd /path/to/tarit
make build
sudo make guest
sudo install -d -m 0755 /var/lib/taritd
sudo install -m 0644 guest-assets/vmlinux guest-assets/rootfs.ext4 /var/lib/taritd/
```

`make guest` verifies the pinned release kernel checksum and falls back to the
same checksum-pinned source build if the artifact is unavailable.

Start one Linux/KVM host:

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

Use one PostgreSQL database and one peer HMAC key for the fleet. Give each
node its own host ID, private peer listener and certificate. Expose only the
public API listener to the load balancer. [Node configuration](docs/CONFIGURATION.md#one-node-of-a-three-node-cluster-with-postgresql) · [Three-node setup](docs/OPERATIONS.md#run-a-three-node-cluster)

## Placement and routing at a glance

- Create is exhaustive: try local warm/cold capacity first, then every healthy peer with capacity, best-first by least sandbox count and most free memory.
- A 429 on create means the whole visible cluster stayed full for `TARIT_ADMISSION_TIMEOUT_MS`; the response includes `Retry-After`.
- VM ownership comes from PostgreSQL `fleet_vms` in cluster mode. Gossip is not used.
- Get, delete, pause, resume, snapshot, execute, and egress update resolve the VM owner and forward to `/internal/v1/*` on that node.
- Snapshot returns an opaque `snapshot_id`. Restore resolves the holding node internally. Cross-node live fork verifies the artifact at its destination before publishing a child.

## API at a glance

Unauthenticated endpoints:

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/health` | Load balancer health check, returns `{"status":"ok"}`. |
| GET | `/openapi.yaml` | Served OpenAPI document, with server URL rewritten from `Host`. |
| GET | `/docs` | Swagger UI for the served OpenAPI document. |

Public endpoints require `X-API-Key`. User keys are tenant-scoped; admin keys can call admin-only routes such as `/v1/cluster`.

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/metrics` | Read this node’s Prometheus metrics (admin key required). |
| GET | `/v1/vms` | List VM records visible to the caller's tenant. |
| POST | `/v1/vms` | Create a tenant-owned VM locally or on any peer with capacity, subject to that tenant's VM quota. |
| GET | `/v1/vms/{id}` | Resolve owner and return the VM record. |
| GET | `/v1/vms/{id}/status` | Resolve owner and return live VMM runtime status. |
| DELETE | `/v1/vms/{id}` | Resolve owner, stop the VM, mark it stopped locally, clear fleet ownership. |
| POST | `/v1/vms/{id}/pause` | Resolve owner and pause. |
| POST | `/v1/vms/{id}/fork` | Fork a running VM at one checkpoint. Retry with the same child ID. |
| POST | `/v1/vms/{id}/hibernate` | Release the VMM and retain the logical VM for wake. |
| POST | `/v1/vms/{id}/resume` | Resolve owner and resume. |
| POST | `/v1/vms/{id}/snapshot` | Resolve owner and snapshot, returning an opaque `snapshot_id`. |
| GET | `/v1/snapshots` | List the tenant’s snapshots across the fleet. Partial results carry `X-Tarit-Unavailable-Hosts`. |
| DELETE | `/v1/snapshots/{id}` | Delete a snapshot no branch, hibernated VM, or fork still needs. |
| POST | `/v1/restore` | Restore from a `snapshot_id`, routed to the node that holds it. |
| POST | `/v1/execute` | Resolve owner and run a command synchronously, returning the finished record. |
| POST | `/v1/execute_async` | Resolve owner and run a command asynchronously. |
| GET | `/v1/executions/{id}` | Read an execution record from the fleet store or the single node. |
| PATCH | `/v1/egress/vm/{id}` | Resolve owner and update live egress policy. |
| POST / GET | `/v1/ssh-keys` | Register or list caller-scoped OpenSSH public keys. |
| DELETE | `/v1/ssh-keys/{key_id}` | Deactivate a key. |
| POST / GET | `/v1/vms/{id}/pty/sessions` | Create or list PTY sessions. |
| DELETE | `/v1/vms/{id}/pty/sessions/{pty_id}` | Close a session. |
| POST | `/v1/vms/{id}/pty/sessions/{pty_id}/resize` | Update a PTY session's recorded dimensions. |
| WS | `/v1/vms/{id}/pty/{pty_id}/connect?token=<connect_token>` | Bridge WebSocket PTY bytes to the owning local VMM stream. One connection is active per session; the session can reconnect after disconnect. |
| GET | `/v1/cluster` | Admin-only cluster capacity and health summary. |
| GET | `/v1/usage` | Per-key usage stats from the primary store or local outbox (admin: all keys; user: own). |
| GET | `/v1/audit` | Per-key audit trail from the primary store or local outbox (admin: all keys; user: own). |

Full request and response schemas are in [docs/API.md](docs/API.md).

API request admission is per API node. `TARIT_API_REQUESTS_PER_SECOND` (default
5,000) is a token-bucket rate per configured tenant; all keys for a tenant share
the bucket. Invalid credentials and PTY WebSocket connects without an API key
share a separate fixed bucket and cannot spend authenticated tenants' capacity.
`TARIT_API_MAX_IN_FLIGHT` (default 1,024) caps authenticated handlers globally;
each tenant may hold at most half that budget (minimum one). The unauthenticated
budget has its own half-sized concurrency cap. Total admitted handlers are thus
bounded by the global authenticated cap plus the unauthenticated cap. A global
cap of one cannot reserve simultaneous capacity for another tenant.

Live controls (status, balloon, snapshot/fork, pause/suspend/resume, hibernate,
egress-policy update and DELETE) return 409 when another operation holds the
local VM's operation gate, including a long-running asynchronous exec. Retry
once the current operation finishes. Operations already performing lifecycle
publication or rollback continue after an HTTP timeout; only admission to a
busy VM is rejected. Lifecycle recovery and shutdown keep waiting for their
existing work to finish safely.

Asynchronous execution admission lasts until the command reaches a terminal state.
Each API node accepts at most `min(TARIT_API_MAX_IN_FLIGHT, 128)` unfinished
asynchronous commands. Admission checks the tenant first (all its API keys share
at most 32 slots, or half a smaller global budget, with a minimum of one), then
the VM (at most four unfinished commands), then the global budget. Excess
submissions return 429 without reserving global capacity. These limits apply to
administrators too and are per API node; a global budget of one cannot reserve
concurrent capacity for a second tenant. Waiting behind another operation on a
local VM is bounded by `TARIT_API_REQUEST_TIMEOUT_MS`, separately from the
command's runtime timeout.

The execution polling cache is limited to 1,024 records and 64 MiB; evicted
records remain readable from durable storage. `TARIT_EXECUTION_RETENTION_HOURS`
(default 24; zero disables expiry) applies to terminal records in PostgreSQL,
SQLite and memory, based on `updated_at`. Reads reject expired output immediately,
even before the hourly bounded cleanup runs. Pending/running work is retained.
Execution output belongs to the submitting tenant permanently, even after VM
deletion or UUID reuse. New records also retain the authorized VM creation time.
Legacy SQLite executions without an owner binding are admin-only: inferring an
owner from a possibly reused VM UUID would expose another tenant's output.

For PostgreSQL integration tests, use a disposable database and set
`TARIT_TEST_DATABASE_URL`; set `TARIT_REQUIRE_TEST_DATABASE=1` in CI to fail instead
of skipping the execution regressions when the database URL is missing.
Run `cargo test --locked -p tarit-fleet -p taritd -- --test-threads=1`.

Shares are revoked when their VM is deleted, including when retention frees its
UUID. Recreating that UUID requires new shares. SSH public-key registrations are
matched within the target VM's tenant, so the same public key can be registered
by multiple tenants independently.

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
| `taritd vm snapshot <id> [--diff]` | Snapshot a VM; prints the opaque `snapshot_id`. |
| `taritd restore <snapshot_id>` | Restore a VM from a snapshot handle. |
| `taritd exec <id> <command...>` | Run a command in the VM over `POST /v1/execute`; exits with the command's exit code. |
| `taritd image build --oci <OCI_REF> --name <NAME[:TAG]> [--size <MIB>]` | Build and register a rootfs image; size defaults to 1024 MiB and bounds OCI unpack work. |
| `taritd image ls` | List registered images. |
| `taritd image rm <NAME[:TAG]>` | Remove an unreferenced image. |
| `taritd image gc [--older-than-days <DAYS>] [--pattern <PATTERN>] [--dry-run]` | Remove unreferenced images older than a threshold (default 7 days). |
| `taritd ssh-key add <FILE\|->` | Register an OpenSSH public key from a file, or from stdin with `-`. |
| `taritd ssh-key list` | List active SSH keys. |
| `taritd ssh-key rm <key_id>` | Remove an SSH key. |
| `taritd pty <id> [--shell <S>]` | Attach an interactive PTY over WebSocket. |
| `taritd ssh <id> [--ssh-host <HOST>] [--ssh-port <PORT>] [ssh options...] [command...]` | Open the VM through the SSH gateway. Host and port default from `TARIT_SSH_GATEWAY_ADDR` (`127.0.0.1:2222`). Options go before the destination; the first other word (or anything after `--`) is the remote command. With `-i`, only that key is offered. |

## Environment variable reference

The [configuration reference](docs/CONFIGURATION.md) lists daemon variables,
defaults and validation rules, including capacity, networking, the VM jail and
the dedicated fleet peer listener.

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

# In a second shell, set VM_ID to the target VM UUID.
ssh -p 2222 -i ~/.ssh/tarit -o IdentitiesOnly=yes "${VM_ID}@127.0.0.1"
```

The gateway answers every offered key the same way and checks a key against
the registry only after the client has signed with it, so which keys are
registered cannot be probed without holding them. A wrong key is rejected
without delay; a client that tries several agent keys first signs once with
each. Offering only the registered key (`-i ... -o IdentitiesOnly=yes`) avoids
that; `taritd ssh <id> -i KEY [ssh options] [command]` adds `IdentitiesOnly`
for you.

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
# prewarm = "node -e 0"
```

With `restore = true`, `taritd` cold-boots one ready golden VM for the class,
takes a snapshot, tears down the builder, then refills that class by restoring
clones from the golden. Before its snapshot the golden runs `true`, so clones
start with the guest shell in their page cache, or the class's `prewarm`
command, which can do the same for an image's runtime; a failing command fails
the golden. Each restored clone receives its own writable CoW overlay; `restore = false` keeps the bounded cold-boot refill path. Refill starts
only below `low_watermark`, fills back to `target`, and uses the optional
low-weight refill cgroup when `TARIT_REFILL_CGROUP`/`refill_cgroup` is set.

Configured classes are not the only ones. taritd keeps a pool for each
registered image (the newest few at startup, and any registered later), and
learns any other shape creates ask for: the first such create boots cold, the
following ones come from a pool built for it. A learned class that goes
unused for `TARIT_WARM_POOL_LEARN_IDLE_SECS` is retired and its golden
snapshot deleted. `GET /v1/warm-pool` lists learned classes with
`"learned": true`.

## Image registry

`taritd image build --oci <ref> --name <name>[:tag]` uses
the VMM to pull the pinned image and inject the configured agent, runs `e2fsck -fy`, stores the
rootfs under `TARIT_IMAGES_DIR`, and registers it in the node-local SQLite DB.
Create requests can use `--image <name>[:tag]` (or JSON `image`) instead of a raw
`rootfs_path`; warm-pool classes can set `image = "name[:tag]"`.

An image can carry a prewarm command, which the golden VM of that image's
warm pool runs before its snapshot. Sandboxes then start with the runtime
already in their page cache: on the reference host the first `node -v` in a
fresh `node:22` sandbox dropped from about 45 ms (22 MiB read from disk) to
13 ms (no reads). Set it at build time with `--prewarm`, or later with
`taritd image prewarm`; a running taritd rebuilds that image's golden within
seconds.

```sh
taritd image build --oci node:20-slim --name node20 --size 2048 --prewarm 'node -e 0'
taritd image prewarm node20 'node -e 0'   # or with no command, to clear it
taritd image ls
taritd vm create --image node20 --vcpus 1 --memory-mib 256
taritd image rm node20
taritd image gc --older-than-days 7 --dry-run
```

Image build verifies OCI descriptor size and SHA-256 before extraction. Layer,
compressed-byte, expanded-byte, entry-count, file-size, and path-length limits
are derived from `--size`; a rejected image is not registered or published.

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
