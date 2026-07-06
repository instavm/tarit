# Resilience and scale scenarios

This document lists the failure, failover, and scale scenarios `taritd` (with the
`vmm` microVM backend) is designed for, how each is handled, and the test that
validates it. It is meant to be read alongside `ARCHITECTURE.md` (how the pieces
fit) and `OPERATIONS.md` (how to run and troubleshoot).

## Test environment

All KVM-dependent validation runs on a nested-virt EC2 host (`c8i.xlarge`).
Cluster and durability tests use a dedicated PostgreSQL fleet store (an
isolated RDS instance provisioned by `deploy/provision-rds.sh`). No other
infrastructure is touched.

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
| RDS TLS | The RDS CA bundle is added to the rustls root store alongside native roots (`TARIT_RDS_CA_FILE`) | e2e (all cluster tests connect over `sslmode=require`) |
| Usage/audit write-behind | Per-key usage stats and audit events buffer in a local SQLite outbox and flush to Postgres; a DB outage delays but never drops them, and re-sends are idempotent (`UNIQUE(vm_id, kind, window_end)`) | e2e: `e2e_usage_audit.sh` + outbox design |

Result: `PG_BLIP_PASS` (no crash during outage, full recovery after).

The DB outage is injected with an nftables drop rule to the database endpoint, so
the test is self-contained and does not depend on a cloud control-plane API. A
managed failover (RDS reboot / Multi-AZ promotion) presents to the node as the
same connection loss and recovery this test exercises.

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

## Running the suite

Cluster and durability tests expect the fleet RDS env (written by
`deploy/provision-rds.sh`) and a release `taritd` build on the KVM host:

```sh
# on the KVM host, with the fleet DB env sourced
set -a; . ~/.taritd/cp-rds.env; set +a
cargo build --release -p taritd

# coordination-plane resilience
bash tests/e2e_ha_failover.sh          # leader failover, node down/rejoin
sudo -E bash tests/e2e_pg_blip.sh      # PostgreSQL outage durability
bash tests/e2e_autoscale.sh            # autoscaler provider actuation
sudo -E bash tests/e2e_usage_audit.sh  # per-key usage stats + audit trail

# full 3-node cluster e2e against the RDS (creates real VMs)
sudo bash tests/run_cluster_rds.sh && cat /tmp/cluster-result.log

# single-node feature validations
sudo bash tests/e2e_ssh_pty.sh
sudo bash tests/e2e_net_scale.sh
sudo bash tests/e2e_lifecycle.sh
sudo bash tests/e2e_multitenant.sh
sudo bash tests/e2e_cpu_refill.sh
```

VMM-side validations (`pty-validate.sh`, `restore-clone-validate.sh`,
`suspend-validate.sh`, `gc-validate.sh`, `cgroup-validate.sh`,
`livesnap-gate.sh`) live in the VMM workspace under `vmm/ci/`.

## Known limitations and coverage gaps

These are tracked and called out so the matrix above is not read as complete
coverage:

- Managed RDS hardware failover (Multi-AZ promotion) is simulated as a connection
  outage (section 2), not driven through the cloud API in an automated test.
- `virtio-balloon` reclaim needs a balloon-enabled guest kernel; the current test
  kernel lacks `CONFIG_VIRTIO_BALLOON`.
- `aarch64` guests are unsupported (KVM VM/vCPU/GIC/PSCI/FDT path pending).
- Live migration across hosts needs bare-metal support and is out of scope.
- Cold boot (create to first exec) is not latency-optimized; sub-100ms today
  comes from restore/warm-pool, not cold boot.
- Public and internal routes share one listener; isolate `/internal/v1/*` with
  network policy in production (see `OPERATIONS.md`).
- `GET /v1/executions/{id}` is not fleet-forwarded; use sticky routing or poll the
  accepting node.
