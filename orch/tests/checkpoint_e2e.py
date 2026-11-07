#!/usr/bin/env python3
"""Checkpoint loop against a running taritd: snapshot, restore, change, repeat.

Every snapshot taritd takes is a standalone full snapshot, but after the
first one (or after a restore or wake) the VMM pauses the guest only for the
pages changed since and copies the rest from that earlier snapshot. This
drives every path that feeds such a base and checks each restored checkpoint
byte for byte:

  * cold boot -> s1 (no base) -> s2 with diff=true (base s1) -> s3 (base s2)
  * restore s2 -> change -> s4 (base: the restored snapshot)
  * hibernate -> wake (wake image deleted) -> change -> s5 (base: wake image)
  * snapshot of a paused VM keeps it paused; a running VM keeps running
  * deleting s1, the base s2 was taken against, leaves s2 restorable

Several loops run at once on the same taritd. Needs TARIT_BASE_URL and
TARIT_API_KEY; CHECKPOINT_E2E_PARALLEL (default 2) and
CHECKPOINT_E2E_MEMORY_MIB (default 384) size the run. Exits non-zero on any
failure.
"""

import json
import os
import sys
import threading
import time
import urllib.error
import urllib.request

BASE_URL = os.environ["TARIT_BASE_URL"].rstrip("/")
API_KEY = os.environ["TARIT_API_KEY"]
PARALLEL = int(os.environ.get("CHECKPOINT_E2E_PARALLEL", "2"))
MEMORY_MIB = int(os.environ.get("CHECKPOINT_E2E_MEMORY_MIB", "384"))

# Data in guest RAM (tmpfs), data on the guest disk, and one marker per step.
SEED = (
    "head -c 48M /dev/urandom > /tmp/ram.bin && sha256sum /tmp/ram.bin | cut -c1-16 > /root/ram.sum && "
    "head -c 16M /dev/urandom > /root/disk.bin && sha256sum /root/disk.bin | cut -c1-16 > /root/disk.sum && "
    "rm -f /root/m.* && echo 1 > /root/m.1 && sync"
)
CHECK = (
    "ls /root | grep '^m\\.' | sort -t. -k2 -n | tr '\\n' ' '; "
    "[ \"$(sha256sum /tmp/ram.bin | cut -c1-16)\" = \"$(cat /root/ram.sum)\" ] && echo ram-ok; "
    "[ \"$(sha256sum /root/disk.bin | cut -c1-16)\" = \"$(cat /root/disk.sum)\" ] && echo disk-ok"
)
# Longest gap a guest program sees while a snapshot runs, if python3 exists.
STALL_PROBE = r"""command -v python3 >/dev/null || exit 0
cat > /tmp/stall.py <<'PY'
import sys, time
end = time.monotonic() + float(sys.argv[1]); last = time.monotonic(); worst = 0.0
while time.monotonic() < end:
    time.sleep(0.001); now = time.monotonic(); worst = max(worst, now - last); last = now
open('/tmp/stall.out', 'w').write('%.0f' % (worst * 1000))
PY
rm -f /tmp/stall.out; (setsid python3 /tmp/stall.py 4 </dev/null >/dev/null 2>&1 &)"""


class Failure(Exception):
    pass


def request(method, path, body=None, timeout=180):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(
        BASE_URL + path,
        data=data,
        method=method,
        headers={"X-API-Key": API_KEY, "Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as response:
            raw, code = response.read(), response.status
    except urllib.error.HTTPError as error:
        raw, code = error.read(), error.code
    try:
        return code, json.loads(raw) if raw else None
    except ValueError:
        return code, raw.decode(errors="replace")


def expect(method, path, body=None, codes=(200,)):
    code, data = request(method, path, body)
    if code not in codes:
        raise Failure("%s %s -> %s %s" % (method, path, code, data))
    return data


class Loop:
    def __init__(self, index):
        self.name = "loop%d" % index
        self.vms = []
        self.snapshots = []
        self.markers = []
        self.stalls = []

    def log(self, message):
        print("[%s] %s" % (self.name, message), flush=True)

    def exec(self, vm, command, timeout_ms=60000):
        data = expect(
            "POST",
            "/v1/execute",
            {"vm_id": vm, "command": command, "timeout_ms": timeout_ms},
        )
        if data.get("exit_code") != 0:
            raise Failure("exec %r on %s: %s" % (command[:60], vm, data))
        return data.get("stdout", "").strip()

    def create(self):
        vm = expect(
            "POST", "/v1/vms", {"vcpus": 2, "memory_mib": MEMORY_MIB}, codes=(201,)
        )["id"]
        self.vms.append(vm)
        return vm

    def mark(self, vm):
        self.markers.append(len(self.markers) + 1)
        self.exec(vm, "echo x > /root/m.%d && sync" % self.markers[-1])

    def snapshot(self, vm, label, diff=False):
        self.exec(vm, STALL_PROBE)
        time.sleep(0.5)
        started = time.monotonic()
        snapshot = expect("POST", "/v1/vms/%s/snapshot" % vm, {"diff": diff})["snapshot_id"]
        elapsed = (time.monotonic() - started) * 1000
        self.snapshots.append(snapshot)
        # A running VM must still be running and responsive afterwards.
        status = expect("GET", "/v1/vms/%s" % vm)["status"]
        if status != "running":
            raise Failure("%s: VM is %s after a snapshot of a running VM" % (label, status))
        stall = self.exec(vm, "i=0; while [ ! -f /tmp/stall.out ] && [ $i -lt 100 ]; do sleep 0.1; i=$((i+1)); done; cat /tmp/stall.out 2>/dev/null || true")
        self.stalls.append((label, stall))
        self.log("%-34s %6.0f ms, guest stall %s ms" % (label, elapsed, stall or "n/a"))
        return snapshot, list(self.markers)

    def verify(self, vm, label, markers):
        got = self.exec(vm, CHECK).split()
        want = ["m.%d" % m for m in markers] + ["ram-ok", "disk-ok"]
        if got != want:
            raise Failure("%s: guest sees %s, expected %s" % (label, got, want))
        self.log("%-34s ok (%s)" % (label, " ".join(got)))

    def restore(self, snapshot, label, markers, keep=True):
        vm = expect("POST", "/v1/restore", {"snapshot_id": snapshot}, codes=(200, 201))["id"]
        self.vms.append(vm)
        self.verify(vm, label, markers)
        if not keep:
            # Only checked, never used again: free its slot so a loop holds
            # at most four VMs at once.
            expect("DELETE", "/v1/vms/" + vm, codes=(204,))
            self.vms.remove(vm)
        return vm

    def run(self):
        vm = self.create()
        self.exec(vm, SEED, timeout_ms=120000)
        self.markers = [1]
        s1, m1 = self.snapshot(vm, "s1 cold (no base)")
        self.mark(vm)
        s2, m2 = self.snapshot(vm, "s2 diff=true (base s1)", diff=True)
        self.mark(vm)
        s3, m3 = self.snapshot(vm, "s3 (base s2)")

        # A paused VM stays paused across a snapshot and resumes intact.
        expect("POST", "/v1/vms/%s/pause" % vm, {})
        sp = expect("POST", "/v1/vms/%s/snapshot" % vm, {})["snapshot_id"]
        self.snapshots.append(sp)
        status = expect("GET", "/v1/vms/%s" % vm)["status"]
        if status != "paused":
            raise Failure("snapshot of a paused VM left it %s" % status)
        expect("POST", "/v1/vms/%s/resume" % vm, {})
        self.verify(vm, "paused snapshot, resumed source", m3)

        r2 = self.restore(s2, "restore s2", m2)
        self.markers = list(m2)
        self.mark(r2)
        s4, m4 = self.snapshot(r2, "s4 after restore (base s2)")
        r4 = self.restore(s4, "restore s4", m4)
        self.restore(s3, "restore s3", m3, keep=False)
        self.restore(sp, "restore paused snapshot", m3, keep=False)

        # Hibernate deletes nothing visible; wake consumes the wake image.
        expect("POST", "/v1/vms/%s/hibernate" % r4, {})
        expect("POST", "/v1/vms/%s/resume" % r4, {})
        self.markers = list(m4)
        self.verify(r4, "after wake", m4)
        self.mark(r4)
        s5, m5 = self.snapshot(r4, "s5 after wake (base wake image)")
        self.restore(s5, "restore s5", m5, keep=False)

        # s2 is standalone: deleting the snapshot it was taken against is fine.
        expect("DELETE", "/v1/snapshots/" + s1, codes=(204,))
        self.snapshots.remove(s1)
        self.restore(s2, "restore s2 after deleting s1", m2, keep=False)

    def cleanup(self):
        for vm in self.vms:
            request("DELETE", "/v1/vms/" + vm)
        for snapshot in self.snapshots:
            request("DELETE", "/v1/snapshots/" + snapshot)


def main():
    loops = [Loop(i) for i in range(PARALLEL)]
    errors = []

    def worker(loop):
        try:
            loop.run()
        except Exception as error:  # noqa: BLE001 - report every loop's failure
            errors.append("%s: %s" % (loop.name, error))
        finally:
            loop.cleanup()

    threads = [threading.Thread(target=worker, args=(loop,)) for loop in loops]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    for error in errors:
        print("FAIL: " + error, flush=True)
    if errors:
        return 1
    print("PASS: %d checkpoint loops" % len(loops), flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
