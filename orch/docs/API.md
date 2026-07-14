# API reference

This document reflects the Rust handlers in `crates/taritd/src/api.rs` and `crates/taritd/src/internal.rs`. The served `openapi.yaml` covers all public routes; it does not describe the internal peer routes, and this document remains the more detailed reference for behavior and status codes.

## Authentication

| Surface | Header | Applies to |
| --- | --- | --- |
| Public API | `X-API-Key: <api key>` | All `/v1/*` routes. Keys resolve to a tenant and role. |
| PTY WebSocket | `?token=<connect_token>` | `/v1/vms/{id}/pty/{pty_id}/connect`. The one-time token comes from the create-session response and expires after 5 minutes. |
| Peer API | `X-Peer-Secret: <TARIT_PEER_SECRET>` | All `/internal/v1/*` routes. |
| Unauthenticated | none | `/health`, `/metrics`, `/openapi.yaml`, `/docs`. |

Error responses from public handlers use:

```json
{ "error": "message" }
```

The peer secret middleware returns a bare `401 Unauthorized` with no JSON body.

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
  "host_id": "node-a",
  "status": "creating|running|paused|stopped|error",
  "memory_mib": 256,
  "vcpus": 1,
  "kernel_path": "/path/to/kernel",
  "rootfs_path": "/path/to/rootfs or null",
  "cmdline": "kernel command line",
  "socket_path": "/path/to/vm.sock or null",
  "pid": 12345,
  "created_at": "2026-07-02T00:00:00Z",
  "updated_at": "2026-07-02T00:00:01Z"
}
```

`rootfs_path`, `socket_path`, and `pid` may be `null`. Restored VM records currently use placeholder shape fields: `memory_mib: 0`, `vcpus: 0`, `kernel_path: "(restored)"`, and `cmdline: "(restored)"`.

### `LiveVmStatus`

Returned by the VMM `Status` op. This is live runtime state from the owning VMM process, not the stored orchestrator record returned by `GET /v1/vms/{id}`.

```json
{
  "state": "created|running|paused|suspended|stopped",
  "uptime_ms": 1037,
  "vcpus": 1,
  "mem_mib": 256,
  "volumes": 1,
  "nets": 0,
  "kernel": "/path/to/kernel",
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

No authentication. Returns Prometheus text exposition metrics for the local `taritd` process.
Includes `taritd_tenant_vms{tenant="..."}` for local active VM counts by tenant.

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

Tarit can also terminate share HTTPS in process with an automatically issued and
renewed wildcard certificate for `*.<TARIT_SHARE_DOMAIN>`, so shares are served
at `https://<slug>.<TARIT_SHARE_DOMAIN>` without an external TLS edge. This is
off by default. See the [Wildcard TLS (ACME)](CONFIGURATION.md#wildcard-tls-acme)
configuration section.

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
  "id": "optional uuid",
  "memory_mib": 256,
  "vcpus": 1,
  "kernel_path": "optional admin-only kernel override",
  "image": "optional registered image name[:tag]",
  "rootfs_path": "optional admin-only rootfs override, empty string means no rootfs",
  "cmdline": "optional kernel command line override"
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

List VM records visible to the caller. User keys only see their tenant's VMs; admin keys can see all local VM records.

Response `200`:

```json
[
  { "id": "uuid", "host_id": "node-a", "status": "running" }
]
```

The actual objects are full `VmRecord` values. In cluster mode this endpoint is not a cluster-wide list. It does not aggregate peer stores or read `fleet_vms`.

Status codes: `200`, `401`, `500`.

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
| `404` | VM not found in fleet or local fallback. |
| `500` | Owner host missing `rpc_addr`, peer failure, or store failure. |

### `GET /v1/vms/{id}/status`

Resolve owner through the fleet registry and query the owning VMM over its Unix socket for live status.

Response `200`: `LiveVmStatus`.

This differs from `GET /v1/vms/{id}`: `/status` reports the live VMM state, uptime, configured device counts, kernel path, and `vcpu_alive`; `GET /v1/vms/{id}` returns the persisted orchestrator `VmRecord`.

Status codes: `200`, `401`, `403`, `404`, `409` (VM is stopped), `500`.

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

`connect_token` is a one-time per-session secret. Pass it as the `token` query parameter when attaching over the WebSocket route below. It expires after 5 minutes or on first successful connect.

Additional REST routes:

| Method | Path | Response |
| --- | --- | --- |
| `GET` | `/v1/vms/{id}/pty/sessions` | `{ "sessions": [...] }` |
| `GET` | `/v1/vms/{id}/pty/sessions/{pty_id}` | PTY session record |
| `DELETE` | `/v1/vms/{id}/pty/sessions/{pty_id}` | `204` no body |
| `POST` | `/v1/vms/{id}/pty/sessions/{pty_id}/resize` | `{ "pty_id": "uuid", "cols": N, "rows": N }` |

`WS /v1/vms/{id}/pty/{pty_id}/connect?token=<connect_token>` upgrades to a WebSocket. It authenticates with the session's one-time `connect_token`, not the API key. Binary messages are raw PTY bytes. Text messages are JSON controls: client-to-server `{"type":"resize","cols":N,"rows":N}` and server-to-client `{"type":"exit","exit_code":N}`.

Status codes: `200`, `201`, `204`, `400`, `401`, `404`, `409`, `500`. WebSocket failures close the socket instead: `4401` for a bad or missing token, `1008` for an unknown session, `1013` when the VM is not on this node, `1011` for attach errors.

### `DELETE /v1/vms/{id}`

Resolve owner, stop the VM on its owner, mark the owner's local record as `stopped`, release the local scheduler slot, and remove the VM ownership row from `fleet_vms`.

Response `204`: no body.

Status codes: `204`, `401`, `403`, `404`, `500`.

Note: the local SQLite VM row is not deleted. On the owner, a later local `GET` can still find the stopped record. Other nodes generally cannot find it after `fleet_vms` is cleared.

### `POST /v1/vms/{id}/pause`

Resolve owner and pause the VM. The public handler does not require a JSON body.

Response `200`: updated `VmRecord` with `status: "paused"`.

Status codes: `200`, `401`, `403`, `404`, `409` (VM is stopped), `500`.

### `POST /v1/vms/{id}/resume`

Resolve owner and resume a paused VM. The public handler does not require a JSON body.

Response `200`: updated `VmRecord` with `status: "running"`.

Status codes: `200`, `401`, `403`, `404`, `409` (VM is stopped), `500`.

### `POST /v1/vms/{id}/snapshot`

Resolve owner and ask the VMM to write a snapshot. Snapshot files are node-local.

Request:

```json
{ "diff": false }
```

`diff` defaults to `false`.

Response `200`:

```json
{
  "path": "/path/on/owner/snapshot",
  "host_id": "node-a"
}
```

Always preserve `host_id`; pass it to `POST /v1/restore` so the restore routes to the node that has the file.

Status codes: `200`, `401`, `403`, `404`, `409` (VM is stopped), `500`.

### `POST /v1/restore`

Restore a VM from a snapshot file. If `host_id` is present and is not the receiving node, the request is routed to that host. No snapshot bytes are copied between nodes.

Request:

```json
{
  "snapshot_path": "/path/on/snapshot-owner/snapshot",
  "host_id": "node-a",
  "id": "optional new vm uuid"
}
```

Response `201`: `VmRecord`.

Status codes:

| Status | Meaning |
| --- | --- |
| `201` | VM restored on the selected node. |
| `401` | Missing or wrong `X-API-Key`. |
| `403` | Tenant VM quota reached. |
| `404` | `host_id` not found in the fleet. |
| `429` | Selected node is at local capacity; includes `Retry-After` in seconds. Restore does not exhaustively try other nodes because the snapshot file is node-local. |
| `500` | VMM restore, peer, or fleet failure. |

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

`timeout_ms` defaults to `30000`.

Response `200`: final `ExecutionRecord` with `status: "completed"` (result fields `exit_code`, `stdout`, `stderr`, `duration_ms` set) or `status: "failed"` (`error` set). A command that runs but exits non-zero is still `completed`; check `exit_code`. An exec that could not run at all (for example, guest agent unavailable) returns `200` with `status: "failed"`, not an HTTP error.

Status codes:

| Status | Meaning |
| --- | --- |
| `200` | Execution finished; the record carries the outcome (completed or failed). |
| `401` | Missing or wrong `X-API-Key`. |
| `403` | VM belongs to a different tenant. |
| `404` | VM not found. |
| `500` | Owner resolution, peer, or internal failure. |

### `POST /v1/execute_async`

Resolve the VM owner and run a command asynchronously. The execution record is created on the API node that accepts the request, even when the VM owner is remote.

Request:

```json
{
  "vm_id": "uuid",
  "command": "echo hello",
  "timeout_ms": 30000
}
```

`timeout_ms` defaults to `30000`.

Response `202`: initial `ExecutionRecord` with `status: "pending"`.

Status codes:

| Status | Meaning |
| --- | --- |
| `202` | Execution accepted. |
| `401` | Missing or wrong `X-API-Key`. |
| `403` | VM belongs to a different tenant. |
| `404` | VM not found. |
| `500` | Store, peer, or VMM failure. |

Polling note: `GET /v1/executions/{id}` must hit the same API node that accepted the execution request unless an external system replicates execution records. Execution status lookups are not routed through the fleet.

### `GET /v1/executions/{id}`

Return an execution record from the receiving node's local SQLite store.

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

`allow_existing` defaults to `false`.

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

Aggregated per-key usage stats from the primary store. Requires a fleet database. Admins see every key; a non-admin key sees only its own. Query parameters: `from`, `to` (RFC3339; default last 30 days), and `api_key_id` (admin only).

```json
[
  { "api_key_id": "a9842b8c...c385aa", "owner_key": "default", "vm_runtime_seconds": 11.05, "exec_count": 1, "exec_duration_ms": 1 }
]
```

Status codes: `200`, `401`, `500` (500 if no fleet database is configured).

### `GET /v1/audit`

Recent audit trail, newest first. Requires a fleet database. Admins see every key; a non-admin key sees only its own. Query parameters: `api_key_id` (admin only), `vm_id`, `limit` (default 100, max 1000).

```json
[
  { "id": "…", "api_key_id": "a9842b8c...c385aa", "owner_key": "default", "host_id": "node-a", "vm_id": "33827851-…", "action": "exec", "outcome": "ok", "detail": null, "created_at": "2026-07-03T00:00:00Z" }
]
```

Both endpoints record raw stats only. See [USAGE-AND-AUDIT.md](USAGE-AND-AUDIT.md).

Status codes: `200`, `401`, `500`.

## Internal peer API

Internal routes are mounted on the same HTTP listener and are protected only by `X-Peer-Secret`. Do not expose them publicly. Place nodes on a private network, restrict security groups, and prefer mTLS in production.

### Internal endpoint table

| Method | Path | Body | Success | Purpose |
| --- | --- | --- | --- | --- |
| POST | `/internal/v1/vms` | `CreateVmRequest` | `201 VmRecord` | Create on this node only. Returns 429 + `Retry-After` if this node is full. |
| POST | `/internal/v1/restore` | `RestoreRequest` | `201 VmRecord` | Restore on this node only. |
| GET | `/internal/v1/vms/{id}` | none | `200 VmRecord` | Get from this node's local store. |
| GET | `/internal/v1/vms/{id}/status` | none | `200 LiveVmStatus` | Query live status from this node's local VMM. |
| DELETE | `/internal/v1/vms/{id}` | none | `204` | Stop on this node and clear local/fleet ownership. |
| POST | `/internal/v1/vms/{id}/exec` | `{"command":"...","timeout_ms":30000}` | `200 {exit_code,stdout,stderr,duration_ms}` | Execute on this node's VM. |
| POST | `/internal/v1/vms/{id}/pause` | none | `200 VmRecord` | Pause local VM. |
| POST | `/internal/v1/vms/{id}/resume` | none | `200 VmRecord` | Resume local VM. |
| POST | `/internal/v1/vms/{id}/snapshot` | `{"diff":false}` | `200 {path,host_id}` | Snapshot local VM. |
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
| `401` | Missing or invalid API key or peer secret. |
| `403` | Authenticated caller is not allowed to access the tenant resource, lacks admin role, or exceeded tenant VM quota. |
| `404` | VM, execution, owner host, or snapshot owner not found. |
| `409` | Genuine state conflict, such as a duplicate requested VM id or an operation invalid for the current VM state. |
| `429` | Capacity or overload backpressure. Create/restore responses include `Retry-After` in seconds. |
| `500` | Internal, VMM, peer, store, or fleet failure. |
