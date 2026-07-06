# Usage stats and audit trail

`taritd` records two kinds of per-API-key data and treats the primary store
(PostgreSQL) as the one source of truth for both:

- **Usage stats**: raw metering only. How many wall-clock seconds each VM ran and
  per-exec durations, attributed to the API key that owns the VM.
- **Audit trail**: which key took which action (create, delete, pause, resume,
  snapshot, restore, exec, egress change, SSH attempt) and the outcome.

This layer computes no prices and has no notion of users or invoices. A user or
billing layer sits above the orchestrator and interprets these stats. That is
why nothing here is named "billing".

## Attribution

Every event carries a stable, non-secret `api_key_id`: the lowercase hex of the
SHA-256 hash of the API key. It never exposes the key itself and is safe to
store and hand to an upper layer. The tenant the key maps to is kept alongside as
`owner_key`.

A single user is expected to hold many keys. The orchestrator attributes to the
key; mapping keys to users is the upper layer's job.

Cross-node actions stay correctly attributed: when a node forwards an operation
to the VM's owner, it passes the caller's key id in the `X-Tarit-Api-Key-Id`
header, and the owner records the event under that key.

## Data flow (write-behind, one source of truth)

```
node: meter / action  ->  local SQLite outbox  ->  flusher  ->  PostgreSQL
                          (usage_outbox,                        (usage_events,
                           audit_outbox)                         audit_events)
```

Events are written to a node-local outbox first, then a background flusher
pushes unsent rows to Postgres and marks them sent. This gives eventual
consistency and survives a database outage: rows stay pending and are retried
when the database returns (see the PostgreSQL outage test in `RESILIENCE.md`).
Without a fleet database (single-host mode) events accumulate in the local
outbox and the API endpoints are unavailable.

Re-sending a not-yet-acked batch is safe. `usage_events` has a
`UNIQUE (vm_id, kind, window_end)` constraint and inserts use `ON CONFLICT DO
NOTHING`; audit inserts dedupe on the event id.

## Usage stats

### VM runtime (primary)

The meter runs every `TARIT_USAGE_METER_INTERVAL_SECS`. For each alive local VM
(running or paused) it bills the wall-clock seconds since that VM's last billed
watermark, emits a `vm_runtime` event for the interval `[watermark, now]`, and
advances the watermark. Intervals never overlap, so nothing is double counted,
and a crash loses at most one interval. The watermark is persisted per VM
(`billing_watermark` in local SQLite), so pause, resume, and node restart do not
re-bill already-billed time. When a VM stops, a final interval is billed and the
watermark is dropped.

Each node meters only the VMs it owns, so a cluster never double counts a VM.

### Exec (secondary)

When an exec command completes, an `exec` usage event records its `duration_ms`,
attributed to the calling key.

## Audit trail

Each audited action is recorded as an `audit_event` with the acting key, the VM
(if any), the action verb, an outcome (`ok`, `denied`, `error`), and a small
secret-free detail string. Audited actions: `create`, `delete`, `pause`,
`resume`, `snapshot`, `restore`, `exec`, `update_egress`, and `ssh_attempt`
(both accepted and denied, for security review).

## API

Both endpoints require a fleet database. Admins see every key; a non-admin key
sees only its own data.

`GET /v1/usage` aggregates usage per key over a time range.

Query parameters: `from`, `to` (RFC3339; default is the last 30 days),
`api_key_id` (admin only, to scope to one key).

```json
[
  {
    "api_key_id": "a9842b8c...c385aa",
    "owner_key": "default",
    "vm_runtime_seconds": 11.05,
    "exec_count": 1,
    "exec_duration_ms": 1
  }
]
```

`GET /v1/audit` lists recent actions, newest first.

Query parameters: `api_key_id` (admin only), `vm_id`, `limit` (default 100, max
1000).

```json
[
  {
    "id": "…",
    "api_key_id": "a9842b8c...c385aa",
    "owner_key": "default",
    "host_id": "node-a",
    "vm_id": "33827851-…",
    "action": "exec",
    "outcome": "ok",
    "detail": null,
    "created_at": "2026-07-03T00:00:00Z"
  }
]
```

## Configuration

| Variable | Default | Description |
| --- | --- | --- |
| `TARIT_USAGE_METER_INTERVAL_SECS` | `30` | How often the VM-runtime meter bills alive local VMs. |
| `TARIT_USAGE_FLUSH_INTERVAL_SECS` | `10` | How often the flusher pushes local outboxes to Postgres. |

Attribution and the API need cluster mode (`TARIT_DATABASE_URL` set). The meter
always runs; without a fleet the flusher is idle and events stay local.

## Schema

PostgreSQL (primary source of truth):

```sql
CREATE TABLE usage_events (
  id UUID PRIMARY KEY,
  api_key_id TEXT NOT NULL,
  owner_key TEXT NOT NULL,
  host_id TEXT NOT NULL,
  vm_id UUID NOT NULL,
  kind TEXT NOT NULL,            -- 'vm_runtime' | 'exec'
  seconds DOUBLE PRECISION,      -- vm_runtime
  duration_ms BIGINT,            -- exec
  window_start TIMESTAMPTZ NOT NULL,
  window_end TIMESTAMPTZ NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  CONSTRAINT usage_events_dedupe UNIQUE (vm_id, kind, window_end)
);

CREATE TABLE audit_events (
  id UUID PRIMARY KEY,
  api_key_id TEXT NOT NULL,
  owner_key TEXT NOT NULL,
  host_id TEXT NOT NULL,
  vm_id UUID,
  action TEXT NOT NULL,
  outcome TEXT NOT NULL,         -- 'ok' | 'denied' | 'error'
  detail TEXT,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

Node-local SQLite holds the `usage_outbox`, `audit_outbox`, and
`billing_watermark` tables that back the write-behind flush.
