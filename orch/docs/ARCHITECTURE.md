# Architecture

This document describes the distributed design implemented in `crates/taritd` and `crates/tarit-fleet`. Single-host mode and cluster mode are the same binary; cluster mode is selected by setting `TARIT_DATABASE_URL` and a strong shared `TARIT_PEER_SECRET`.

## Components

| Component | Code | Responsibility |
| --- | --- | --- |
| Public HTTP API | `crates/taritd/src/api.rs` | Client-facing routes, API key auth, cluster placement, owner resolution, and peer forwarding. |
| Internal peer API | `crates/taritd/src/internal.rs` | Node-local execution routes protected by `X-Peer-Secret`. |
| Cluster logic | `crates/taritd/src/cluster.rs` | VM owner lookup, peer RPC lookup, placement candidate selection, ownership map maintenance. |
| Node-local operations | `crates/taritd/src/ops.rs` | Shared local create, restore, stop, pause, resume, snapshot, exec, egress, and get operations. |
| Peer client | `crates/taritd/src/peer.rs` | Hardened blocking HTTP client for `/internal/v1/*` calls. |
| Supervisor | `crates/taritd/src/supervisor.rs` | Spawns one `vmm serve --socket <uds>` child process per sandbox and talks to it over UDS. |
| Warm pool | `crates/taritd/src/warmpool.rs` | Maintains pre-booted VMs for matching create requests. |
| Scheduler | `crates/taritd/src/scheduler.rs` | Local atomic slot reservation and advertised local capacity. |
| Network provisioning | `crates/taritd/src/net.rs` | Optional tap, `/30`, and nftables NAT provisioning per VM. |
| Fleet registry | `crates/tarit-fleet/src/lib.rs` | PostgreSQL membership, VM ownership, and autoscaler lease election. |
| Image registry | `crates/tarit-store/src/lib.rs` + `crates/taritd/src/image.rs` | Node-local golden rootfs metadata, OCI build pipeline, and safe image GC. |
| Autoscaler | `crates/taritd/src/autoscale.rs` | Leader-elected capacity loop with provider-command actuation. |

## Runtime model

Each sandbox maps to one local `vmm` process. `taritd` launches the child as:

```text
vmm serve --socket <TARIT_SOCKET_DIR>/<vm-id>.sock
```

The `tarit-vmm-client` client sends length-prefixed JSON over the UDS. The first 4 bytes are a big-endian `u32` payload length, followed by a JSON request such as `{"op":"pause"}` or `{"op":"create","config":...}`. Frames larger than 16 MiB are rejected by the client library.

## Single-host mode

If `TARIT_DATABASE_URL` is unset, `state.fleet` is `None`:

- Creates run locally.
- Ownership resolution falls back to local SQLite.
- `/v1/cluster` reports the local SQLite host roster.
- No peer forwarding or autoscaler leader election occurs.

This mode is useful for development, a single bare-metal host, or one node behind no cluster load balancer.

## Cluster mode

If `TARIT_DATABASE_URL` is set, `taritd` connects to PostgreSQL and creates these tables if needed:

| Table | Role |
| --- | --- |
| `fleet_hosts` | One row per node: `host_id`, `rpc_addr`, `sandbox_count`, `free_vcpus`, `free_memory_mib`, `healthy`, `last_heartbeat`. |
| `fleet_vms` | VM ownership map: VM UUID to owning `host_id`, plus VM shape and timestamps. |
| `fleet_leader` | Single-row lease used by the autoscaler. |

PostgreSQL is the source of truth for node membership and VM ownership in cluster mode. Gossip is not used for ownership. A node also keeps a local SQLite store for its own VM records and execution records.

Every node runs a 5 second heartbeat loop. It writes its current local capacity to `fleet_hosts` and copies the PostgreSQL host roster into the local SQLite cache. Placement and cluster status treat a host as usable only when `healthy = true` and `last_heartbeat` is fresher than about 15 seconds.

## Ownership model

`fleet_vms` maps a VM to exactly one owning `host_id`. The owning node is the only node that has the VMM child process and its UDS. Public requests can arrive at any node, but operations that touch a VM are executed by the owner.

Owner resolution in cluster mode:

1. Look up `fleet_vms(id)` in PostgreSQL.
2. If the owner is the current node, execute locally.
3. If the owner is another node, look up that node's `rpc_addr` in `fleet_hosts` and forward to `/internal/v1/*`.
4. If the fleet lookup fails or misses, fall back to the local SQLite store so a node can still answer for VMs it owns.
5. If neither source knows the VM, return 404.

Ownership writes are best effort. A successful local create or restore writes the local SQLite record and then calls `fleet.upsert_vm`. If that fleet write fails, the local VM still exists and a warning is logged. Operators should alert on fleet write failures because peer routing depends on the map.

## Request flow: any node to owner

```text
client
  |
  | X-API-Key
  v
load balancer
  |
  v
node B public API
  |
  | resolve_owner(vm_id)
  |   SELECT host_id FROM fleet_vms WHERE id = vm_id
  |   SELECT rpc_addr FROM fleet_hosts WHERE host_id = 'node A'
  v
node B peer client
  |
  | X-Peer-Secret
  | PATCH/POST/GET/DELETE http://node-a:8080/internal/v1/...
  v
node A internal API
  |
  | ops::*_local
  v
node A VmmSupervisor -> UDS -> vmm child process
  |
  v
response returns through node B to client
```

This applies to get, delete, pause, resume, snapshot, egress update, and the command execution part of `POST /v1/execute_async`.

Important operational detail: the execution job record for `POST /v1/execute_async` is stored in the local SQLite database of the public API node that accepted the request. The command can run on a remote owner, but `GET /v1/executions/{id}` does not forward through the cluster. If an external load balancer distributes requests randomly, use stickiness for execution polling or have clients poll the same node that accepted the execution request.

## Placement model

Create uses an exhaustive local-first strategy:

1. Try `ops::create_local` on the node that received the request.
2. If local has a matching warm VM, hand it out immediately.
3. Otherwise atomically reserve a local slot and cold boot.
4. If local is full and PostgreSQL fleet mode is enabled, collect every healthy peer with enough advertised `free_vcpus` and `free_memory_mib`.
5. Sort candidates by least `sandbox_count`, then most `free_memory_mib`.
6. Try each peer in order using `POST /internal/v1/vms`.
7. Retry this local and peer loop until `TARIT_ADMISSION_TIMEOUT_MS` expires.
8. Return 429 with `Retry-After` only when the whole visible cluster remains full for that admission window.

### Request flow: local full to peer placement

```text
client -> load balancer -> node A POST /v1/vms
                           |
                           | ops::create_local
                           |   scheduler.try_reserve() == false
                           v
                         node A queries fleet_hosts
                           |
                           | candidates: healthy, fresh, rpc_addr set,
                           | enough free_vcpus and free_memory_mib,
                           | excluding node A
                           v
                         node A tries peers best-first
                           |
                           | POST /internal/v1/vms to node B
                           | if 429 or 404, try node C
                           | if other error, log and try next
                           v
                         first accepting peer returns VmRecord
```

Capacity is intentionally conservative. `Scheduler::try_reserve` is atomic per process and prevents concurrent creates from overshooting `TARIT_MAX_VMS`. Warm pool reservations use the same slot counter, so warm VMs plus assigned VMs respect the local VM cap.

Create requests may specify `image: "name[:tag]"` instead of `rootfs_path`.
Each node resolves that reference through its local SQLite image registry to a
rootfs path before warm matching or cold boot. Operators should build/register
the same image names on every placement host.

## Restore and snapshots

Snapshots are node-local files produced by the owner through the VMM UDS. Public snapshot responses include:

```json
{
  "path": "/path/on/owner/snapshot",
  "host_id": "node-a"
}
```

`POST /v1/restore` accepts `snapshot_path`, optional `host_id`, and optional new `id`. If `host_id` is present and not the receiving node, the request is routed to that host using `cluster::peer_rpc`. The file is not copied across nodes.

Restore behavior:

- Restore succeeds only on the node that can read `snapshot_path`.
- The restored `VmRecord` currently uses `memory_mib: 0`, `vcpus: 0`, `kernel_path: "(restored)"`, and `cmdline: "(restored)"` because the code does not reconstruct shape metadata from the snapshot.
- Restored VMs write ownership to `fleet_vms` like newly created VMs.

A disaster-recovery or cross-zone restore design can store snapshots in shared storage or object storage, put the URI in the snapshot response, and make restore download or mount the artifact on the selected target node.

## Membership and health

The fleet heartbeat runs every 5 seconds in `spawn_fleet_sync`. Each heartbeat writes:

- `host_id`
- `rpc_addr`
- `sandbox_count`
- `free_vcpus`
- `free_memory_mib`
- `healthy = true`
- `last_heartbeat = now()`

Placement and `/v1/cluster` consider a node up only if the row is healthy and less than 15 seconds old. This tolerates a missed heartbeat without quickly flapping nodes out of placement.

`TARIT_REGION`, `TARIT_ZONE`, and `TARIT_CLOUD` are included in autoscaler decisions, but they are not persisted per host in `fleet_hosts`. Region-aware placement would need additional fleet columns or an external provider inventory.

## Consistency model

The design is simple and mostly eventually consistent:

- PostgreSQL is authoritative for VM ownership and node membership in cluster mode.
- A node's local SQLite store is authoritative for records and executions created on that node.
- Fleet ownership writes after local create and restore are best effort. A write failure can make a live VM unreachable through peers until repaired.
- Host capacity is heartbeat based and can be stale for up to about 15 seconds.
- Peer create handles races by treating peer 429 as capacity backpressure and preserving 409 for duplicate requested IDs or other state conflicts.
- Delete clears `fleet_vms` after stopping the local VM. The local SQLite VM row is marked `stopped`, not deleted.

## Failure handling

| Failure | Behavior | Operator action |
| --- | --- | --- |
| Node misses heartbeats | Excluded from placement and shown as `up: false` after about 15 seconds. | Investigate node and VMM child processes. |
| Owner node down | Requests routed by `fleet_vms` can fail with peer HTTP errors until the node returns or ownership is repaired. | Restore from a node-local snapshot on that node, or use shared snapshot storage if implemented externally. |
| PostgreSQL unavailable | Owner lookup falls back to local only; cross-node placement returns no candidates. Heartbeat logs warnings. | Restore PostgreSQL service. Do not rely on new cross-node placement while unavailable. |
| Peer full during placement | Peer returns 429, caller tries the next candidate. | No action unless all peers are full. |
| Whole cluster full | Create waits up to `TARIT_ADMISSION_TIMEOUT_MS`, then returns 429 with `Retry-After`. | Add capacity or enable autoscaling. |
| Fleet ownership write fails | VM keeps running locally, but other nodes may not find it. | Alert on logs and reconcile ownership. |
| Snapshot restore sent to wrong node | `host_id` in restore routes to the right node if available. Without it, restore is local only. | Preserve and pass `host_id` from snapshot responses. |

## Security boundaries

- Public API routes require `X-API-Key`.
- Peer routes require `X-Peer-Secret`.
- Cluster mode refuses the built-in `dev-peer-secret`.
- Peer URLs are not user supplied. They come from `fleet_hosts.rpc_addr`.
- The peer HTTP client disables redirects to avoid redirect-based SSRF.
- Production deployments should use mTLS between peers, restrict peer ports to private networks or security groups, and terminate public TLS at the load balancer or at `taritd` through a sidecar/proxy.
