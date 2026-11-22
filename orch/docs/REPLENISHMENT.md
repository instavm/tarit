# Warm-pool replenishment

taritd restores warm VMs from a golden snapshot by default. It cold-boots a
builder for each configured restore class, waits for readiness, runs its
optional prewarm command, captures a snapshot and tears down the builder.
Each restored VM gets a private writable overlay.

A create can lease a ready VM. If the pool is empty, an eligible create can
restore from the class’s golden snapshot on demand. A shape without a usable
golden can still require a cold boot.

## Refill controls

- Set `restore = false` on a class to refill by cold boot. Both modes use
  bounded background concurrency. More simultaneous boots can starve live execs.
- Each class has a hard floor, low watermark, target and high watermark.
  Refill starts below the low watermark and aims for the target. The default
  class targets eight VMs. [Warm-pool settings](CONFIGURATION.md#warm-pool)
- `TARIT_REFILL_CGROUP` and `TARIT_REFILL_CPU_WEIGHT` can place refill VMMs
  in a lower-weight CPU cgroup. The default weight is 10 when configured.

## Historical cold-boot benchmark

On a c8i.metal-48xl, a target of 200 and refill concurrency of 48 delivered
p95 create-to-first-command latencies of 38 ms sequentially, 56 ms for a burst
of 100 and 37 ms with 200 ms between creates. All operations completed.
Raising concurrency to 100 made the staggered run time out at 30 seconds.
These measurements describe cold-boot refill on that host. The
[protected KVM workflow](../../.github/workflows/kvm.yml) defines the current
release thresholds.
