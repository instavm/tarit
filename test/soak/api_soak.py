#!/usr/bin/env python3
"""Hold N sandboxes busy through taritd's public API for a fixed time.

Each worker owns one VM and loops over the operations an agent platform
issues: run a command and check its output, write a marker to disk and read
it back, checkpoint and restore into a new VM, fork, pause and resume, and
recycle (delete and create). Every operation that reads state checks it: a
restored or forked VM must hold the marker its source wrote.

A monitor samples taritd's memory and open files, the host's free memory,
and the number of VMM processes. At the end every VM and snapshot the soak
made is deleted and the host is checked for leftovers.

    TARIT_URL=http://127.0.0.1:8080 TARIT_API_KEY=... \\
      test/soak/api_soak.py --vms 100 --minutes 30 --out soak.json

Exit status 1 when the error rate exceeds --max-error-rate, data checks
fail, or VMs, snapshots or VMM processes are left behind.
"""

import argparse
import json
import os
import random
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from collections import defaultdict

OPS = {  # name: weight
    "exec": 40,
    "disk": 20,
    "checkpoint": 10,
    "fork": 8,
    "pause": 7,
    "recycle": 15,
}


class Api:
    def __init__(self, url, key, timeout):
        self.url = url.rstrip("/")
        self.headers = {"X-API-Key": key, "Content-Type": "application/json"}
        self.timeout = timeout

    def call(self, method, path, body=None):
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(
            self.url + path, method=method, headers=self.headers, data=data
        )
        try:
            with urllib.request.urlopen(request, timeout=self.timeout) as response:
                text = response.read()
        except urllib.error.HTTPError as error:
            detail = error.read().decode(errors="replace")[:300]
            raise RuntimeError(f"{method} {path}: {error.code} {detail}") from None
        return json.loads(text) if text else None

    def exec(self, vm, command):
        out = self.call(
            "POST", "/v1/execute", {"vm_id": vm, "command": command, "timeout_ms": 60_000}
        )
        if out.get("exit_code") != 0:
            raise RuntimeError(f"exec {command!r}: exit {out.get('exit_code')} {out.get('stderr', '')[:200]}")
        return out.get("stdout", "")


class Stats:
    def __init__(self):
        self.lock = threading.Lock()
        self.latency = defaultdict(list)
        self.errors = defaultdict(int)
        self.error_samples = []
        self.integrity_failures = []

    def ok(self, op, seconds):
        with self.lock:
            self.latency[op].append(seconds * 1000)

    def failed(self, op, error):
        with self.lock:
            self.errors[op] += 1
            if len(self.error_samples) < 50:
                self.error_samples.append({"op": op, "error": str(error)[:400], "at": time.time()})

    def integrity(self, what):
        with self.lock:
            self.integrity_failures.append(what)


class Worker(threading.Thread):
    def __init__(self, index, api, args, stats, deadline, created, snapshots):
        super().__init__(daemon=True)
        self.index = index
        self.api = api
        self.args = args
        self.stats = stats
        self.deadline = deadline
        self.created = created  # every VM id this soak created, for cleanup
        self.snapshots = snapshots
        self.rng = random.Random(args.seed * 1000 + index)
        self.vm = None
        self.marker = 0

    def create(self):
        body = {"vcpus": self.args.vcpus, "memory_mib": self.args.memory_mib}
        if self.args.image:
            body["image"] = self.args.image
        vm = self.api.call("POST", "/v1/vms", body)["id"]
        self.created.add(vm)
        return vm

    def delete(self, vm):
        try:
            self.api.call("DELETE", f"/v1/vms/{vm}")
        except RuntimeError as error:
            if ": 404" not in str(error):
                raise

    def write_marker(self):
        self.marker += 1
        value = f"{self.index}-{self.marker}"
        self.api.exec(self.vm, f"sh -c 'echo {value} > /root/soak.marker && sync'")
        return value

    def check_marker(self, vm, expected, what):
        got = self.api.exec(vm, "cat /root/soak.marker").strip()
        if got != expected:
            self.stats.integrity(f"{what}: worker {self.index} expected {expected!r}, got {got!r}")
            raise RuntimeError(f"{what}: marker {got!r} != {expected!r}")

    def op(self, name):
        if name == "exec":
            n = self.rng.randint(2, 99999)
            out = self.api.exec(self.vm, f"sh -c 'echo $(({n} * {n}))'").strip()
            if out != str(n * n):
                self.stats.integrity(f"exec: worker {self.index} {n}^2 gave {out!r}")
                raise RuntimeError(f"exec arithmetic gave {out!r}")
        elif name == "disk":
            self.check_marker(self.vm, self.write_marker(), "disk read-back")
        elif name == "checkpoint":
            expected = self.write_marker()
            snapshot = self.api.call("POST", f"/v1/vms/{self.vm}/snapshot", {"diff": True})["snapshot_id"]
            self.snapshots.add(snapshot)
            restored = self.api.call("POST", "/v1/restore", {"snapshot_id": snapshot})["id"]
            self.created.add(restored)
            self.check_marker(restored, expected, "restore")
            old, self.vm = self.vm, restored
            self.delete(old)
            self.api.call("DELETE", f"/v1/snapshots/{snapshot}")
            self.snapshots.discard(snapshot)
        elif name == "fork":
            expected = self.write_marker()
            child = self.api.call("POST", f"/v1/vms/{self.vm}/fork", {})["vm"]["id"]
            self.created.add(child)
            try:
                self.check_marker(child, expected, "fork")
            finally:
                self.delete(child)
        elif name == "pause":
            self.api.call("POST", f"/v1/vms/{self.vm}/pause")
            self.api.call("POST", f"/v1/vms/{self.vm}/resume")
            self.api.exec(self.vm, "true")
        elif name == "recycle":
            old, self.vm = self.vm, None
            self.delete(old)
            self.vm = self.create()
            self.marker = 0
            self.write_marker()

    def run(self):
        names = list(OPS)
        weights = [OPS[name] for name in names]
        while time.time() < self.deadline:
            if self.vm is None:
                started = time.perf_counter()
                try:
                    self.vm = self.create()
                    self.marker = 0
                    self.write_marker()
                    self.stats.ok("create", time.perf_counter() - started)
                except Exception as error:  # noqa: BLE001 - every failure is recorded
                    self.stats.failed("create", error)
                    time.sleep(1)
                continue
            name = self.rng.choices(names, weights)[0]
            started = time.perf_counter()
            try:
                self.op(name)
                self.stats.ok(name, time.perf_counter() - started)
            except Exception as error:  # noqa: BLE001
                self.stats.failed(name, error)
                # Start over on a fresh VM; the old one may be in any state.
                if self.vm is not None:
                    try:
                        self.delete(self.vm)
                    except Exception:  # noqa: BLE001
                        pass
                self.vm = None
            if self.args.think_ms:
                time.sleep(self.rng.uniform(0, self.args.think_ms / 1000))
        if self.vm is not None:
            try:
                self.delete(self.vm)
            except Exception as error:  # noqa: BLE001
                self.stats.failed("final-delete", error)


def taritd_pid(port):
    try:
        out = subprocess.run(["pgrep", "-f", "taritd"], capture_output=True, text=True).stdout
    except FileNotFoundError:
        return None
    for pid in out.split():
        try:
            environ = open(f"/proc/{pid}/environ", "rb").read().split(b"\0")
        except OSError:
            continue
        if any(entry.startswith(b"TARIT_LISTEN=") and entry.endswith(f":{port}".encode()) for entry in environ):
            return int(pid)
    return None


def sample(pid):
    point = {"t": time.time(), "load1": float(open("/proc/loadavg").read().split()[0])}
    meminfo = dict(line.split(":", 1) for line in open("/proc/meminfo"))
    point["host_available_mib"] = int(meminfo["MemAvailable"].split()[0]) // 1024
    point["vmm_processes"] = len(subprocess.run(["pgrep", "-x", "vmm"], capture_output=True, text=True).stdout.split())
    if pid:
        try:
            status = dict(line.split(":", 1) for line in open(f"/proc/{pid}/status"))
            point["taritd_rss_mib"] = int(status["VmRSS"].split()[0]) // 1024
            point["taritd_threads"] = int(status["Threads"])
            point["taritd_fds"] = len(os.listdir(f"/proc/{pid}/fd"))
        except OSError:
            pass
    return point


def percentile(values, p):
    if not values:
        return None
    values = sorted(values)
    return round(values[min(len(values) - 1, int(len(values) * p))], 1)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--vms", type=int, default=100)
    parser.add_argument("--minutes", type=float, default=30)
    parser.add_argument("--image", default=os.environ.get("SOAK_IMAGE"))
    parser.add_argument("--vcpus", type=int, default=1)
    parser.add_argument("--memory-mib", type=int, default=256)
    parser.add_argument("--think-ms", type=int, default=200, help="random pause up to this long between operations")
    parser.add_argument("--max-error-rate", type=float, default=0.001)
    parser.add_argument("--timeout", type=float, default=180)
    parser.add_argument("--seed", type=int, default=1)
    parser.add_argument("--out", default="soak.json")
    args = parser.parse_args()

    url = os.environ.get("TARIT_URL", "http://127.0.0.1:8080")
    api = Api(url, os.environ["TARIT_API_KEY"], args.timeout)
    pid = taritd_pid(url.rsplit(":", 1)[-1].rstrip("/"))
    stats = Stats()
    created, snapshots = set(), set()
    timeline = [sample(pid)]
    baseline_vmm = timeline[0]["vmm_processes"]
    started = time.time()
    deadline = started + args.minutes * 60
    workers = [Worker(i, api, args, stats, deadline, created, snapshots) for i in range(args.vms)]
    for worker in workers:
        worker.start()
    while any(worker.is_alive() for worker in workers):
        time.sleep(10)
        point = sample(pid)
        timeline.append(point)
        done = sum(len(v) for v in stats.latency.values())
        failed = sum(stats.errors.values())
        print(
            f"[{int(point['t'] - started):>5}s] ops {done} failed {failed} "
            f"vmm {point['vmm_processes']} taritd rss {point.get('taritd_rss_mib')} MiB "
            f"fds {point.get('taritd_fds')} host free {point['host_available_mib']} MiB load {point['load1']}",
            flush=True,
        )
    for worker in workers:
        worker.join()

    # Everything this soak made must be gone, by the API and on the host.
    leaked_snapshots = []
    for snapshot in list(snapshots):
        try:
            api.call("DELETE", f"/v1/snapshots/{snapshot}")
        except RuntimeError as error:
            if ": 404" not in str(error):
                leaked_snapshots.append(snapshot)
    leaked_vms = []
    for vm in created:
        try:
            record = api.call("GET", f"/v1/vms/{vm}")
        except RuntimeError as error:
            if ": 404" in str(error):
                continue
            raise
        if record and record.get("status") not in ("stopped", "deleted"):
            leaked_vms.append({"id": vm, "status": record.get("status")})
    time.sleep(5)
    end = sample(pid)
    pool = api.call("GET", "/v1/warm-pool")
    warm = sum(c.get("depth", 0) for c in pool.get("classes", [])) if pool else 0
    # VMM processes left: warm VMs plus at most the pool's in-flight refills.
    extra_vmm = end["vmm_processes"] - warm

    total_ok = sum(len(v) for v in stats.latency.values())
    total_failed = sum(stats.errors.values())
    error_rate = total_failed / max(1, total_ok + total_failed)
    report = {
        "vms": args.vms,
        "minutes": args.minutes,
        "image": args.image,
        "operations": total_ok,
        "failed": total_failed,
        "error_rate": round(error_rate, 6),
        "errors_by_op": dict(stats.errors),
        "error_samples": stats.error_samples,
        "integrity_failures": stats.integrity_failures,
        "latency_ms": {
            op: {"n": len(v), "p50": percentile(v, 0.5), "p95": percentile(v, 0.95), "p99": percentile(v, 0.99), "max": percentile(v, 1.0)}
            for op, v in sorted(stats.latency.items())
        },
        "vms_created": len(created),
        "leaked_vms": leaked_vms,
        "leaked_snapshots": leaked_snapshots,
        "vmm_processes": {"before": baseline_vmm, "after": end["vmm_processes"], "warm_pool": warm},
        "timeline": timeline + [end],
    }
    with open(args.out, "w") as handle:
        json.dump(report, handle, indent=1)

    print(f"\n{total_ok} operations, {total_failed} failed ({error_rate:.4%}), {len(created)} VMs created")
    for op, lat in report["latency_ms"].items():
        print(f"  {op:<10} n={lat['n']:<6} p50 {lat['p50']} ms  p95 {lat['p95']} ms  p99 {lat['p99']} ms")
    for op, count in sorted(stats.errors.items()):
        print(f"  errors {op}: {count}")
    rss = [p["taritd_rss_mib"] for p in report["timeline"] if "taritd_rss_mib" in p]
    if rss:
        print(f"  taritd RSS {rss[0]} -> max {max(rss)} -> {rss[-1]} MiB")
    problems = []
    if error_rate > args.max_error_rate:
        problems.append(f"error rate {error_rate:.4%} above {args.max_error_rate:.4%}")
    if stats.integrity_failures:
        problems.append(f"{len(stats.integrity_failures)} data check failures")
    if leaked_vms:
        problems.append(f"{len(leaked_vms)} VMs left running")
    if leaked_snapshots:
        problems.append(f"{len(leaked_snapshots)} snapshots left")
    if extra_vmm > 4:
        problems.append(f"{extra_vmm} VMM processes beyond the warm pool")
    for problem in problems:
        print(f"FAIL: {problem}")
    print(f"report: {args.out}")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
