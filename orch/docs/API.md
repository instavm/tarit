# API reference

This document reflects the Rust handlers in `crates/taritd/src/api.rs` and `crates/taritd/src/internal.rs`. The [OpenAPI contract](../openapi.yaml) lists public operations and wire schemas. This guide explains selected endpoints and fleet behavior.

## Authentication

| Surface | Header | Applies to |
| --- | --- | --- |
| Public API | `X-API-Key: <api key>` | All `/v1/*` routes. Keys resolve to a tenant and role. |
| PTY WebSocket | `?token=<connect_token>` | `/v1/vms/{id}/pty/{pty_id}/connect`. The session token comes from the create-session response and expires after 5 minutes without a connection. |
| Peer API | target-bound request HMAC derived from `TARIT_PEER_SECRET` | All `/internal/v1/*` routes. The shared key is never sent. |
| Unauthenticated | none | `/health`, `/livez`, `/startupz`, `/readyz`, `/openapi.yaml`, `/docs`. |

Error responses from public handlers use:

```json
{ "error": "message" }
```

The peer HMAC middleware returns a bare `401 Unauthorized` with no JSON body.

API keys are configured either with legacy `TARIT_API_KEY` (tenant `default`,
role `admin`, unlimited VMs), `TARIT_API_KEYS="key:tenant:role:max_vms,..."`,
or `[api_keys]` in `TARIT_CONFIG`:

```toml
[api_keys]
"key1" = { tenant = "tenantA", role = "user", max_vms = 20 }
"key2" = { tenant = "tenantB", role = "admin", max_vms = 0 }
```

Roles are `admin` or `user`; `max_vms = 0` means unlimited. Raw keys are hashed
in memory before comparison. User keys only see and act on their tenant's VMs;
admin keys can call admin-only routes such as `/v1/cluster`.

## Common data types

### `VmRecord`

```json
{
  "id": "uuid",
  "status": "creating|running|paused|suspended|hibernated|stopped|error",
  "revision": 3,
  "startup_path": "cold|warm|snapshot_restore",
  "memory_mib": 256,
  "vcpus": 1,
  "created_at": "2026-07-02T00:00:00Z",
  "updated_at": "2026-07-02T00:00:01Z"
}
```

This public representation deliberately omits the physical host id, tenant
ownership metadata, kernel/rootfs/socket paths, kernel command line, and VMM
process id. Those fields exist only in node persistence and authenticated peer
RPC. Successfully deleted records are not returned by list or get.

### `LiveVmStatus`

Returned by the public status endpoint after sanitizing the owning VMM's live
status. It is not the stored record returned by `GET /v1/vms/{id}`.

```json
{
  "state": "created|running|paused|suspended|stopped",
  "uptime_ms": 1037,
  "vcpus": 1,
  "mem_mib": 256,
  "vcpu_alive": true
}
```

### `ExecutionRecord`

```json
{
  "id": "uuid",
  "vm_id": "uuid",
  "command": "echo hello",
  "timeout_ms": 30000,
  "status": "pending|running|completed|failed",
  "exit_code": 0,
  "stdout": "hello\n",
  "stderr": "",
  "duration_ms": 42,
  "error": null,
  "created_at": "2026-07-02T00:00:00Z",
  "updated_at": "2026-07-02T00:00:01Z"
}
```

When pending or running, result fields are usually `null`.

## Public endpoints

### `GET /health`

No authentication. Used by load balancers.

Response `200`:

```json
{ "status": "ok" }
```

### `GET /metrics`

Requires an admin API key. Returns Prometheus text exposition metrics for the
local `taritd` process. Includes `taritd_tenant_vms{tenant="..."}` for local
active VM counts by tenant, plus bounded PTY metrics for active connections,
configured global/tenant/VM limits, and admission rejections by scope. The
`tenant` and per-VM `vm_id` labels are a stable short hash by default so scraping
cannot enumerate tenant or VM identities; set
`TARIT_METRICS_EXPOSE_TENANT_LABELS=1` to expose raw values on a trusted network.
PTY samples are `taritd_pty_active_connections`,
`taritd_pty_connection_limit{scope="..."}`, and
`taritd_pty_admission_rejections_total{scope="..."}`; their scope label is one
of `global`, `tenant`, or `vm` and never contains an identity.

Response `200`: `text/plain; version=0.0.4`.

### `GET /openapi.yaml`

No authentication. Returns the bundled OpenAPI YAML with `http://localhost:8080` rewritten from the request `Host` header.

Response `200`: `application/yaml`.

### `GET /docs`

No authentication. Returns Swagger UI HTML that loads `/openapi.yaml`.

Response `200`: `text/html`.

### Port shares

Port-share control routes use `X-API-Key` and are separate from the guest
gateway. The gateway listener requires `TARIT_SHARE_LISTEN`,
`TARIT_SHARE_DOMAIN`, and `TARIT_SHARE_TOKEN_KEY`; it accepts requests for
`<slug>.<TARIT_SHARE_DOMAIN>` and is not an API-key route. Disabling only
`TARIT_SHARE_LISTEN` leaves the control routes below available on the normal
API listener. See [CONFIGURATION.md](CONFIGURATION.md) and
`deploy/Caddyfile.shares.example`.

`ShareRecord`:

```json
{
  "id": "uuid",
  "slug": "lowercase-dns-label",
  "owner_key": "tenant-a",
  "vm_id": "uuid",
  "guest_port": 8080,
  "visibility": "private",
  "token_version": 0,
  "revoked_at": null,
  "created_at": "2026-07-12T00:00:00Z",
  "updated_at": "2026-07-12T00:00:00Z"
}
```

Visibility is `private` or `public`; omitted create visibility defaults to
`private`, while omitted update visibility preserves the existing value.
`guest_port` must be in `1..=65535`. Create and update bodies reject unknown
fields.

| Method | Path | Success | Behavior |
| --- | --- | --- | --- |
| `POST` | `/v1/shares` | `201 ShareRecord` | Create a share for a running VM the caller can access. |
| `GET` | `/v1/shares` | `200 ShareRecord[]` | List shares owned by the caller's tenant. |
| `GET` | `/v1/shares/{id}` | `200 ShareRecord` | Get a share owned by the caller's tenant, or any share with an `admin` API key. |
| `PATCH` | `/v1/shares/{id}` | `200 ShareRecord` | Update the VM, guest port, and/or visibility. |
| `DELETE` | `/v1/shares/{id}` | `204` | Revoke a share. |
| `POST` | `/v1/shares/{id}/tokens` | `200 ShareTokenResponse` | Issue a private-share gateway token. |

User API keys can access only their tenant's existing shares. An `admin` API
key can get, update, revoke, or issue a token for an existing share in any
tenant; listing remains scoped to the caller's tenant.

Create request:

```json
{
  "vm_id": "uuid",
  "guest_port": 8080,
  "visibility": "private"
}
```

Update request fields are all optional:

```json
{
  "guest_port": 3000,
  "visibility": "public"
}
```

The replacement `vm_id`, when supplied, must identify a running VM accessible
to the caller and owned by the same tenant as the share. A revoked share cannot
be updated. Changing `vm_id`, `guest_port`, or `visibility` increments
`token_version` and invalidates all previously issued private-share tokens.
Revocation also increments `token_version`, sets `revoked_at`, removes the
share from the gateway, and is idempotent for an authorized caller.

Only active private shares can issue tokens:

```json
{
  "token": "base64url-payload.base64url-signature",
  "expires_at": "2026-07-12T00:05:00Z"
}
```

Use the token only in the guest request header:

```text
X-Tarit-Share-Token: <token>
```

The gateway rejects absent, malformed, duplicate, expired, or
token-version-mismatched private tokens with `401`; revoked shares return
`404` before token verification. Tokens expire at
`expires_at`, which is issuance time plus `TARIT_SHARE_TOKEN_TTL_SECS`. Public
shares do not need a token; share tokens are not accepted in query parameters.
For `POST /v1/shares/{id}/tokens`, `400` means an invalid identifier or a
request for a public or revoked share; an unknown share is `404`.

Share control error bodies are JSON: `{ "error": "..." }`. Across the six
control operations, `400` means an invalid identifier or request body (plus
the public/revoked token cases above), `401`
means a missing or invalid API key, `403` means another tenant's share or VM,
`404` means an unknown share or VM, `409` means a revoked share, non-running VM,
or mutation conflict, and `503` means the share owner, share service, or audit
service is unavailable. `GET /v1/shares` can return `200`, `401`, or `503`;
other statuses apply only where their operation can produce that condition.

### `POST /v1/vms`

Create a VM owned by the caller's tenant. The receiving node tries local warm or cold capacity first, then all healthy peers with capacity. If the tenant is at its configured VM quota, the response is `403 Forbidden`. If the whole visible cluster remains full for the admission window, it returns `429 Too Many Requests` with `Retry-After: <seconds>`.

Request:

```json
{
  "memory_mib": 256,
  "vcpus": 1,
  "image": "registered-image:tag",
  "egress": { "allowlist": ["0.0.0.0/0:443/tcp", "10.0.0.2/32:53/udp"], "allow_existing": false }
}
```

Defaults from `tarit-types` and `Config`:

| Field | Default |
| --- | --- |
| `id` | generated UUID |
| `memory_mib` | `256` |
| `vcpus` | `1` |
| `kernel_path` | `TARIT_KERNEL`; admin-only override |
| `image` | unset; when present it resolves through the node-local image registry and cannot be combined with `rootfs_path` |
| `rootfs_path` | `TARIT_ROOTFS`; admin-only override; empty string disables rootfs |
| `cmdline` | default virtio block kernel cmdline when rootfs exists, otherwise `console=ttyS0 panic=1` |
| `egress` | unset. With `TARIT_ENABLE_NET=1` a VM can reach nothing until it has an egress policy; set one here (same form as `PATCH /v1/egress/vm/{id}`) to create the VM with it. An invalid policy is rejected with `400` before anything is created, and if it cannot be applied the new VM is removed and the create fails. |

A fork inherits its source's egress policy. Networked guests get the resolvers
in `TARIT_GUEST_DNS`; allow them in the egress policy (for example
`<resolver>/32:53/udp`) for name resolution to work.

Response `201`: `VmRecord`.

Status codes:

| Status | Meaning |
| --- | --- |
| `201` | VM created locally or on a peer. |
| `400` | Malformed request body or other bad request. |
| `401` | Missing or wrong `X-API-Key`. |
| `403` | Tenant VM quota reached. |
| `409` | Requested VM id already exists or another genuine state conflict occurred. |
| `429` | Cluster stayed at capacity until `TARIT_ADMISSION_TIMEOUT_MS`; includes `Retry-After` in seconds. |
| `500` | VMM, peer, fleet, or internal failure. |

### `GET /v1/vms`

Return an array of public [VM records](#vmrecord). Fleet mode reads
PostgreSQL across nodes. Single-host mode reads the local cache. User keys
see their tenant’s records, admin keys see all tenants, and both omit stopped VMs.
Physical host IDs and paths stay private.

Status codes are `200`, `401` and `500`.

### `GET /v1/vms/{id}`

Resolve owner through the fleet registry and return the VM record from the owner.

User keys can only read VMs owned by their tenant; otherwise the response is
`403 Forbidden`. Admin keys can read any tenant's VM.

Response `200`: `VmRecord`.

Status codes:

| Status | Meaning |
| --- | --- |
| `200` | VM found. |
| `401` | Missing or wrong `X-API-Key`. |
| `403` | VM belongs to a different tenant. |
| `404` | No visible active VM with that ID. |
| `500` | Internal store or protocol failure. Details are not exposed. |
| `503` | The owning host is unhealthy, stale, or unavailable. |

### `GET /v1/vms/{id}/status`

Resolve owner through the fleet registry and query the owning VMM over its Unix socket for live status.

Response `200`: `LiveVmStatus`.

This differs from `GET /v1/vms/{id}`: `/status` reports sanitized live state,
uptime, shape, and `vcpu_alive`; it does not expose the VMM's kernel path or
device configuration.

Status codes: `200`, `401`, `403`, `404`, `409` (VM is stopped), `500`, `503`.

### SSH keys

All SSH key records are scoped to the caller's tenant.
RSA keys remain valid for guest authorized-key injection but cannot authenticate
to the SSH gateway.

`POST /v1/ssh-keys`

Request:

```json
{ "public_key": "ssh-ed25519 AAAA... comment" }
```

Response `201`:

```json
{
  "id": "uuid",
  "fingerprint": "SHA256:...",
  "key_type": "ssh-ed25519",
  "created_at": "2026-07-02T00:00:00Z"
}
```

`GET /v1/ssh-keys` response `200`:

```json
{ "keys": [] }
```

`DELETE /v1/ssh-keys/{key_id}` response `204`: no body.

Status codes: `200`, `201`, `204`, `400`, `401`, `404`, `500`.

### PTY sessions and WebSocket attach

PTY routes operate on the VM's owning node. If a request lands on a non-owner, these routes return `409`; connect to the owner node or use load-balancer stickiness for PTY sessions.

`POST /v1/vms/{id}/pty/sessions`

Request:

```json
{ "cols": 80, "rows": 24, "shell": "/bin/bash" }
```

Response `201`:

```json
{ "pty_id": "uuid", "cols": 80, "rows": 24, "connect_token": "..." }
```

`connect_token` is a per-session secret. Pass it as the `token` query parameter
when attaching over the WebSocket route below. It expires five minutes after
creation or the most recent disconnect.

Additional REST routes:

| Method | Path | Response |
| --- | --- | --- |
| `GET` | `/v1/vms/{id}/pty/sessions` | `{ "sessions": [...] }` |
| `GET` | `/v1/vms/{id}/pty/sessions/{pty_id}` | PTY session record |
| `DELETE` | `/v1/vms/{id}/pty/sessions/{pty_id}` | `204` no body |
| `POST` | `/v1/vms/{id}/pty/sessions/{pty_id}/resize` | `{ "pty_id": "uuid", "cols": N, "rows": N }` |

`WS /v1/vms/{id}/pty/{pty_id}/connect?token=<connect_token>` upgrades to a WebSocket. It authenticates with the session's `connect_token`, not the API key. One connection may be active for a session at a time; after disconnect, the same session may reconnect for another five minutes. Before upgrading, taritd reserves the configured global, authenticated-tenant, and VM active-connection capacity. Capacity rejection returns HTTP `429` without invalidating the token. Binary messages are raw PTY bytes. Text messages are JSON controls: client-to-server `{"type":"resize","cols":N,"rows":N}` and server-to-client `{"type":"exit","exit_code":N}`. VM teardown removes all of its pending and connected session records.

Status codes: `200`, `201`, `204`, `400`, `401`, `404`, `409`, `429`, `500`. A bad, missing, expired, or unknown connect token is rejected with HTTP `401` before upgrade. A second concurrent connection or deletion of a connected session returns `409`. Post-upgrade failures close the socket with `1013` when the VM is unavailable on this node and `1011` for attach or stream errors.

### `DELETE /v1/vms/{id}`

Resolve owner, stop the VM on its owner, mark the owner's local record as `stopped`, release the local scheduler slot, and remove the VM ownership row from `fleet_vms`.

Response `204`: no body.

Status codes: `204`, `401`, `403`, `404`, `500`.

Note: the local SQLite VM row is not deleted. On the owner, a later local `GET` can still find the stopped record. Other nodes generally cannot find it after `fleet_vms` is cleared.

### `POST /v1/vms/{id}/pause`

Resolve the owner, stop every vCPU, drain and park the block, network, and vsock
workers, then publish the paused state. The public handler does not require a
JSON body.

If a vCPU or device-worker transition fails and the VMM cannot confirm a safe
rollback, it fences the VM paused. The orchestrator observes and durably
records that state before returning the operation failure, allowing a later
explicit resume or delete instead of leaving the control plane marked running.

Response `200`: updated `VmRecord` with `status: "paused"`.

Status codes: `200`, `401`, `403`, `404`, `409` (invalid lifecycle
transition), `500`.

### `POST /v1/vms/{id}/suspend`

Resolve the owner and capture a coherent in-process suspend image after every
vCPU and guest-memory-writing device worker acknowledges quiescence. Resident
guest RAM is released, but VM ownership, the VMM process, scheduler capacity,
and tenant quota remain reserved.

Response `200`: updated `VmRecord` with `status: "suspended"`.

Status codes: `200`, `401`, `403`, `404`, `409` (invalid lifecycle
transition), `500`.

### `POST /v1/vms/{id}/hibernate`

Capture and authenticate a live RAM, device, and private-disk artifact, satisfy
the configured replication policy, stop the resident VMM, and release CPU,
memory, cgroup, network, and scheduler capacity. The logical VM and its tenant
ownership remain durable for later activation.

Response `200`: updated `VmRecord` with `status: "hibernated"`.

Status codes: `200`, `401`, `403`, `404`, `409` (VM is not running), `500`,
`503` (durable artifact or peer lifecycle requirements are unavailable).

### `POST /v1/vms/{id}/resume`

Resolve the owner and resume a paused or suspended VM. A hibernated VM is
activated through the fenced single-flight restore path, including normal
placement, artifact verification, network repair, and policy restoration.
Device workers must leave their parked state before vCPUs restart, and the
operation returns only after guest readiness succeeds. The public handler does
not require a JSON body.

Response `200`: updated `VmRecord` with `status: "running"`.

Status codes: `200`, `401`, `403`, `404`, `409` (invalid lifecycle state),
`429` (no placement capacity), `500`, `503` (owner or restore prerequisites are
unavailable).

### `POST /v1/vms/{id}/snapshot`

Resolve the owner and publish an authenticated snapshot behind an opaque UUID.
Host paths, physical host identity, and artifact locators remain private.

Every snapshot is a standalone full snapshot: it can be restored, forked or
deleted without any other snapshot. Capture is incremental anyway. After the
VM was restored or woken from a snapshot, or after its last snapshot on a node
without VM jails, the guest is paused only to write the memory pages it changed
since then; the rest is copied from that earlier snapshot after the guest
resumes. `diff: true` is accepted and behaves the same way.

Request:

```json
{ "diff": false }
```

`diff` defaults to `false`.

Response `200`:

```json
{
  "snapshot_id": "uuid"
}
```

Status codes: `200`, `401`, `403`, `404`, `409` (the lifecycle state does not
support snapshots), `429` (storage pressure or a busy disk, retry after the specified delay), `500`.

### `POST /v1/vms/{id}/fork`

Fork a running source. Supply `{"id":"<child UUID>"}` to retry safely with the
same child ID. The response contains `source_vm_id` and the child’s public
`vm` record. A new child returns `201`. Replaying its committed request
returns `200`. A healthy non-owner node can run the child after verifying the
source artifact and meeting replica policy. Workload access waits for repair.

### `GET /v1/snapshots`

List the caller's snapshots, oldest first. Snapshots owned
by a fork or used as a hibernated VM's wake image are managed by that VM and
are not listed.

Snapshots are always scoped to the caller's own tenant, for `admin` keys too:
unlike VMs and shares, an admin key cannot list another tenant's snapshots
(owner decision, 2026-11-13).

Response `200`:

```json
[
  {
    "snapshot_id": "uuid",
    "source_vm_id": "uuid",
    "created_at": "2026-11-06T07:52:00Z",
    "size_bytes": 41943040,
    "memory_mib": 256,
    "vcpus": 1
  }
]
```

In cluster mode the list covers every node that holds one of the caller's
snapshots. A node that cannot answer (down, unhealthy, stale, deregistered,
running a version without the peer snapshot route, or silent for 10 seconds)
does not fail the request:
the response is still `200` with the snapshots every other node returned, and
carries the header `X-Tarit-Unavailable-Hosts: <n>`, the number of nodes holding
some of the caller's snapshots that could not be asked. The header is absent
when the list is complete, so a client that needs the full list should retry
while it is present. It counts only nodes that hold the caller's own snapshots,
and node ids are not exposed (the server log names them). The body stays a
plain array so existing clients keep working.
A node that is retired for good (its `fleet_hosts` row removed) is not
counted: its snapshot locators are dropped within about a minute, see
[AUTOSCALING.md](AUTOSCALING.md#drain-and-scale-in-safety).

Status codes: `200` (complete, or partial with `X-Tarit-Unavailable-Hosts`),
`401`, `503` (fleet store unavailable).

### `DELETE /v1/snapshots/{id}`

Delete a snapshot and its files. Snapshots are kept until deleted, so a client
that checkpoints often should delete the ones it no longer needs; otherwise
the node eventually reaches disk pressure and refuses new snapshots with `429`.
A snapshot that a branch head references, a hibernated VM would wake from, or
a fork owns is refused. Delete the branch first to release a branch head.

Like listing, deletion is scoped to the caller's own tenant for `admin` keys
too. Another tenant's snapshot answers exactly like one that does not exist.

Status codes: `204`, `401`, `404` (unknown, or another tenant's), `409`
(still referenced), `422` (held by another node), `503` (fleet store
unavailable).

### `POST /v1/restore`

Restore a VM from an opaque tenant-owned snapshot handle. The control plane
resolves its private locator, verifies the authenticated artifact, and routes
the restore to the node that holds it. Clients cannot provide a host, path, or
storage locator.

Request:

```json
{
  "snapshot_id": "5746f422-d767-4f01-82cf-20e71d76c57a"
}
```

The server generates the new VM ID unless you supply `id`. Eligible live
snapshots can grow within their reserved maximum using `target_memory_mib`.
[Memory growth requirements](../../docs/memory-growth.md)

Response `201`: `VmRecord`.

Status codes:

| Status | Meaning |
| --- | --- |
| `201` | VM restored and ready. |
| `401` | Missing or wrong `X-API-Key`. |
| `403` | Tenant VM quota is reached. |
| `404` | Snapshot handle not found or belongs to another tenant. |
| `409` | Requested VM id already exists. |
| `429` | The snapshot-owning node has no capacity; includes `Retry-After` in seconds. |
| `500` | Internal restore failure. |
| `503` | The snapshot-owning node is unhealthy, stale, or unavailable. |

### `POST /v1/execute`

Resolve the VM owner and run a command synchronously: one request returns the finished execution record, no polling. Use this for low-latency request/response exec; use `POST /v1/execute_async` plus `GET /v1/executions/{id}` when you would rather poll.

Request:

```json
{
  "vm_id": "uuid",
  "command": "echo hello",
  "timeout_ms": 30000
}
```

`timeout_ms` defaults to `30000` and must not exceed `3600000` or the server's API request deadline (`TARIT_API_REQUEST_TIMEOUT_MS`, default `180000`); use `POST /v1/execute_async` for longer commands. A request that cannot start its command in time because another operation holds the VM returns `409` instead of outliving that deadline.

Response `200`: final `ExecutionRecord` with `status: "completed"` (result fields `exit_code`, `stdout`, `stderr`, `duration_ms` set) or `status: "failed"` (`error` set). A command that runs but exits non-zero is still `completed`; check `exit_code`. An exec that could not run at all (for example, guest agent unavailable) returns `200` with `status: "failed"`, not an HTTP error.

Status codes:

| Status | Meaning |
| --- | --- |
| `200` | Execution finished; the record carries the outcome (completed or failed). |
| `400` | `timeout_ms` is above `3600000` or the API request deadline. |
| `401` | Missing or wrong `X-API-Key`. |
| `403` | VM belongs to a different tenant. |
| `404` | VM not found. |
| `409` | VM is not running, or stayed busy with another operation for too long. |
| `500` | Owner resolution, peer, or internal failure. |

### `POST /v1/execute_async`

Run a command asynchronously on the VM owner and return an execution ID.

Request:

```json
{
  "vm_id": "uuid",
  "command": "echo hello",
  "timeout_ms": 30000
}
```

`timeout_ms` defaults to `30000` and must not exceed `3600000`.

Response `202`: initial `ExecutionRecord` with `status: "pending"`.

Status codes:

| Status | Meaning |
| --- | --- |
| `202` | Execution accepted. |
| `400` | `timeout_ms` is above `3600000`. |
| `401` | Missing or wrong `X-API-Key`. |
| `403` | VM belongs to a different tenant. |
| `404` | VM not found. |
| `500` | Store, peer, or VMM failure. |

### `GET /v1/executions/{id}`

In fleet mode, any healthy API node can read the execution record from PostgreSQL. A standalone node reads its local record.

Response `200`: `ExecutionRecord`.

Status codes: `200`, `401`, `403`, `404`, `500`.

### `PATCH /v1/egress/vm/{id}`

Resolve owner and update the running VM's egress allowlist through the VMM.

Request:

```json
{
  "allowlist": ["10.0.0.0/8:443/tcp"],
  "allow_existing": true
}
```

`allow_existing` defaults to `false`. Entries entirely inside IPv4 link-local `169.254.0.0/16` are rejected with `400`: the host always drops that range.

Response `200`:

```json
{ "rules_applied": 1 }
```

Status codes: `200`, `401`, `403`, `404`, `409` (VM is stopped), `500`.

### `GET /v1/cluster`

Admin-only. Return cluster capacity and health. In cluster mode, data comes from PostgreSQL `fleet_hosts`. In single-host mode, data comes from the local SQLite host roster.

Response `200`:

```json
{
  "this_host": "node-a",
  "clustered": true,
  "total_nodes": 3,
  "healthy_nodes": 2,
  "cluster_free_vcpus": 10,
  "cluster_free_memory_mib": 24576,
  "nodes": [
    {
      "host_id": "node-a",
      "rpc_addr": "http://10.0.1.10:8080",
      "sandbox_count": 4,
      "free_vcpus": 12,
      "free_memory_mib": 32768,
      "up": true,
      "last_heartbeat": "2026-07-02T00:00:00Z"
    }
  ]
}
```

`healthy_nodes`, `cluster_free_vcpus`, and `cluster_free_memory_mib` include only nodes with `healthy = true` and a heartbeat fresher than about 15 seconds.

Status codes: `200`, `401`, `403`, `500`.

### `GET /v1/usage`

Aggregated per-key usage stats from the primary store, or from the node's local outbox when no fleet database is configured. Admins see every key; a non-admin key sees only its own. Query parameters: `from`, `to` (RFC3339; default last 30 days), and `api_key_id` (admin only).

```json
[
  { "api_key_id": "a9842b8c...c385aa", "owner_key": "default", "vm_runtime_seconds": 11.05, "exec_count": 1, "exec_duration_ms": 1 }
]
```

Status codes: `200`, `401`, `500`.

### `GET /v1/audit`

Recent audit trail, newest first, from the primary store or, without a fleet database, the node's local outbox. Admins see every key; a non-admin key sees only its own. Query parameters: `api_key_id` (admin only), `vm_id`, `limit` (default 100, max 1000).

```json
[
  { "id": "…", "api_key_id": "a9842b8c...c385aa", "owner_key": "default", "host_id": "node-a", "vm_id": "33827851-…", "action": "exec", "outcome": "ok", "detail": null, "created_at": "2026-07-03T00:00:00Z" }
]
```

Both endpoints record raw stats only. See [USAGE-AND-AUDIT.md](USAGE-AND-AUDIT.md).

Status codes: `200`, `401`, `500`.

## Internal peer API

Fleet peers use a dedicated private listener with mutual TLS by default.
Each request carries a short-lived HMAC bound to the request and peer
identities. The receiver checks the boot sessions and rejects replayed
requests. The shared key stays on the nodes. [Peer configuration](CONFIGURATION.md#cluster-and-durability)

### Selected internal endpoints

| Method | Path | Body | Success | Purpose |
| --- | --- | --- | --- | --- |
| POST | `/internal/v1/vms` | `CreateVmRequest` | `201 VmRecord` | Create on this node only. Returns 429 + `Retry-After` if this node is full. |
| POST | `/internal/v1/restore` | Private restore request | `201 VmRecord` | Restore on this node only. |
| GET | `/internal/v1/vms/{id}` | none | `200 VmRecord` | Get from this node's local store. |
| GET | `/internal/v1/vms/{id}/status` | none | `200 LiveVmStatus` | Query live status from this node's local VMM. |
| DELETE | `/internal/v1/vms/{id}` | none | `204` | Stop on this node and clear local/fleet ownership. |
| POST | `/internal/v1/vms/{id}/exec` | `{"command":"...","timeout_ms":30000}` | `200 {exit_code,stdout,stderr,duration_ms}` | Execute on this node's VM. |
| POST | `/internal/v1/vms/{id}/pause` | none | `200 VmRecord` | Pause local VM. |
| POST | `/internal/v1/vms/{id}/resume` | none | `200 VmRecord` | Resume local VM. |
| POST | `/internal/v1/vms/{id}/snapshot` | `{"diff":false}` | `200 {snapshot_id,live_snapshot?}` | Snapshot local VM. |
| PATCH | `/internal/v1/vms/{id}/egress` | `EgressUpdateRequest` | `200 {rules_applied}` | Update local VM egress. |

Internal handlers call the same `ops::*_local` functions as public handlers, so local behavior is shared. Peer-forwarded public requests carry the resolved caller identity in the `X-Tarit-Tenant`, `X-Tarit-Role`, and `X-Tarit-Api-Key-Id` headers so the owner node can enforce tenant access before local VM operations. Requests without a valid tenant and role are rejected with `401`; `X-Tarit-Api-Key-Id` may be empty. They intentionally do not perform owner resolution.

## Status code summary

| Status | Meaning |
| --- | --- |
| `200` | Successful read, pause, resume, snapshot, egress update, synchronous execute, or cluster status. |
| `201` | Successful create or restore. |
| `202` | Execution accepted. |
| `204` | Successful delete/stop. |
| `400` | Bad request. Usually malformed JSON or invalid payload shape. |
| `401` | Missing or invalid API credentials or peer request signature. |
| `403` | Authenticated caller is not allowed to access the tenant resource, lacks admin role, or exceeded tenant VM quota. |
| `404` | VM, execution, owner host, or snapshot owner not found. |
| `409` | Genuine state conflict, such as a duplicate requested VM id or an operation invalid for the current VM state. |
| `429` | Capacity or overload backpressure. Create/restore responses include `Retry-After` in seconds. |
| `500` | Internal, VMM, peer, store, or fleet failure. |
