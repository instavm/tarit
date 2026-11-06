#!/usr/bin/env python3
"""Markdown tables from vmbench.py and agentbench.py result files.

Every pass of a run writes one JSON file per harness. Given all of them, this
prints the median of each metric across passes, with the per-pass values in
brackets, and the correctness checks each VMM passed, so a published table
can be regenerated from the raw reports.

    python3 test/compare/report.py --vm vm-rep*.json --agent agent-rep*.json
"""
import argparse, json, statistics

# (label, harness, scenario path, lower is better). Paths index the per-VMM
# result; "/p50" picks the median of a summary.
ROWS = [
    ("Cold boot to first command (ms)", "vm", "cold/time_to_interactive_ms/p50", True),
    ("Boot to agent ready, coding session (ms)", "agent", "session/metrics/boot_to_ready_ms", True),
    ("Exec round trip, `true` (ms)", "vm", "exec/true_ms/p50", True),
    ("Full snapshot (ms)", "vm", "snapshot/snapshot_create_ms", True),
    ("Restore to first command (ms)", "vm", "snapshot/restore_to_exec_ms/p50", True),
    ("Incremental snapshot, durable (ms)", "vm", "chain/incremental_snapshot_ms/p50", True),
    ("Restore to first command, after a chain (ms)", "vm", "chain/restore_to_exec_ms/p50", True),
    ("Checkpoint on disk after 10 steps (MiB)", "vm", "chain/checkpoint_bytes_on_disk_last", True),
    ("Snapshot during an agent session (ms)", "agent", "resume/metrics/snapshot_ms/p50", True),
    ("Restore to agent ready (ms)", "agent", "resume/metrics/restore_to_ready_ms/p50", True),
    ("Fork: per clone ready (ms)", "agent", "fork/metrics/per_clone_ready_ms/p50", True),
    ("20 clones of one snapshot: all ready (ms)", "vm", "fanout/all_ready_ms", True),
    ("20 clones of one snapshot: disk used (MiB)", "vm", "fanout/disk_used_mib", True),
    ("Burst of 20 cold boots at once: all ready (ms)", "vm", "burst/all_ready_ms", True),
    ("Burst of sandboxes: first command (ms)", "agent", "burst/metrics/time_to_first_command_ms/p50", True),
    ("Bulk download, checksummed (MB/s)", "agent", "net/metrics/download_MBps", False),
    ("Disk: 20k small files (ms)", "agent", "disk/metrics/step_ms/20k small files", True),
    ("Disk: 256 MiB write + fsync (ms)", "agent", "disk/metrics/step_ms/256 MiB write + fsync", True),
    ("Disk: compile (ms)", "agent", "disk/metrics/step_ms/compile sqlite-sized C", True),
    ("VMM memory after restore (MiB PSS)", "vm", "snapshot/vmm_pss_mib_after_restore/p50", True),
    ("Reliability: VMs that stopped answering", "vm", "reliab/exec_dead", True),
]
SCALE = {"chain/checkpoint_bytes_on_disk_last": 1.0 / (1 << 20)}


def lookup(result, path):
    node, parts = result, path.split("/")
    while parts:
        if not isinstance(node, dict):
            return None
        if parts[0] in node:
            node = node[parts.pop(0)]
        else:
            # Some metric names contain a slash ("step_ms/git clone").
            node, parts = node.get("/".join(parts)), []
    return node if isinstance(node, (int, float)) and not isinstance(node, bool) else None


def fmt(value):
    if value is None:
        return "-"
    return "%.1f" % value if abs(value) < 100 else "{:,.0f}".format(value)


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--vm", nargs="*", default=[])
    p.add_argument("--agent", nargs="*", default=[])
    a = p.parse_args()
    runs = {"vm": [json.load(open(f)) for f in a.vm],
            "agent": [json.load(open(f)) for f in a.agent]}

    print("| Measure | tarit | Firecracker | Better |")
    print("|---|---|---|---|")
    for label, harness, path, lower in ROWS:
        cells, medians = [], []
        for kind in ("tarit", "firecracker"):
            values = [lookup(r.get(kind, {}), path) for r in runs[harness]]
            values = [v * SCALE.get(path, 1.0) for v in values if v is not None]
            median = statistics.median(values) if values else None
            medians.append(median)
            per_pass = ", ".join(fmt(v) for v in values)
            cells.append("%s [%s]" % (fmt(median), per_pass) if len(values) > 1 else fmt(median))
        t, f = medians
        if t is None or f is None:
            better = "-"
        elif abs(t - f) <= 0.03 * max(abs(t), abs(f), 1e-9):
            better = "even"
        else:
            better = "tarit" if (t < f) == lower else "Firecracker"
        print("| %s | %s | %s | %s |" % (label, cells[0], cells[1], better))

    if runs["agent"]:
        print()
        print("| Agent-suite checks passed | tarit | Firecracker |")
        print("|---|---|---|")
        for scenario in runs["agent"][0].get("tarit", {}):
            row = []
            for kind in ("tarit", "firecracker"):
                passes = [r.get(kind, {}).get(scenario, {}) for r in runs["agent"]]
                row.append(", ".join("%s/%s" % (s.get("passed", "-"), s.get("total", "-")) for s in passes))
            print("| %s | %s | %s |" % (scenario, row[0], row[1]))
        for kind in ("tarit", "firecracker"):
            lost = sorted({"%s: %s" % (name, c["check"]) for r in runs["agent"]
                           for name, s in r.get(kind, {}).items() if isinstance(s, dict)
                           for c in s.get("checks", []) if not c.get("ok")})
            print()
            print("%s failed checks: %s" % (kind, "; ".join(lost) if lost else "none"))


if __name__ == "__main__":
    main()
