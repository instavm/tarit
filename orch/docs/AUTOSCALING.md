# Autoscaling

`taritd` includes a small fleet autoscaler in `crates/taritd/src/autoscale.rs`. It is intentionally provider-neutral: the Rust control loop decides when to scale, then invokes an operator-provided command that talks to EC2 Auto Scaling Groups, GCP Managed Instance Groups, Terraform, or another control plane.

## Enablement

Autoscaling runs only in cluster mode and only when enabled:

```sh
export TARIT_DATABASE_URL='postgres://taritd:password@postgres.example:5432/taritd?sslmode=require'
export TARIT_PEER_SECRET='replace-with-a-long-random-peer-secret'
export TARIT_AUTOSCALE=true
export TARIT_AUTOSCALE_MIN=3
export TARIT_AUTOSCALE_MAX=20
export TARIT_AUTOSCALE_OUT_FREE_VCPUS=4
export TARIT_AUTOSCALE_IN_FREE_VCPUS=64
export TARIT_AUTOSCALE_PROVIDER_CMD='exec /usr/local/bin/taritd-autoscale-provider "$1"'
```

If `TARIT_AUTOSCALE_PROVIDER_CMD` is unset, the autoscaler logs decisions but does not actuate them.

## Control loop

| Constant | Value | Meaning |
| --- | --- | --- |
| Tick | 10 seconds | How often each node wakes up. |
| Leader TTL | 30 seconds | PostgreSQL lease duration in `fleet_leader`. |
| Host freshness | 15 seconds | Healthy hosts older than this are excluded. |
| Cooldown | 60 seconds | Minimum time between provider actions by the current leader. |

Every node starts the loop, but only the current PostgreSQL lease holder acts. The lease is acquired with `PostgresFleet::try_acquire_leader`, which updates row `fleet_leader(id=1)` if the caller already owns it or if the existing lease expired.

On each leader tick:

1. Read `fleet_hosts`.
2. Keep hosts where `healthy = true` and `last_heartbeat` is fresh.
3. Count healthy nodes.
4. Sum aggregate `free_vcpus`.
5. If cooldown has not elapsed, do nothing.
6. If `free_vcpus < TARIT_AUTOSCALE_OUT_FREE_VCPUS` and healthy nodes are below max, emit `scale_out` with target `node_count + 1`.
7. Else if `free_vcpus > TARIT_AUTOSCALE_IN_FREE_VCPUS` and healthy nodes are above min, pick the least-loaded node by `sandbox_count` and emit `scale_in` with target `node_count - 1` and `victim` set.

## Signals and current metrics

Current fleet rows track:

| Signal | Source | Used for |
| --- | --- | --- |
| `sandbox_count` | Scheduler heartbeat | Least-loaded scale-in victim selection and placement sort. |
| `free_vcpus` | Scheduler heartbeat | Scale-out and scale-in threshold decisions. |
| `free_memory_mib` | Scheduler heartbeat | Placement eligibility and cluster status. Not used by autoscaler thresholds today. |
| `healthy` | Heartbeat row | Host inclusion. Current heartbeat always writes `true`. |
| `last_heartbeat` | Heartbeat row | Host freshness and node count. |
| `rpc_addr` | Config heartbeat | Peer routing. |

Configuration labels included in decisions:

| Label | Env | Behavior |
| --- | --- | --- |
| Region | `TARIT_REGION` | Included in provider decision JSON from the leader's config. |
| Zone | `TARIT_ZONE` | Included in provider decision JSON from the leader's config. |
| Cloud | `TARIT_CLOUD` | Included in provider decision JSON from the leader's config. |

Region, zone, cloud, and drain state are not persisted per host in `fleet_hosts`. Region-aware placement and multi-region autoscaling need fleet columns or the provider's inventory as the source of topology and node state.

## Provider command contract

The autoscaler builds this JSON decision:

```json
{
  "action": "scale_out",
  "target_nodes": 4,
  "current_nodes": 3,
  "free_vcpus": 1,
  "victim": null,
  "region": "us-east-1",
  "zone": "us-east-1a",
  "cloud": "aws"
}
```

For scale-in:

```json
{
  "action": "scale_in",
  "target_nodes": 2,
  "current_nodes": 3,
  "free_vcpus": 96,
  "victim": "node-c",
  "region": "us-east-1",
  "zone": "us-east-1a",
  "cloud": "aws"
}
```

Implementation detail: Rust invokes the command as:

```text
sh -c "$TARIT_AUTOSCALE_PROVIDER_CMD" provider '<decision-json>'
```

That means the decision JSON is available as `$1` inside the shell command. A safe command string should explicitly pass it to a script:

```sh
export TARIT_AUTOSCALE_PROVIDER_CMD='exec /usr/local/bin/taritd-autoscale-provider "$1"'
```

Provider expectations:

| Input | Meaning |
| --- | --- |
| `action` | `scale_out` or `scale_in`. |
| `target_nodes` | Desired healthy node count after action. |
| `current_nodes` | Healthy fresh nodes observed by the leader. |
| `free_vcpus` | Aggregate free vCPUs among healthy fresh nodes. |
| `victim` | Least-loaded node for scale-in, or `null` for scale-out. |
| `region`, `zone`, `cloud` | Topology labels from the leader node's environment. |

The provider should be idempotent. The autoscaler may retry after a cooldown if the cluster still appears under or over target.

## Example AWS provider sketch

```sh
#!/usr/bin/env bash
set -euo pipefail

decision_json="$1"
action=$(printf '%s' "$decision_json" | jq -r '.action')
target=$(printf '%s' "$decision_json" | jq -r '.target_nodes')
victim=$(printf '%s' "$decision_json" | jq -r '.victim // empty')
region=$(printf '%s' "$decision_json" | jq -r '.region')

asg="taritd-${region}"

case "$action" in
  scale_out)
    aws autoscaling set-desired-capacity \
      --auto-scaling-group-name "$asg" \
      --desired-capacity "$target" \
      --honor-cooldown \
      --region "$region"
    ;;
  scale_in)
    if [ -n "$victim" ]; then
      # Example policy:
      # 1. Deregister victim from the external load balancer.
      # 2. Stop sending new API traffic to victim.
      # 3. Wait for sandbox_count to reach 0, or snapshot and recreate stateful VMs.
      # 4. Terminate the corresponding EC2 instance.
      instance_id=$(lookup_instance_id_from_host_id "$victim")
      aws autoscaling terminate-instance-in-auto-scaling-group \
        --instance-id "$instance_id" \
        --should-decrement-desired-capacity \
        --region "$region"
    else
      aws autoscaling set-desired-capacity \
        --auto-scaling-group-name "$asg" \
        --desired-capacity "$target" \
        --honor-cooldown \
        --region "$region"
    fi
    ;;
esac
```

Production AWS notes:

- Store `host_id -> instance_id` in tags, instance metadata, or a provider inventory table.
- New instances should run `taritd` on boot with the shared `TARIT_DATABASE_URL` and `TARIT_PEER_SECRET`.
- New nodes self-register through the normal 5 second heartbeat.
- Use target group deregistration delay for public API traffic.
- Keep peer traffic private inside security groups.

## Example GCP provider sketch

```sh
#!/usr/bin/env bash
set -euo pipefail

decision_json="$1"
action=$(printf '%s' "$decision_json" | jq -r '.action')
target=$(printf '%s' "$decision_json" | jq -r '.target_nodes')
victim=$(printf '%s' "$decision_json" | jq -r '.victim // empty')
zone=$(printf '%s' "$decision_json" | jq -r '.zone')
mig="taritd-${zone}"

case "$action" in
  scale_out)
    gcloud compute instance-groups managed resize "$mig" \
      --zone "$zone" \
      --size "$target"
    ;;
  scale_in)
    if [ -n "$victim" ]; then
      # Prefer deleting a specific instance after drain when host_id mapping is known.
      instance=$(lookup_gce_instance_from_host_id "$victim")
      gcloud compute instance-groups managed delete-instances "$mig" \
        --zone "$zone" \
        --instances "$instance"
    else
      gcloud compute instance-groups managed resize "$mig" \
        --zone "$zone" \
        --size "$target"
    fi
    ;;
esac
```

## Terraform provider sketch

A Terraform-backed provider can write a variable file or call Terraform Cloud/Enterprise:

```sh
#!/usr/bin/env bash
set -euo pipefail

decision_json="$1"
target=$(printf '%s' "$decision_json" | jq -r '.target_nodes')
region=$(printf '%s' "$decision_json" | jq -r '.region')

cat > ./.taritd-autoscale.auto.tfvars.json <<EOF
{
  "taritd_region": "$region",
  "taritd_desired_nodes": $target
}
EOF

terraform apply -auto-approve
```

Use locking and remote state. Terraform actuation is usually slower than native ASG or MIG APIs, so set thresholds and cooldowns accordingly.

## Cross-cloud and region-aware operation

The autoscaler uses one leader's view of aggregate cluster capacity. For multi-region or multi-cloud production deployments, extend the provider or fleet schema to track:

| Desired signal | Why it matters |
| --- | --- |
| Per-host `region`, `zone`, `cloud` | Scale where capacity is actually low. |
| Node lifecycle state | Avoid choosing already draining or terminating nodes. |
| Instance ID or provider resource ID | Terminate or resize specific resources safely. |
| Load balancer registration state | Remove nodes from traffic before scale-in. |
| Snapshot/storage locality | Restore stateful VMs in the same zone or from shared storage. |
| Warm pool depth per node | Scale based on ready capacity rather than raw free slots. |

Until those fields are in `fleet_hosts`, keep this inventory in the provider system and treat `TARIT_REGION`, `TARIT_ZONE`, and `TARIT_CLOUD` as labels for the leader's control plane instance.

## Drain and scale-in safety

For scale-in, Rust selects a victim and invokes the provider. It does not mark a host as draining in PostgreSQL and it does not move VMs automatically. The provider must implement the drain policy.

Recommended safe scale-in sequence:

1. Mark the chosen victim as draining in the provider inventory or an external state store.
2. Remove the victim from the external load balancer so it receives no new public API traffic.
3. Keep the `taritd` process running so existing owner-routed operations can complete.
4. Wait for `sandbox_count` to reach 0 if workloads are disposable.
5. For stateful workloads, snapshot VMs on the victim and preserve each snapshot's `host_id` and path. Because snapshots are node-local today, evacuation requires either restoring before termination on that same node, copying snapshots externally, or using shared storage.
6. Stop remaining VMs or restore them elsewhere according to workload policy.
7. Terminate the underlying instance only after VM ownership is cleared or safely recreated.
8. Verify the node disappears from `healthy_nodes` after heartbeat freshness expires.

If node-local snapshots are required for recovery, do not terminate a victim until snapshots are copied to durable storage.

## Alerting recommendations

Alert on:

- `cluster_free_vcpus` below scale-out threshold for more than one cooldown.
- `healthy_nodes` below `TARIT_AUTOSCALE_MIN`.
- Fleet heartbeat failures.
- Fleet ownership write failures.
- Provider command failures or non-zero exits.
- Nodes with stale heartbeats but non-zero sandbox count.
- Repeated create 429s or high `Retry-After` values.
- Peer HTTP errors during owner forwarding.

## Testing autoscaling safely

1. Start with `TARIT_AUTOSCALE=true` and no provider command. Confirm log-only decisions.
2. Use a provider command that logs `$1` to a controlled file in the project or service log path.
3. Set a high `TARIT_AUTOSCALE_OUT_FREE_VCPUS` in a small test cluster to force scale-out.
4. Set a low node max and high free capacity to force scale-in.
5. Verify the provider is idempotent before allowing it to terminate instances.
6. Test loss of the leader node and confirm another node acquires the lease after the 30 second TTL.
