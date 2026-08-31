#!/usr/bin/env python3
"""Continuous real-KVM workload for long-lived VM lifecycle qualification."""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import os
import random
import shutil
import sqlite3
import sys
import time
import urllib.error
import urllib.request
import uuid
from dataclasses import dataclass


@dataclass
class Anchor:
    proof: str
    created_at: float
    operations: int = 0


class Soak:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.base_url = args.base_url.rstrip("/")
        self.headers = {
            "X-API-Key": args.api_key,
            "Content-Type": "application/json",
        }
        self.random = random.Random(args.seed)
        self.anchors: dict[str, Anchor] = {}
        self.transient: set[str] = set()
        self.snapshots = 0
        self.operations = 0
        self.started_at = time.monotonic()

    def event(self, kind: str, **fields: object) -> None:
        print(json.dumps({"event": kind, "elapsed_s": round(time.monotonic() - self.started_at, 3), **fields}, sort_keys=True), flush=True)

    def request(self, method: str, path: str, body: object | None = None,
                expected: int | set[int] = 200, timeout: int = 240):
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(
            self.base_url + path, data=data, method=method, headers=self.headers
        )
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                status, payload = response.status, response.read()
        except urllib.error.HTTPError as error:
            status, payload = error.code, error.read()
        allowed = {expected} if isinstance(expected, int) else expected
        if status not in allowed:
            raise AssertionError(
                f"{method} {path}: expected {sorted(allowed)}, got {status}: "
                f"{payload.decode(errors='replace')}"
            )
        self.operations += 1
        return status, json.loads(payload) if payload else None

    def exec(self, vm_id: str, command: str, timeout_ms: int = 60_000) -> str:
        _, row = self.request(
            "POST", "/v1/execute",
            {"vm_id": vm_id, "command": command, "timeout_ms": timeout_ms},
            timeout=max(240, timeout_ms // 1000 + 60),
        )
        assert row["status"] == "completed" and row["exit_code"] == 0, row
        assert not row.get("error"), row
        return (row.get("stdout") or "").rstrip("\r\n")

    @staticmethod
    def parse_state(value: str) -> dict[str, str]:
        return dict(field.split("=", 1) for field in value.split())

    def workload_state(self, vm_id: str) -> dict[str, str]:
        return self.parse_state(
            self.exec(vm_id, "/usr/local/bin/tarit-clone-repair-workload state")
        )

    def install_workload(self, vm_id: str, proof: str) -> None:
        command = r'''set -eu
mkdir -p /usr/libexec/tarit /run/tarit
printf '%s\n' '#!/bin/sh' 'set -eu' 'test "$TARIT_POST_FORK" = 1' 'exec /usr/local/bin/tarit-clone-repair-workload repair-signal "$TARIT_CLONE_ID"' > /usr/libexec/tarit/post-fork
chmod 0755 /usr/libexec/tarit/post-fork
printf '%s\n' PROOF_VALUE > /root/tarit-soak-proof
/usr/local/bin/tarit-clone-repair-workload serve >/tmp/clone-workload.log 2>&1 &
for i in $(seq 1 200); do /usr/local/bin/tarit-clone-repair-workload state >/dev/null 2>&1 && break; sleep 0.05; done
/usr/local/bin/tarit-clone-repair-workload cache long-lived-session >/dev/null
busybox setsid sh -c 'i=0; while :; do i=$((i + 1)); printf "%s\n" "$i" > /tmp/soak-counter.next; mv /tmp/soak-counter.next /tmp/soak-counter; busybox dd if=/dev/urandom of=/tmp/soak-workset bs=4096 count=16 seek=$((i % 256)) conv=notrunc 2>/dev/null; sync; sleep 1; done' </dev/null >/tmp/soak-writer.log 2>&1 &
test "$(cat /root/tarit-soak-proof)" = PROOF_VALUE
/usr/local/bin/tarit-clone-repair-workload state
'''.replace("PROOF_VALUE", proof)
        output = self.exec(vm_id, command)
        assert "clone=cold-boot" in output, output

    def create_anchor(self, index: int) -> str:
        proof = f"anchor-{self.args.seed}-{index}-{uuid.uuid4().hex[:12]}"
        _, row = self.request(
            "POST", "/v1/vms", {"vcpus": 1, "memory_mib": 256}, 201
        )
        vm_id = row["id"]
        assert row["status"] == "running", row
        self.install_workload(vm_id, proof)
        self.anchors[vm_id] = Anchor(proof=proof, created_at=time.monotonic())
        self.event("anchor_created", vm_id=vm_id, proof=proof)
        return vm_id

    def assert_anchor(self, vm_id: str) -> None:
        anchor = self.anchors[vm_id]
        output = self.exec(
            vm_id,
            "set -eu; cat /root/tarit-soak-proof; test -s /tmp/soak-counter; "
            "test ! -e /dev/kvm; ! grep -Eq '(^|[[:space:]])(vmx|svm)([[:space:]]|$)' /proc/cpuinfo",
        ).splitlines()
        assert output and output[0] == anchor.proof, (anchor, output)
        state = self.workload_state(vm_id)
        assert state["cache"] in {"long-lived-session", "-"}, state
        anchor.operations += 1

    def fork_anchor(self, vm_id: str) -> None:
        before = self.workload_state(vm_id)
        ticket = self.exec(vm_id, "/usr/local/bin/tarit-clone-repair-workload ticket")
        _, response = self.request("POST", f"/v1/vms/{vm_id}/fork", {}, 201, 360)
        child = response["vm"]
        child_id = child["id"]
        self.transient.add(child_id)
        assert child["status"] == "running", child
        assert self.exec(child_id, "cat /root/tarit-soak-proof") == self.anchors[vm_id].proof
        after = self.workload_state(child_id)
        for field in ("clone", "prng", "ticket", "prefix"):
            assert after[field] != before[field], (field, before, after)
        assert after["counter"] == "0" and after["cache"] == "-", after
        assert self.exec(
            child_id,
            f"/usr/local/bin/tarit-clone-repair-workload accept-ticket '{ticket}'",
        ) == "rejected"
        child_proof = f"child-{uuid.uuid4().hex[:16]}"
        self.exec(child_id, f"printf '%s\n' {child_proof} > /root/tarit-soak-proof; sync")
        assert self.exec(vm_id, "cat /root/tarit-soak-proof") == self.anchors[vm_id].proof
        self.delete_vm(child_id)
        self.event("fork_verified", source=vm_id, child=child_id)

    def snapshot_restore(self, vm_id: str) -> None:
        if self.snapshots >= self.args.max_snapshots:
            self.assert_anchor(vm_id)
            return
        before = self.workload_state(vm_id)
        _, snapshot = self.request(
            "POST", f"/v1/vms/{vm_id}/snapshot", {"diff": False}, 200, 360
        )
        snapshot_id = snapshot["snapshot_id"]
        self.snapshots += 1
        _, restored = self.request(
            "POST", "/v1/restore", {"snapshot_id": snapshot_id}, 201, 360
        )
        restored_id = restored["id"]
        self.transient.add(restored_id)
        assert self.exec(restored_id, "cat /root/tarit-soak-proof") == self.anchors[vm_id].proof
        after = self.workload_state(restored_id)
        for field in ("clone", "prng", "ticket", "prefix"):
            assert after[field] != before[field], (field, before, after)
        self.delete_vm(restored_id)
        self.event("snapshot_restore_verified", source=vm_id, restored=restored_id, snapshot=snapshot_id)

    def hibernate_resume(self, vm_id: str) -> None:
        before = self.workload_state(vm_id)
        ticket = self.exec(vm_id, "/usr/local/bin/tarit-clone-repair-workload ticket")
        _, hibernated = self.request("POST", f"/v1/vms/{vm_id}/hibernate", {}, 200, 360)
        assert hibernated["status"] == "hibernated", hibernated
        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
            futures = [pool.submit(self.exec, vm_id, f"echo wake-{index}") for index in range(4)]
            assert [future.result(timeout=360) for future in futures] == [f"wake-{index}" for index in range(4)]
        after = self.workload_state(vm_id)
        for field in ("clone", "prng", "ticket", "prefix"):
            assert after[field] != before[field], (field, before, after)
        assert after["counter"] == "0" and after["cache"] == "-", after
        assert self.exec(
            vm_id,
            f"/usr/local/bin/tarit-clone-repair-workload accept-ticket '{ticket}'",
        ) == "rejected"
        self.event("hibernate_resume_verified", vm_id=vm_id)

    def long_hibernate_clock_timer(self, vm_id: str) -> None:
        timer_seconds = self.args.guest_timer_seconds
        before = self.exec(
            vm_id,
            "set -eu; rm -f /tmp/tarit-timer-fired /tmp/tarit-realtime-fired; "
            "read uptime rest < /proc/uptime; "
            f"busybox setsid sh -c 'sleep {timer_seconds}; printf fired > "
            "/tmp/tarit-timer-fired' </dev/null >/tmp/tarit-timer.log 2>&1 & "
            f"deadline=$(($(date +%s) + {timer_seconds})); "
            "busybox setsid sh -c '/usr/local/bin/tarit-clone-repair-workload "
            "wait-realtime \"$1\" && printf fired > /tmp/tarit-realtime-fired' "
            "sh \"$deadline\" </dev/null >/tmp/tarit-realtime-timer.log 2>&1 & "
            "printf '%s %s\\n' \"$uptime\" \"$(date +%s)\"",
        ).split()
        assert len(before) == 2, before
        uptime_before, realtime_before = float(before[0]), int(before[1])
        _, hibernated = self.request("POST", f"/v1/vms/{vm_id}/hibernate", {}, 200, 360)
        assert hibernated["status"] == "hibernated", hibernated
        hold_started = time.monotonic()
        time.sleep(self.args.hibernate_hold_seconds)
        resume_started = time.monotonic()
        immediate = self.exec(
            vm_id,
            "set -eu; read uptime rest < /proc/uptime; "
            "test ! -e /tmp/tarit-timer-fired; "
            "! dmesg | tail -n 300 | grep -Eiq "
            "'watchdog: BUG|soft lockup|hard LOCKUP|Kernel panic|BUG:'; "
            "printf '%s %s\\n' \"$uptime\" \"$(date +%s)\"",
        ).split()
        assert len(immediate) == 2, immediate
        uptime_after, realtime_after = float(immediate[0]), int(immediate[1])
        host_realtime = int(time.time())
        assert uptime_after >= uptime_before, (uptime_before, uptime_after)
        assert uptime_after - uptime_before < timer_seconds, (uptime_before, uptime_after)
        assert abs(realtime_after - host_realtime) <= 3, (realtime_after, host_realtime)
        assert realtime_after - realtime_before >= self.args.hibernate_hold_seconds - 3, (
            realtime_before, realtime_after,
        )

        realtime_timer_deadline = time.monotonic() + 2
        while time.monotonic() < realtime_timer_deadline:
            if self.exec(
                vm_id,
                "if test -e /tmp/tarit-realtime-fired; then echo fired; else echo pending; fi",
            ) == "fired":
                break
            time.sleep(0.05)
        else:
            raise AssertionError("expired absolute-realtime timer did not fire after repair")
        realtime_timer_elapsed = time.monotonic() - resume_started

        timer_deadline = time.monotonic() + timer_seconds + 15
        while time.monotonic() < timer_deadline:
            if self.exec(vm_id, "if test -e /tmp/tarit-timer-fired; then echo fired; else echo pending; fi") == "fired":
                break
            time.sleep(0.25)
        else:
            raise AssertionError("guest monotonic timer did not fire after resume")
        timer_elapsed = time.monotonic() - resume_started
        assert timer_elapsed >= timer_seconds - 1, timer_elapsed
        self.event(
            "long_hibernate_clock_timer_verified",
            vm_id=vm_id,
            hold_seconds=round(resume_started - hold_started, 3),
            guest_timer_seconds=timer_seconds,
            timer_after_resume_seconds=round(timer_elapsed, 3),
            realtime_timer_after_resume_seconds=round(realtime_timer_elapsed, 3),
            guest_uptime_delta_seconds=round(uptime_after - uptime_before, 3),
            guest_realtime_delta_seconds=realtime_after - realtime_before,
            host_realtime_error_seconds=realtime_after - host_realtime,
        )

    def pause_resume(self, vm_id: str) -> None:
        _, paused = self.request("POST", f"/v1/vms/{vm_id}/pause", {}, 200)
        assert paused["status"] == "paused", paused
        _, running = self.request("POST", f"/v1/vms/{vm_id}/resume", {}, 200)
        assert running["status"] == "running", running
        self.assert_anchor(vm_id)
        self.event("pause_resume_verified", vm_id=vm_id)

    def balloon(self, vm_id: str) -> None:
        target = self.random.choice([0, 16, 32, 64])
        _, row = self.request(
            "PUT", f"/v1/vms/{vm_id}/balloon", {"target_mib": target}, 200
        )
        assert row["target_mib"] == target, row
        self.assert_anchor(vm_id)
        self.event("balloon_verified", vm_id=vm_id, target_mib=target)

    def mutate(self, vm_id: str) -> None:
        anchor = self.anchors[vm_id]
        proof = f"anchor-{self.args.seed}-{anchor.operations}-{uuid.uuid4().hex[:12]}"
        self.exec(vm_id, f"printf '%s\n' {proof} > /root/tarit-soak-proof; sync")
        anchor.proof = proof
        self.assert_anchor(vm_id)
        self.event("guest_work_verified", vm_id=vm_id, proof=proof)

    def assert_global_invariants(self) -> None:
        _, rows = self.request("GET", "/v1/vms")
        by_id = {row["id"]: row for row in rows}
        for vm_id in self.anchors:
            assert vm_id in by_id and by_id[vm_id]["status"] == "running", (vm_id, by_id)
        with sqlite3.connect(self.args.database) as database:
            resident = database.execute(
                "select id,pid from vms where status in ('running','paused','suspended')"
            ).fetchall()
        expected = set(self.anchors) | self.transient
        assert {vm_id for vm_id, _ in resident} == expected, (resident, expected)
        for vm_id, pid in resident:
            assert pid and os.path.exists(f"/proc/{pid}"), (vm_id, pid)

    def delete_vm(self, vm_id: str) -> None:
        self.request("DELETE", f"/v1/vms/{vm_id}", expected=204, timeout=360)
        self.transient.discard(vm_id)

    def assert_storage_headroom(self) -> None:
        if not self.args.storage_path:
            return
        free_bytes = shutil.disk_usage(self.args.storage_path).free
        if free_bytes < self.args.min_free_bytes:
            self.event(
                "storage_floor",
                path=self.args.storage_path,
                free_bytes=free_bytes,
                minimum_free_bytes=self.args.min_free_bytes,
            )
            raise RuntimeError(
                f"storage floor reached: {free_bytes} < {self.args.min_free_bytes}"
            )

    def cleanup(self) -> None:
        for vm_id in list(self.transient) + list(self.anchors):
            try:
                self.request("DELETE", f"/v1/vms/{vm_id}", expected={204, 404}, timeout=120)
            except Exception as error:
                self.event("cleanup_error", vm_id=vm_id, error=str(error))
        self.transient.clear()
        self.anchors.clear()

    def run(self) -> None:
        self.assert_storage_headroom()
        for index in range(self.args.anchors):
            self.create_anchor(index)
            self.assert_storage_headroom()
        if self.args.hibernate_hold_seconds:
            self.long_hibernate_clock_timer(next(iter(self.anchors)))
            self.assert_global_invariants()
        deadline = time.monotonic() + self.args.duration_seconds
        actions = [
            self.assert_anchor, self.assert_anchor, self.mutate, self.fork_anchor,
            self.snapshot_restore, self.hibernate_resume, self.pause_resume, self.balloon,
        ]
        while time.monotonic() < deadline:
            self.assert_storage_headroom()
            vm_id = self.random.choice(list(self.anchors))
            action = self.random.choice(actions)
            started = time.monotonic()
            action(vm_id)
            self.assert_global_invariants()
            self.event(
                "operation_complete", action=action.__name__, vm_id=vm_id,
                duration_ms=round((time.monotonic() - started) * 1000, 3),
                operations=self.operations, snapshots=self.snapshots,
            )
            time.sleep(self.args.interval_seconds)
        minimum_age = min(time.monotonic() - anchor.created_at for anchor in self.anchors.values())
        self.event(
            "soak_pass", seed=self.args.seed, operations=self.operations,
            snapshots=self.snapshots, anchors=len(self.anchors),
            minimum_anchor_age_s=round(minimum_age, 3),
        )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--api-key", required=True)
    parser.add_argument("--database", required=True)
    parser.add_argument("--vmm", required=True)
    parser.add_argument("--jail-uid-base", type=int, required=True)
    parser.add_argument("--jail-uid-count", type=int, required=True)
    parser.add_argument("--max-vms", type=int, required=True)
    parser.add_argument("--max-snapshots", type=int, default=8)
    parser.add_argument("--seeds", default="7")
    parser.add_argument("--steps", type=int, default=20)
    parser.add_argument("--duration-seconds", type=int, default=1800)
    parser.add_argument("--interval-seconds", type=float, default=1.0)
    parser.add_argument("--anchors", type=int, default=3)
    parser.add_argument("--hibernate-hold-seconds", type=int, default=0)
    parser.add_argument("--guest-timer-seconds", type=int, default=5)
    parser.add_argument("--storage-path")
    parser.add_argument("--min-free-bytes", type=int, default=0)
    args = parser.parse_args()
    args.seed = int(args.seeds.split(",", 1)[0])
    if args.duration_seconds < 60 or args.anchors < 2 or args.anchors > args.max_vms - 2:
        parser.error("duration must be at least 60 seconds and anchors must leave two transient slots")
    if args.min_free_bytes < 0 or (args.min_free_bytes and not args.storage_path):
        parser.error("a non-negative storage floor requires --storage-path")
    if args.hibernate_hold_seconds and args.hibernate_hold_seconds <= args.guest_timer_seconds + 2:
        parser.error("hibernate hold must exceed the guest timer by at least three seconds")
    if args.guest_timer_seconds < 2:
        parser.error("guest timer must be at least two seconds")
    return args


def main() -> int:
    args = parse_args()
    soak = Soak(args)
    try:
        soak.run()
        return 0
    except Exception as error:
        soak.event("soak_failure", error=repr(error), operations=soak.operations)
        raise
    finally:
        soak.cleanup()


if __name__ == "__main__":
    sys.exit(main())
