# Architecture

This document describes the distributed design implemented in `crates/taritd` and `crates/tarit-fleet`. Single-host mode and cluster mode are the same binary; cluster mode is selected by setting `TARIT_DATABASE_URL` and a strong shared `TARIT_PEER_SECRET`.

## Components

| Component | Code | Responsibility |
| --- | --- | --- |
| Public HTTP API | `crates/taritd/src/api.rs` | Client-facing routes, API key auth, cluster placement, owner resolution, and peer forwarding. |
| Internal peer API | `crates/taritd/src/internal.rs` | Dedicated peer routes with session-bound HMACs and mutual TLS by default. |
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
| Share control API | `crates/taritd/src/api.rs` + `crates/taritd/src/shares.rs` | Tenant-authorized share lifecycle, token issuance, and durable share records. |
| Share gateway | `crates/taritd/src/share_gateway.rs` | Separate listener that authorizes a share hostname and streams HTTP or WebSocket traffic to its VM guest port. |

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

In fleet mode, PostgreSQL supplies the authoritative owner. A remote owner
must have a fresh heartbeat, a current boot session and a routable peer
address. Missing ownership returns 404. An unavailable registry or stale
owner fails the request. Single-host mode resolves ownership locally.

Lifecycle publication checks revisions and fences ownership writes. Recovery
reconciles the durable record and the actual VMM before reclaiming capacity.

## Port-share gateway

The port-share data plane is opt-in and binds a separate listener configured by
`TARIT_SHARE_LISTEN`; the control listener remains responsible for the
API-key-protected `/v1/shares` lifecycle API. A trusted Caddy or Envoy edge
terminates public TLS and proxies only wildcard share hosts to that listener.
The share listener is private to that edge: direct public access is unsupported
and must be firewalled. Disabling only the share listener does not remove the
control routes from the normal API listener. The edge must preserve the original
`Host`, overwrite rather than append forwarding headers, pass WebSocket
upgrades, and reject `/internal/v1/*` rather than forwarding it.

For a request to `<slug>.<TARIT_SHARE_DOMAIN>`, the gateway requires exactly
one valid Host header, derives the slug from the configured domain, and loads
the active share record. Private shares require the
`X-Tarit-Share-Token` header; public shares do not. The gateway does not
accept a caller-selected upstream address or port. After authorization it
resolves `share.vm_id` through the normal owner map. On the owner, it derives
the target exclusively from the live VM network allocation plus the persisted
`guest_port`.

```text
browser or client
  |
  | TLS, Host: <slug>.<share-domain>
  v
trusted Caddy or Envoy edge
  |
  | preserved Host and one X-Forwarded-Proto value
  v
Tarit share listener
  |
  | authorize slug and, for private shares, X-Tarit-Share-Token
  | resolve VM owner
  v
owning Tarit node
  |
  | live guest network allocation + persisted guest_port
  v
guest service
```

If the owner is remote, Tarit forwards the already-authorized request only
over its authenticated peer path; the public edge never exposes internal peer
routes. The owner rechecks the authoritative share record and owner identity
before deriving its guest target. This keeps public requests independent of
peer addressing and prevents a share record or client header from selecting an
arbitrary upstream.

HTTP request and response bodies stream without full buffering. WebSocket
handshakes, subprotocol negotiation, frames, ping/pong messages, and close
frames are bridged while connection and idle limits are applied. Before traffic
reaches the guest, Tarit removes hop-by-hop, `X-API-Key`,
proxy-authentication, the private-share token, `X-Tarit-*`, and untrusted
forwarding headers; it supplies its own forwarding metadata. It accepts only
one exact lowercase `http` or `https` `X-Forwarded-Proto` value from the
trusted edge; missing, repeated, comma-separated, or invalid values become
`http`. Equivalent unsafe response headers are removed before they reach the
client. Guest applications that need application credentials should use
`Authorization` or another non-reserved header, not `X-API-Key`.

The normal Prometheus `/metrics` output includes bounded share-gateway metrics:
request totals by visibility and status class, authorization, owner-resolution,
and target failures, ingress and egress bytes, plus active HTTP request and
WebSocket gauges. It deliberately does not label metrics with share IDs,
tokens, hostnames, or tenants.

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
  | Session-bound request HMAC over mTLS
  | PATCH/POST/GET/DELETE https://node-a:8443/internal/v1/...
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

In fleet mode the owner runs the command and PostgreSQL stores the execution record. A client can poll `GET /v1/executions/{id}` through any healthy API node.

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

Public snapshot creation returns `{"snapshot_id":"<uuid>"}`. Restore accepts
that handle, an optional new VM ID and an optional memory target for a compatible
template. taritd resolves the private locator and routes ordinary restore to
the holding node. Public responses omit host paths and placement identifiers.

Cross-node live fork has a separate transfer path. The receiving node obtains
the authenticated source artifact, verifies it against the configured replica
policy and publishes the child with its own writable overlay. [Public API](API.md)

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

- In fleet mode PostgreSQL is authoritative for ownership, node membership and execution records. Standalone nodes persist VM and execution records in SQLite.
- Fleet writes check the VM revision and incarnation. Failed publication enters recovery or cleanup before capacity can be reused.
- Host capacity is heartbeat based and can be stale for up to about 15 seconds.
- Peer create handles races by treating peer 429 as capacity backpressure and preserving 409 for duplicate requested IDs or other state conflicts.
- Delete clears `fleet_vms` after stopping the local VM. The stopped record reserves its ID during the configured retention window (24 hours by default).

## Failure handling

| Failure | Behavior | Operator action |
| --- | --- | --- |
| Node misses heartbeats | Excluded from placement and shown as `up: false` after about 15 seconds. | Investigate node and VMM child processes. |
| Owner node down | Owner-routed requests fail when the owner is stale. | Recover the owner or use a supported fenced recovery path. |
| PostgreSQL unavailable | Fleet owner lookup fails and heartbeats report errors. | Restore PostgreSQL service. Do not rely on new cross-node placement while unavailable. |
| Peer full during placement | Peer returns 429, caller tries the next candidate. | No action unless all peers are full. |
| Whole cluster full | Create waits up to `TARIT_ADMISSION_TIMEOUT_MS`, then returns 429 with `Retry-After`. | Add capacity or enable autoscaling. |
| Fleet ownership write fails | Lifecycle publication reports failure and recovery preserves the ownership fence. | Alert on logs and reconcile ownership. |
| Snapshot restore sent to wrong node | taritd resolves the holding node from the snapshot handle. | Pass the opaque `snapshot_id` and check owner availability. |

## Security boundaries

- Public API routes require `X-API-Key`.
- Peer routes require a session-bound HMAC derived from the shared signing key.
- Cluster mode refuses the built-in `dev-peer-secret`.
- Peer URLs are not user supplied. They come from `fleet_hosts.rpc_addr`.
- The peer HTTP client disables redirects to avoid redirect-based SSRF.
- Production mode requires peer mTLS on a dedicated listener. Restrict peer ports to the private fleet network and terminate public TLS at the edge.
