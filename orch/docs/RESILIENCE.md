# Resilience and scale scenarios

This document lists the failure, failover, and scale scenarios `taritd` (with the
`vmm` microVM backend) is designed for, how each is handled, and the test that
validates it. It is meant to be read alongside `ARCHITECTURE.md` (how the pieces
fit) and `OPERATIONS.md` (how to run and troubleshoot).

## Test environment

KVM-dependent validation runs on Linux hosts with KVM, either directly or with
nested virtualization enabled. Cluster and durability tests use a configured
PostgreSQL fleet store. The test scripts use namespaced records and restore any
host-network state they create before completing.

Test scripts live in `tests/`. Each row below names the script and the pass
marker it prints. To reproduce, see "Running the suite" at the end.

Legend for validation status:

- `e2e`: end to end on real KVM on the validation host, result recorded here.
- `unit`: covered by `cargo test` (deterministic, no KVM).
- `design`: guaranteed by construction and reviewed, not exercised by an
  automated test yet. Called out honestly so nothing is oversold.

## 1. Node and host failures (high availability)

Cluster mode runs any number of `taritd` nodes over one shared PostgreSQL fleet
store. Every node accepts public API traffic, owns the VMs it booted, and
forwards operations for VMs owned by peers. Node liveness is a heartbeat row in
`fleet_hosts`; a node is considered up while its heartbeat is younger than 15s.

| Scenario | Expected behavior | Validation |
| --- | --- | --- |
| Cluster forms from N nodes | Each node registers in `fleet_hosts`; `/v1/cluster` reports `healthy_nodes = N` | e2e: `e2e_ha_failover.sh`, `e2e_cluster.sh` (`infra/auth`) |
| Leader election | Exactly one node holds the `fleet_leader` lease (autoscaler singleton) | e2e: `e2e_ha_failover.sh` |
| Non-leader node dies | `healthy_nodes` drops after the 15s stale window; survivors keep serving | e2e: `e2e_ha_failover.sh` |
| Leader node dies | Lease expires after its TTL (30s) and a survivor acquires leadership; self-heals with no operator action | e2e: `e2e_ha_failover.sh` (lease moved n2 -> n1) |
| Node rejoins | Restarted node re-registers; `healthy_nodes` returns to N | e2e: `e2e_ha_failover.sh` |
| Request lands on a non-owner node | The receiving node looks up the owner in `fleet_vms` and forwards the lifecycle/exec call over the internal peer API | e2e: `e2e_cluster.sh` (`cross-node routing`) |
| Cross-node snapshot then restore | Snapshot is node-local; restore on another node is routed to the owner (or takes an explicit `host_id`) | e2e: `e2e_cluster.sh` (`restore from full snapshot`, `cross-node routing`) |
| Startup reconciliation | On boot a node loads persisted VM records, keeps only its own live children, and frees orphaned per-VM resources | unit + design (`main.rs` live-VM reconcile, `net.rs allocator_recovers_only_live_valid_entries`) |

Result: `HA_FAILOVER_PASS` (5/5) and `e2e_cluster.sh` 7/7.

## 2. Database (PostgreSQL) failures (durability)

The fleet store is the only shared dependency. It holds node membership, VM
ownership, and the leader lease. Node-local state (VM/exec records, cached host
roster) lives in per-node SQLite, so a node can keep serving its own VMs across a
database blip.

| Scenario | Expected behavior | Validation |
| --- | --- | --- |
| PostgreSQL outage or partition | Nodes do not crash. Fleet writes (heartbeats) and reads (`/v1/cluster`) error and are logged; the process stays up | e2e: `e2e_pg_blip.sh` (procs alive throughout a 45s DB partition) |
| PostgreSQL recovery | Connections re-establish through the pool; heartbeats resume; `healthy_nodes` climbs back to N within about 30s | e2e: `e2e_pg_blip.sh` (health 0 -> 1 -> 2 after unblock) |
| Two nodes race for leadership | Lease row `INSERT ... ON CONFLICT ... WHERE leader_id = self OR expires_at < now()` admits exactly one writer; no split-brain leader | unit + design (`tarit-fleet` `try_acquire_leader`) |
| Missing peer secret in cluster mode | Startup refuses the default `dev-peer-secret` when `TARIT_DATABASE_URL` is set | unit + design (config validation) |
| PostgreSQL TLS | A configured CA bundle is added to the Rust TLS root store alongside native roots | e2e (cluster tests connect over `sslmode=require`) |
| Usage/audit write-behind | Per-key usage stats and audit events buffer in a local SQLite outbox and flush to Postgres; a DB outage delays but never drops them, and re-sends are idempotent (`UNIQUE(vm_id, kind, window_end)`) | e2e: `e2e_usage_audit.sh` + outbox design |

Result: `PG_BLIP_PASS` (no crash during outage, full recovery after).

The DB outage is injected with an nftables drop rule to the database endpoint, so
the test is self-contained and does not depend on a cloud control-plane API. A
managed database failover presents to the node as the same connection loss and
recovery this test exercises.

## 3. Capacity, admission, and autoscaling (scalability)

Admission reserves a scheduler slot before a VM boots. Warm and assigned VMs
share one `TARIT_MAX_VMS` cap per node, plus optional vCPU and memory caps.
When the local node is full, the scheduler places on a peer with capacity. When
the whole cluster is full, the request waits up to `TARIT_ADMISSION_TIMEOUT_MS`
for a slot, then returns overload backpressure.

| Scenario | Expected behavior | Validation |
| --- | --- | --- |
| Single node full, cluster has room | Create is placed on a peer node and returns 201 with the owning `host_id` | e2e: `e2e_cluster.sh` (`cross-node routing`) |
| Whole cluster full | Create returns `429 Too Many Requests` with `Retry-After`, not a crash or a hang | e2e: `e2e_cluster.sh` (`capacity/backpressure`) |
| Burst of concurrent creates | Warm pool absorbs the burst; overflow cold-boots or gets backpressure, never a partial/corrupt VM | e2e: warm-pool restore path (`e2e_warmpool_restore.sh`); prior burst-100 run |
| Warm pool refill under load | Refill runs on a low `cpu.weight` cgroup with a concurrency limit and watermark hysteresis, so refill never starves live traffic | e2e: `e2e_cpu_refill.sh` |
| Autoscaler scale-out signal | The leader emits a `scale_out` decision when free vCPUs fall below the threshold and invokes `TARIT_AUTOSCALE_PROVIDER_CMD` with the decision JSON | e2e: `e2e_autoscale.sh` (provider invoked with a scale_out decision) |
| Autoscaler scale-in signal | The leader emits a `scale_in` decision with the least-loaded victim node when the cluster is idle, after a cooldown | design + unit (cooldown, victim selection) |
| Network address pool exhaustion | Allocation past the /30 pool returns overload rather than colliding addresses | unit (`allocator_exhaustion_returns_overloaded`) + e2e net lifecycle |

The autoscaler is off by default and stays cloud-SDK-free: it only decides and
calls a provider command, so it works behind an ASG, MIG, or Terraform. See
`AUTOSCALING.md`.

## 4. VM lifecycle and state correctness

| Scenario | Expected behavior | Validation |
| --- | --- | --- |
| create -> exec -> pause -> resume -> snapshot -> delete | Each transition returns the right status; exec output is faithful; delete is terminal | e2e: `e2e_cluster.sh` (`create+lifecycle state machine`) |
| Invalid transitions | Operations on missing or stopped VMs return 404/409, not 500 | e2e: `e2e_cluster.sh` (`invalid transitions/errors`) |
| Restore-clone disk isolation | Two clones restored from one golden snapshot use separate overlays; writes do not cross; the base is unchanged | e2e: VMM `restore-clone-validate.sh` |
| Suspend frees RAM | Suspend = snapshot + userfaultfd arm + `madvise(DONTNEED)`; RSS drops (226 MB -> 10 MB) and resume is byte-identical (SHA256) | e2e: VMM `suspend-validate.sh` |
| Snapshot and overlay GC | Scratch snapshots/overlays are removed on stop; user snapshots are preserved; `vmm gc` sweeps leftovers | e2e: VMM `gc-validate.sh` |
| Live snapshot consistency | Full-restore, diff-restore, and suspend all restore to a SHA256-consistent guest | e2e: VMM `livesnap-gate.sh` (full + diff + suspend) |

## 5. Networking at scale

Per-VM host networking is opt-in (`TARIT_ENABLE_NET`). Each VM gets a private
/30 tap out of 172.16.0.0/16 with a per-slot nftables masquerade rule. The slot
map is persisted and reconciled on restart.

| Scenario | Expected behavior | Validation |
| --- | --- | --- |
| Create N networked VMs | N taps `insta0..instaN-1` with correct /30 addressing and one masquerade rule each | e2e: `e2e_net_scale.sh` |
| Delete VMs | Taps and nft rules are removed; slots are freed; no leaks | e2e: `e2e_net_scale.sh` (0 taps after delete) |
| Slot reuse | Freed slots are reused lowest-first on the next create | e2e: `e2e_net_scale.sh` (same tap set after recreate) + unit |
| Crash recovery | On restart the allocator rebuilds from live VMs and an age-gated sweep reaps orphaned taps/rules without racing in-flight provisioning | unit (`stale_sweep_selects_only_old_orphan_taritd_taps`) + e2e |
| Pool exhaustion | Allocation past the pool returns overload | unit (`allocator_exhaustion_returns_overloaded`) |

Result: `NET_SCALE_PASS`.

## 6. Security and tenancy

| Scenario | Expected behavior | Validation |
| --- | --- | --- |
| Unauthenticated API call | `/v1/*` returns 401 without a valid `X-API-Key`; `/health` stays public | e2e: `e2e_cluster.sh` (`infra/auth`) |
| Internal peer API exposed | `/internal/v1/*` returns 401 without the shared `X-Peer-Secret` | e2e: `e2e_cluster.sh` (`peer security`) |
| Multi-tenant isolation | Hashed multi-key auth; tenants see only their own VMs; per-tenant VM quotas enforced | e2e: `e2e_multitenant.sh` |
| Admin-only endpoints | `/v1/cluster` and other admin routes require an admin identity | design + `e2e_multitenant.sh` |
| SSH gateway auth | A session is admitted only if the presented public key is registered and maps to a VM the caller owns | e2e: `e2e_ssh_pty.sh` |

## 7. Resource isolation and overload

| Scenario | Expected behavior | Validation |
| --- | --- | --- |
| Per-VM cgroup limits | `vmm serve` applies memory/cpu/pids/cpuset caps when configured | e2e: VMM `cgroup-validate.sh` |
| CPU/memory overcommit | Guest RAM is demand-paged (`MAP_NORESERVE`); idle vCPUs block near 0% CPU, so IO/network-bound VMs pack densely | design (memory backend + vcpu thread) |
| Graceful drain and reaper | Shutdown drains HTTP, reaps local VMs (no orphaned `vmm` processes), and honors `TARIT_REAP_ON_SHUTDOWN` | e2e: `e2e_lifecycle.sh` |
| Overload backpressure | At capacity the API returns 429 + `Retry-After` instead of failing hard | e2e: `e2e_cluster.sh` (`capacity/backpressure`) |

## 8. Access (SSH and interactive PTY)

Interactive access reaches the guest through the vsock agent, with no in-guest
sshd or network service.

| Scenario | Expected behavior | Validation |
| --- | --- | --- |
| `ssh <vm_id>@gateway` | The embedded SSH gateway authenticates by registered key and bridges to the guest PTY over vsock | e2e: `e2e_ssh_pty.sh` |
| WebSocket PTY | `WS /v1/vms/{id}/pty/{pty_id}/connect` relays raw bytes and JSON resize/exit frames to the same guest PTY | e2e: `e2e_ssh_pty.sh` |
| Terminal resize and clean exit | Window-change maps to `TIOCSWINSZ`; child exit propagates an exit code | e2e: VMM `pty-validate.sh` (`stty size` reflects resize, exit 0) |

## 9. Port-share gateway

`tests/e2e_shares.sh` is the KVM release gate for shared guest ports. It starts
two Tarit nodes, boots real networked guests, runs an HTTP/WebSocket application
on guest port `43127`, and sends gateway traffic through the node that does not
own the guest. It exits successfully and prints `SHARES_PASS` only after every
assertion below succeeds.

| Failure domain | Required gate behavior |
| --- | --- |
| TLS edge, listener, and route isolation | A real Caddy `tls internal` edge fronts node B's share listener at a deterministic test hostname. Clients verify Caddy's ephemeral CA; Caddy preserves `Host` and WebSocket upgrades, strips `X-API-Key`, overwrites forwarding headers, and returns its own 404 for `/internal/v1/*`. The control listener cannot serve share-host traffic and the share listener cannot expose control or internal peer routes. |
| Host, path, and forwarding validation | Root and nested paths preserve their query strings. Parsed guest headers prove exactly one edge-generated forwarding value, including `https`, after malicious duplicate and conflicting client forwarding headers. |
| HTTP streaming | A public response of at least 32 MiB is SHA-256 verified while streamed, with application-observed response and upload backpressure; delayed first data and large uploads remain streaming operations. |
| Header boundary | Request method, query string, and application `Authorization` are preserved exactly. `X-API-Key`, share-token, peer, and client-supplied forwarding headers do not reach the guest. A trusted HTTPS scheme is rebuilt for the application. |
| Share authorization | Public access works; private access rejects anonymous and malformed tokens, accepts a newly issued token, expires it, invalidates it on version rotation, and returns the documented result after revocation. |
| Owner routing and peer trust | Share control and data traffic through the non-owner node reach the owner only with a valid signed peer identity. Missing, forged, and unsigned peer calls fail closed. |
| Real KVM and guest availability | Each created VM resolves to its exact VMM PID; its `/proc/<pid>/exe` must be the expected VMM and `/proc/<pid>/fd` must hold `/dev/kvm`. Retargeting changes the active guest target. A stopped target returns the stable `503` JSON error and a revoked share returns `404`. |
| WebSocket transport | Text, binary, ping, pong, graceful close, and abrupt client disconnect traverse the non-owner route without leaving active gateway gauges. |
| Host-network isolation | The gate takes a host-global `/run/lock` lock and refuses unrelated Tarit/VMM processes, tap state, or a pre-existing `taritd_nat` table. It deletes a table only when it created it and its baseline still matches; it restores `ip_forward` only after ownership checks. |
| Metrics and shutdown | Both owner and non-owner metrics have fixed cardinality and expose no raw slug, token, tenant, or VM identifier; exposed HTTP/WebSocket gauges must return to zero. A deterministic in-flight create race records its proposed ID, requires stable `429` JSON (never HTTP `000`), and verifies no local-store, fleet, process, socket, or network-allocation state survives shutdown. |

## Running the suite

Cluster and durability tests require a configured fleet database and a release
`taritd` build on the KVM host:

```sh
# configure TARIT_DATABASE_URL and TARIT_PEER_SECRET through the deployment
# environment, then build the daemon
cargo build --release -p taritd

# coordination-plane resilience
bash tests/e2e_ha_failover.sh          # leader failover, node down/rejoin
sudo -E bash tests/e2e_pg_blip.sh      # PostgreSQL outage durability
bash tests/e2e_autoscale.sh            # autoscaler provider actuation
sudo -E bash tests/e2e_usage_audit.sh  # per-key usage stats + audit trail

# full cluster e2e (creates real VMs)
sudo bash tests/e2e_cluster.sh

# single-node feature validations
sudo bash tests/e2e_ssh_pty.sh
sudo bash tests/e2e_net_scale.sh
sudo bash tests/e2e_lifecycle.sh
sudo bash tests/e2e_multitenant.sh
sudo bash tests/e2e_cpu_refill.sh

# two-node, real-KVM port-share gateway gate
sudo -E bash tests/e2e_shares.sh
```

The share gate must run as root on an otherwise idle Linux KVM host. It requires
`caddy`, `curl`, `python3` (with `sqlite3`), the `sqlite3` CLI, GNU coreutils (`sha256sum`,
`timeout`, `mktemp`, `stat`, `cmp`, `chown`, and `chgrp`), `ip`, `nft`, `ps`,
`readlink`, `grep`, `awk`, `find`, `sysctl`, and `flock`; it also needs `psql` plus `initdb`,
`pg_ctl`, and `runuser` when `TARIT_DATABASE_URL` is unset. The guest rootfs
must include Node.js. Caddy is mandatory; the gate intentionally has no
plaintext fallback. The gate serializes host-network ownership with the fixed
`/run/lock/tarit-e2e-shares.lock` lock and uses a private `PGPASSFILE` for its
bounded PostgreSQL cleanup queries.

VMM-side validations (`pty-validate.sh`, `restore-clone-validate.sh`,
`suspend-validate.sh`, `gc-validate.sh`, `cgroup-validate.sh`,
`livesnap-gate.sh`) live in the VMM workspace under `vmm/ci/`.

## Known limitations and coverage gaps

These are tracked and called out so the matrix above is not read as complete
coverage:

- Managed database failover is simulated as a connection outage (section 2), not
  driven through a provider control API in an automated test.
- `virtio-balloon` reclaim needs a balloon-enabled guest kernel; the current test
  kernel lacks `CONFIG_VIRTIO_BALLOON`.
- `aarch64` guests are unsupported (KVM VM/vCPU/GIC/PSCI/FDT path pending).
- Cold boot (create to first exec) is not latency-optimized; sub-100ms today
  comes from restore/warm-pool, not cold boot.
- Public and internal routes share one listener; isolate `/internal/v1/*` with
  network policy in production (see `OPERATIONS.md`).
- `GET /v1/executions/{id}` is not fleet-forwarded; use sticky routing or poll the
  accepting node.
