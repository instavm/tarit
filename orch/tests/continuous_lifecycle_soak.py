#!/usr/bin/env python3
"""Continuous real-KVM workload for long-lived VM lifecycle qualification."""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import os
import random
import shlex
import shutil
import sqlite3
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
import uuid
from dataclasses import dataclass


def shell_write_command(path: str, value: str) -> str:
    """Build a shell command that writes one exact line to a guest file."""
    return f"printf '%s\\n' {shlex.quote(value)} > {shlex.quote(path)}; sync"


@dataclass
class Anchor:
    proof: str
    created_at: float
    vcpus: int
    operations: int = 0


@dataclass
class HibernationSentinel:
    vm_id: str
    proof: str
    before: dict[str, str]
    ticket: str
    uptime_before: float
    realtime_before: int
    started_at: float
    monotonic_marker: str
    realtime_marker: str


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
        self.sentinel: HibernationSentinel | None = None
        self.snapshots = 0
        self.operations = 0
        self.operations_lock = threading.Lock()
        self.action_latencies_ms: dict[str, list[float]] = {}
        self.fork_metric_samples: dict[str, list[int]] = {}
        self.fork_metric_samples_by_vcpu: dict[int, dict[str, list[int]]] = {}
        self.fork_paths: dict[str, int] = {}
        self.fork_terminations: dict[str, int] = {}
        self.started_at = time.monotonic()

    @staticmethod
    def percentile(values: list[float], percentile: float) -> float:
        if not values:
            return 0.0
        ordered = sorted(values)
        index = round((len(ordered) - 1) * percentile)
        return ordered[index]

    def action_summary(self) -> dict[str, dict[str, float | int]]:
        return {
            action: {
                "count": len(values),
                "p50_ms": round(self.percentile(values, 0.50), 3),
                "p95_ms": round(self.percentile(values, 0.95), 3),
                "p99_ms": round(self.percentile(values, 0.99), 3),
                "max_ms": round(max(values), 3),
            }
            for action, values in sorted(self.action_latencies_ms.items())
        }

    def measurement_summary(
        self, samples: dict[str, list[int]],
    ) -> dict[str, dict[str, float | int]]:
        return {
            name: {
                "p50": round(self.percentile(values, 0.50), 3),
                "p95": round(self.percentile(values, 0.95), 3),
                "p99": round(self.percentile(values, 0.99), 3),
                "max": max(values),
            }
            for name, values in sorted(samples.items())
        }

    def fork_summary(self) -> dict[str, object]:
        counts = [len(values) for values in self.fork_metric_samples.values()]
        return {
            "count": min(counts) if counts else 0,
            "paths": dict(sorted(self.fork_paths.items())),
            "terminations": dict(sorted(self.fork_terminations.items())),
            "measurements": self.measurement_summary(self.fork_metric_samples),
            "by_source_vcpus": {
                str(vcpus): {
                    "count": min(len(values) for values in samples.values()),
                    "measurements": self.measurement_summary(samples),
                }
                for vcpus, samples in sorted(
                    self.fork_metric_samples_by_vcpu.items()
                )
            },
        }

    def record_fork_metrics(
        self, response: dict[str, object], source_vcpus: int,
    ) -> None:
        assert source_vcpus > 0, source_vcpus
        metrics = response.get("metrics")
        assert isinstance(metrics, dict), response
        path = metrics.get("path")
        assert path in {"local", "cross_node"}, metrics
        phase_names = (
            "source_resolution_us",
            "operation_claim_us",
            "snapshot_artifact_us",
            "child_ready_us",
            "operation_commit_us",
        )
        for name in phase_names:
            value = metrics.get(name)
            assert isinstance(value, int) and value > 0, metrics
        total_us = metrics.get("total_us")
        assert isinstance(total_us, int) and total_us >= sum(
            metrics[name] for name in phase_names
        ), metrics
        live = metrics.get("live_snapshot")
        assert isinstance(live, dict), metrics
        for name in ("rounds", "pages_copied", "elapsed_us", "downtime_us"):
            value = live.get(name)
            assert isinstance(value, int) and value > 0, live
        final_dirty_pages = live.get("final_dirty_pages")
        assert isinstance(final_dirty_pages, int) and final_dirty_pages >= 0, live
        assert live["downtime_us"] <= live["elapsed_us"], live
        termination = live.get("termination")
        assert termination in {"converged", "diverging", "timeout", "max_rounds"}, live

        values = {
            **{name: metrics[name] for name in (*phase_names, "total_us")},
            **{
                "live_rounds": live["rounds"],
                "live_pages_copied": live["pages_copied"],
                "live_final_dirty_pages": final_dirty_pages,
                "live_elapsed_us": live["elapsed_us"],
                "live_downtime_us": live["downtime_us"],
            },
        }
        for name, value in values.items():
            self.fork_metric_samples.setdefault(name, []).append(value)
            per_vcpu = self.fork_metric_samples_by_vcpu.setdefault(source_vcpus, {})
            per_vcpu.setdefault(name, []).append(value)
        self.fork_paths[path] = self.fork_paths.get(path, 0) + 1
        self.fork_terminations[termination] = self.fork_terminations.get(termination, 0) + 1

    def record_vm_fork_metrics(self, vm_id: str, response: dict[str, object]) -> None:
        self.record_fork_metrics(response, self.anchors[vm_id].vcpus)

    def write_status(self, kind: str, fields: dict[str, object]) -> None:
        if not self.args.status_file:
            return
        status_path = os.path.abspath(self.args.status_file)
        status_dir = os.path.dirname(status_path)
        os.makedirs(status_dir, mode=0o700, exist_ok=True)
        payload = {
            "schema_version": 1,
            "state": "failed" if kind == "soak_failure" else (
                "passed" if kind == "soak_pass" else "running"
            ),
            "latest_event": kind,
            "updated_at_unix": int(time.time()),
            "elapsed_s": round(time.monotonic() - self.started_at, 3),
            "seed": self.args.seed,
            "case": self.args.case_name,
            "epoch": self.args.epoch,
            "operations": self.operations,
            "snapshots": self.snapshots,
            "anchors": len(self.anchors),
            "anchor_vcpus": sum(anchor.vcpus for anchor in self.anchors.values()),
            "host_logical_cpus": os.cpu_count() or 0,
            "transient_vms": len(self.transient),
            "hibernated_sentinel": self.sentinel.vm_id if self.sentinel else None,
            "actions": self.action_summary(),
            "forks": self.fork_summary(),
            **fields,
        }
        descriptor, temporary = tempfile.mkstemp(
            prefix=".status.", suffix=".tmp", dir=status_dir,
        )
        try:
            os.fchmod(descriptor, 0o600)
            with os.fdopen(descriptor, "w", encoding="utf-8") as output:
                json.dump(payload, output, sort_keys=True)
                output.write("\n")
                output.flush()
                os.fsync(output.fileno())
            os.replace(temporary, status_path)
        except BaseException:
            try:
                os.close(descriptor)
            except OSError:
                pass
            try:
                os.unlink(temporary)
            except FileNotFoundError:
                pass
            raise

    def event(self, kind: str, **fields: object) -> None:
        self.write_status(kind, fields)
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
        with self.operations_lock:
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

    def assert_guest_identity(self, vm_id: str) -> None:
        if not self.args.expected_kernel_prefix and not self.args.expected_os_id:
            return
        output = self.exec(
            vm_id,
            "set -eu; uname -r; . /etc/os-release; printf '%s\\n' \"$ID\"",
        ).splitlines()
        assert len(output) == 2, output
        kernel_release, os_id = output
        if self.args.expected_kernel_prefix:
            assert kernel_release.startswith(self.args.expected_kernel_prefix), (
                self.args.expected_kernel_prefix, kernel_release,
            )
        if self.args.expected_os_id:
            assert os_id == self.args.expected_os_id, (
                self.args.expected_os_id, os_id,
            )
        self.event(
            "guest_identity_verified", vm_id=vm_id,
            kernel_release=kernel_release, os_id=os_id,
        )

    def create_anchor(self, index: int) -> str:
        proof = f"anchor-{self.args.seed}-{index}-{uuid.uuid4().hex[:12]}"
        vcpus = self.args.anchor_vcpus[index % len(self.args.anchor_vcpus)]
        _, row = self.request(
            "POST", "/v1/vms", {"vcpus": vcpus, "memory_mib": 256}, 201
        )
        vm_id = row["id"]
        assert row["status"] == "running", row
        self.assert_guest_identity(vm_id)
        self.install_workload(vm_id, proof)
        self.anchors[vm_id] = Anchor(
            proof=proof, created_at=time.monotonic(), vcpus=vcpus
        )
        self.event("anchor_created", vm_id=vm_id, proof=proof, vcpus=vcpus)
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
        self.record_vm_fork_metrics(vm_id, response)
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
        self.exec(
            child_id,
            shell_write_command("/root/tarit-soak-proof", child_proof),
        )
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

    def start_epoch_hibernation(self) -> None:
        assert self.sentinel is None
        proof = f"sentinel-{self.args.seed}-{uuid.uuid4().hex[:12]}"
        _, row = self.request(
            "POST", "/v1/vms", {"vcpus": 1, "memory_mib": 256}, 201
        )
        vm_id = row["id"]
        assert row["status"] == "running", row
        self.install_workload(vm_id, proof)
        before = self.workload_state(vm_id)
        ticket = self.exec(vm_id, "/usr/local/bin/tarit-clone-repair-workload ticket")
        token = uuid.uuid4().hex
        monotonic_marker = f"/tmp/tarit-epoch-monotonic-{token}"
        realtime_marker = f"/tmp/tarit-epoch-realtime-{token}"
        timer_seconds = self.args.guest_timer_seconds
        armed = self.exec(
            vm_id,
            "set -eu; "
            f"rm -f {monotonic_marker} {realtime_marker}; "
            "read uptime rest < /proc/uptime; "
            f"busybox setsid sh -c 'sleep {timer_seconds}; printf fired > "
            f"{monotonic_marker}' </dev/null >/tmp/tarit-epoch-monotonic.log 2>&1 & "
            f"deadline=$(($(date +%s) + {timer_seconds})); "
            "busybox setsid sh -c '/usr/local/bin/tarit-clone-repair-workload "
            f"wait-realtime \"$1\" && printf fired > {realtime_marker}' "
            "sh \"$deadline\" </dev/null >/tmp/tarit-epoch-realtime.log 2>&1 & "
            "printf '%s %s\n' \"$uptime\" \"$(date +%s)\"",
        ).split()
        assert len(armed) == 2, armed
        _, hibernated = self.request(
            "POST", f"/v1/vms/{vm_id}/hibernate", {}, 200, 360
        )
        assert hibernated["status"] == "hibernated", hibernated
        self.sentinel = HibernationSentinel(
            vm_id=vm_id,
            proof=proof,
            before=before,
            ticket=ticket,
            uptime_before=float(armed[0]),
            realtime_before=int(armed[1]),
            started_at=time.monotonic(),
            monotonic_marker=monotonic_marker,
            realtime_marker=realtime_marker,
        )
        self.event("epoch_hibernation_started", vm_id=vm_id, proof=proof)

    def finish_epoch_hibernation(self) -> None:
        sentinel = self.sentinel
        assert sentinel is not None
        hold_seconds = time.monotonic() - sentinel.started_at
        assert hold_seconds >= self.args.epoch_hibernate_min_seconds, (
            hold_seconds, self.args.epoch_hibernate_min_seconds,
        )
        _, row = self.request("GET", f"/v1/vms/{sentinel.vm_id}")
        assert row["status"] == "hibernated", row
        resume_started = time.monotonic()
        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
            futures = [
                pool.submit(self.exec, sentinel.vm_id, f"echo epoch-wake-{index}")
                for index in range(4)
            ]
            assert [future.result(timeout=360) for future in futures] == [
                f"epoch-wake-{index}" for index in range(4)
            ]
        resume_seconds = time.monotonic() - resume_started
        after = self.workload_state(sentinel.vm_id)
        for field in ("clone", "prng", "ticket", "prefix"):
            assert after[field] != sentinel.before[field], (
                field, sentinel.before, after,
            )
        assert after["counter"] == "0" and after["cache"] == "-", after
        assert self.exec(
            sentinel.vm_id,
            f"/usr/local/bin/tarit-clone-repair-workload accept-ticket '{sentinel.ticket}'",
        ) == "rejected"
        immediate = self.exec(
            sentinel.vm_id,
            "set -eu; "
            f"test \"$(cat /root/tarit-soak-proof)\" = {sentinel.proof}; "
            "read uptime rest < /proc/uptime; "
            f"if test -e {sentinel.monotonic_marker}; then mono=fired; else mono=pending; fi; "
            "! dmesg | tail -n 300 | grep -Eiq "
            "'watchdog: BUG|soft lockup|hard LOCKUP|Kernel panic|BUG:'; "
            "printf '%s %s %s\n' \"$uptime\" \"$(date +%s)\" \"$mono\"",
        ).split()
        assert len(immediate) == 3 and immediate[2] == "pending", immediate
        uptime_after, realtime_after = float(immediate[0]), int(immediate[1])
        host_realtime = int(time.time())
        assert uptime_after >= sentinel.uptime_before, (
            sentinel.uptime_before, uptime_after,
        )
        assert uptime_after - sentinel.uptime_before < self.args.guest_timer_seconds, (
            sentinel.uptime_before, uptime_after,
        )
        assert abs(realtime_after - host_realtime) <= 3, (
            realtime_after, host_realtime,
        )
        assert realtime_after - sentinel.realtime_before >= hold_seconds - 3, (
            sentinel.realtime_before, realtime_after, hold_seconds,
        )

        realtime_deadline = time.monotonic() + 2
        while time.monotonic() < realtime_deadline:
            if self.exec(
                sentinel.vm_id,
                f"if test -e {sentinel.realtime_marker}; then echo fired; else echo pending; fi",
            ) == "fired":
                break
            time.sleep(0.05)
        else:
            raise AssertionError("epoch sentinel realtime timer did not fire")
        monotonic_deadline = time.monotonic() + self.args.guest_timer_seconds + 15
        while time.monotonic() < monotonic_deadline:
            if self.exec(
                sentinel.vm_id,
                f"if test -e {sentinel.monotonic_marker}; then echo fired; else echo pending; fi",
            ) == "fired":
                break
            time.sleep(0.25)
        else:
            raise AssertionError("epoch sentinel monotonic timer did not fire")
        monotonic_seconds = time.monotonic() - resume_started
        assert monotonic_seconds >= self.args.guest_timer_seconds - 1, monotonic_seconds
        self.request("DELETE", f"/v1/vms/{sentinel.vm_id}", expected=204, timeout=360)
        self.sentinel = None
        self.event(
            "epoch_hibernation_verified",
            vm_id=sentinel.vm_id,
            hold_seconds=round(hold_seconds, 3),
            resume_seconds=round(resume_seconds, 3),
            guest_uptime_delta_seconds=round(uptime_after - sentinel.uptime_before, 3),
            guest_realtime_delta_seconds=realtime_after - sentinel.realtime_before,
            host_realtime_error_seconds=realtime_after - host_realtime,
            monotonic_timer_after_resume_seconds=round(monotonic_seconds, 3),
        )

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

    def sibling_fork_timers(self, vm_id: str) -> None:
        timer_seconds = self.args.sibling_fork_timer_seconds
        token = uuid.uuid4().hex
        monotonic_marker = f"/tmp/tarit-sibling-monotonic-{token}"
        realtime_marker = f"/tmp/tarit-sibling-realtime-{token}"
        monotonic_pidfile = f"{monotonic_marker}.pid"
        realtime_pidfile = f"{realtime_marker}.pid"
        before_workload = self.workload_state(vm_id)
        before = self.exec(
            vm_id,
            f"set -eu; rm -f {monotonic_marker} {realtime_marker} "
            f"{monotonic_pidfile} {realtime_pidfile}; "
            "read uptime rest < /proc/uptime; "
            f"busybox setsid sh -c 'sleep {timer_seconds}; printf fired > "
            f"{monotonic_marker}' </dev/null >/tmp/tarit-sibling-monotonic.log 2>&1 & "
            f"printf '%s\n' \"$!\" > {monotonic_pidfile}; "
            f"deadline=$(($(date +%s) + {timer_seconds})); "
            "busybox setsid sh -c '/usr/local/bin/tarit-clone-repair-workload "
            f"wait-realtime \"$1\" && printf fired > {realtime_marker}' "
            "sh \"$deadline\" </dev/null >/tmp/tarit-sibling-realtime.log 2>&1 & "
            f"printf '%s\n' \"$!\" > {realtime_pidfile}; "
            "printf '%s %s %s\n' \"$uptime\" \"$(date +%s)\" \"$deadline\"",
        ).split()
        assert len(before) == 3, before
        uptime_before, realtime_before, realtime_deadline = (
            float(before[0]), int(before[1]), int(before[2])
        )
        started = time.monotonic()

        children: list[str] = []
        fork_ready: dict[str, float] = {}
        for _ in range(2):
            _, response = self.request("POST", f"/v1/vms/{vm_id}/fork", {}, 201, 360)
            self.record_vm_fork_metrics(vm_id, response)
            child = response["vm"]
            child_id = child["id"]
            assert child["status"] == "running", child
            self.transient.add(child_id)
            children.append(child_id)
            fork_ready[child_id] = time.monotonic() - started

        after_source = self.workload_state(vm_id)
        for field in ("clone", "prng", "ticket", "prefix", "counter", "cache"):
            assert after_source[field] == before_workload[field], (
                field, before_workload, after_source,
            )
        child_states = [self.workload_state(child_id) for child_id in children]
        for child_state in child_states:
            for field in ("clone", "prng", "ticket", "prefix"):
                assert child_state[field] != before_workload[field], (
                    field, before_workload, child_state,
                )
            assert child_state["counter"] == "0" and child_state["cache"] == "-", child_state
        for field in ("clone", "prng", "ticket", "prefix"):
            assert child_states[0][field] != child_states[1][field], (
                field, child_states,
            )

        vm_ids = [vm_id, *children]

        def read_timer_state(timer_vm_id: str) -> dict[str, str]:
            return self.parse_state(
                self.exec(
                    timer_vm_id,
                    "set -eu; read uptime rest < /proc/uptime; "
                    f"if test -e {monotonic_marker}; then mono=$(cat {monotonic_marker}); "
                    "else mono=pending; fi; "
                    f"if test -e {realtime_marker}; then real=$(cat {realtime_marker}); "
                    "else real=pending; fi; "
                    "printf 'mono=%s real=%s uptime=%s realtime=%s\\n' "
                    "\"$mono\" \"$real\" \"$uptime\" \"$(date +%s)\"",
                )
            )

        def timer_process_state(timer_vm_id: str) -> str:
            return self.exec(
                timer_vm_id,
                "set -u; "
                f"for spec in monotonic:{monotonic_pidfile} "
                f"realtime:{realtime_pidfile}; do "
                "name=${spec%%:*}; pidfile=${spec#*:}; "
                "if test ! -s \"$pidfile\"; then "
                "printf '%s pidfile=missing\\n' \"$name\"; continue; fi; "
                "pid=$(cat \"$pidfile\"); "
                "if test -r \"/proc/$pid/stat\"; then "
                "state=$(awk '{print $3}' \"/proc/$pid/stat\"); "
                "wchan=$(cat \"/proc/$pid/wchan\" 2>/dev/null || printf unknown); "
                "printf '%s pid=%s alive=yes state=%s wchan=%s\\n' "
                "\"$name\" \"$pid\" \"$state\" \"$wchan\"; "
                "else printf '%s pid=%s alive=no\\n' \"$name\" \"$pid\"; fi; done; "
                "printf '%s\\n' monotonic-log-begin; "
                "tail -n 20 /tmp/tarit-sibling-monotonic.log 2>/dev/null || true; "
                "printf '%s\\n' realtime-log-begin; "
                "tail -n 20 /tmp/tarit-sibling-realtime.log 2>/dev/null || true",
            )

        for timer_vm_id in vm_ids:
            state = read_timer_state(timer_vm_id)
            assert state["mono"] == "pending" and state["real"] == "pending", (
                timer_vm_id, state, fork_ready,
            )
            assert abs(int(state["realtime"]) - int(time.time())) <= 3, state
            assert float(state["uptime"]) >= uptime_before, state
            processes = timer_process_state(timer_vm_id)
            assert processes.count("alive=yes") == 2, (timer_vm_id, processes)

        sleep_until = started + timer_seconds - 2
        if sleep_until > time.monotonic():
            time.sleep(sleep_until - time.monotonic())
        fired: dict[str, dict[str, float]] = {timer_vm_id: {} for timer_vm_id in vm_ids}
        delivery_deadline = started + timer_seconds + max(fork_ready.values()) + 15
        while time.monotonic() < delivery_deadline:
            for timer_vm_id in vm_ids:
                state = read_timer_state(timer_vm_id)
                elapsed = time.monotonic() - started
                if state["real"] == "fired" and "realtime" not in fired[timer_vm_id]:
                    fired[timer_vm_id]["realtime"] = elapsed
                if state["mono"] == "fired" and "monotonic" not in fired[timer_vm_id]:
                    fired[timer_vm_id]["monotonic"] = elapsed
            if all(len(value) == 2 for value in fired.values()):
                break
            time.sleep(0.1)
        if not all(len(value) == 2 for value in fired.values()):
            diagnostics = {
                timer_vm_id: {
                    "timers": fired[timer_vm_id],
                    "state": read_timer_state(timer_vm_id),
                    "processes": timer_process_state(timer_vm_id),
                }
                for timer_vm_id in vm_ids
            }
            self.event(
                "sibling_fork_timer_failure",
                source=vm_id,
                children=children,
                fork_ready_seconds={
                    key: round(value, 3) for key, value in fork_ready.items()
                },
                diagnostics=diagnostics,
            )
            raise AssertionError(diagnostics)

        for timer_vm_id in vm_ids:
            assert fired[timer_vm_id]["realtime"] >= timer_seconds - 2, fired
            assert fired[timer_vm_id]["realtime"] <= max(
                timer_seconds + 3, fork_ready.get(timer_vm_id, 0) + 3,
            ), fired
            assert fired[timer_vm_id]["monotonic"] >= timer_seconds - 1, fired
            assert fired[timer_vm_id]["monotonic"] <= (
                timer_seconds + fork_ready.get(timer_vm_id, 0) + 5
            ), fired

        self.exec(
            children[0],
            f"printf child-one > {monotonic_marker}; printf child-one > {realtime_marker}",
        )
        for timer_vm_id in (vm_id, children[1]):
            assert self.exec(
                timer_vm_id,
                f"printf '%s %s\n' \"$(cat {monotonic_marker})\" "
                f"\"$(cat {realtime_marker})\"",
            ) == "fired fired"

        for child_id in children:
            self.delete_vm(child_id)
        self.event(
            "sibling_fork_timers_verified",
            source=vm_id,
            children=children,
            timer_seconds=timer_seconds,
            source_realtime_before=realtime_before,
            realtime_deadline=realtime_deadline,
            fork_ready_seconds={key: round(value, 3) for key, value in fork_ready.items()},
            delivery_seconds={
                key: {timer: round(value, 3) for timer, value in timers.items()}
                for key, timers in fired.items()
            },
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
        self.exec(
            vm_id,
            shell_write_command("/root/tarit-soak-proof", proof),
        )
        anchor.proof = proof
        self.assert_anchor(vm_id)
        self.event("guest_work_verified", vm_id=vm_id, proof=proof)

    def concurrent_guest_work(self, _vm_id: str) -> None:
        token = uuid.uuid4().hex

        def work(vm_id: str) -> tuple[str, str]:
            output = self.exec(
                vm_id,
                "set -eu; before=$(cat /tmp/soak-counter); "
                f"printf '%s' {token} > /tmp/tarit-concurrent-work.next; "
                "busybox dd if=/dev/urandom of=/tmp/tarit-concurrent-io.next "
                "bs=4096 count=64 2>/dev/null; "
                "test -s /tmp/tarit-concurrent-io.next; "
                "mv /tmp/tarit-concurrent-work.next /tmp/tarit-concurrent-work; "
                "mv /tmp/tarit-concurrent-io.next /tmp/tarit-concurrent-io; "
                "sync; after=$(cat /tmp/soak-counter); "
                "test \"$after\" -ge \"$before\"; "
                "printf '%s %s\n' \"$(cat /tmp/tarit-concurrent-work)\" \"$after\"",
            ).split()
            assert len(output) == 2 and output[0] == token, (vm_id, output)
            return vm_id, output[1]

        with concurrent.futures.ThreadPoolExecutor(max_workers=len(self.anchors)) as pool:
            results = list(pool.map(work, sorted(self.anchors)))
        assert len(results) == len(self.anchors), results
        for vm_id in self.anchors:
            self.assert_anchor(vm_id)
        self.event(
            "concurrent_guest_work_verified",
            token=token,
            counters={vm_id: counter for vm_id, counter in results},
        )

    def contended_guest_agent_exec(self, vm_id: str) -> None:
        # Exercise the same VM through simultaneous API calls immediately after
        # a lifecycle transition. This is the reconnect window where vsock and
        # UART fallback previously admitted overlapping requests and corrupted
        # the guest shell stream.
        _, paused = self.request("POST", f"/v1/vms/{vm_id}/pause", {}, 200)
        assert paused["status"] == "paused", paused
        _, running = self.request("POST", f"/v1/vms/{vm_id}/resume", {}, 200)
        assert running["status"] == "running", running
        tokens = [uuid.uuid4().hex for _ in range(4)]

        def work(index_and_token: tuple[int, str]) -> str:
            index, token = index_and_token
            path = f"/tmp/tarit-contended-exec-{index}"
            return self.exec(
                vm_id,
                "set -eu; "
                f"printf '%s\\n' {token} > {path}; "
                "sleep 1; "
                f"test \"$(cat {path})\" = {token}; "
                f"printf '%s\\n' {token}",
            )

        with concurrent.futures.ThreadPoolExecutor(max_workers=len(tokens)) as pool:
            outputs = list(pool.map(work, enumerate(tokens)))
        assert outputs == tokens, (tokens, outputs)
        self.assert_anchor(vm_id)
        self.event(
            "contended_guest_agent_exec_verified",
            vm_id=vm_id,
            requests=len(tokens),
        )

    def assert_global_invariants(self) -> None:
        _, rows = self.request("GET", "/v1/vms")
        by_id = {row["id"]: row for row in rows}
        for vm_id in self.anchors:
            assert vm_id in by_id and by_id[vm_id]["status"] == "running", (vm_id, by_id)
        if self.sentinel:
            assert by_id.get(self.sentinel.vm_id, {}).get("status") == "hibernated", (
                self.sentinel, by_id,
            )
        with sqlite3.connect(self.args.database) as database:
            resident = database.execute(
                "select id,pid from vms where status in ('running','paused','suspended')"
            ).fetchall()
            snapshots = database.execute(
                "select path,overlay_path,ephemeral_owner_vm_id from snapshots"
            ).fetchall()
            hibernated = {
                row[0] for row in database.execute("select vm_id from hibernations")
            }
        expected = set(self.anchors) | self.transient
        assert {vm_id for vm_id, _ in resident} == expected, (resident, expected)
        for vm_id, pid in resident:
            assert pid and os.path.exists(f"/proc/{pid}"), (vm_id, pid)
        expected_hibernated = {self.sentinel.vm_id} if self.sentinel else set()
        assert hibernated == expected_hibernated, (hibernated, expected_hibernated)
        ephemeral_owners = {owner for _, _, owner in snapshots if owner is not None}
        assert ephemeral_owners == expected_hibernated, (
            ephemeral_owners, expected_hibernated,
        )
        assert len(snapshots) == self.snapshots + len(expected_hibernated), (
            snapshots, self.snapshots, expected_hibernated,
        )
        expected_snapshot_files = {
            path
            for snapshot_path, overlay_path, _ in snapshots
            for path in (snapshot_path, overlay_path, f"{snapshot_path}.integrity")
            if path
        }
        snapshot_dir = os.path.join(os.path.dirname(self.args.database), "sockets", "snapshots")
        actual_snapshot_files = {
            entry.path
            for entry in os.scandir(snapshot_dir)
            if entry.is_file(follow_symlinks=False)
        }
        assert actual_snapshot_files == expected_snapshot_files, (
            actual_snapshot_files - expected_snapshot_files,
            expected_snapshot_files - actual_snapshot_files,
        )

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
        sentinel_ids = [self.sentinel.vm_id] if self.sentinel else []
        for vm_id in list(self.transient) + sentinel_ids + list(self.anchors):
            try:
                self.request("DELETE", f"/v1/vms/{vm_id}", expected={204, 404}, timeout=120)
            except Exception as error:
                self.event("cleanup_error", vm_id=vm_id, error=str(error))
        self.transient.clear()
        self.anchors.clear()
        self.sentinel = None

    def run_action(self, action, vm_id: str) -> None:
        self.assert_storage_headroom()
        started = time.monotonic()
        action(vm_id)
        self.assert_global_invariants()
        duration_ms = (time.monotonic() - started) * 1000
        self.action_latencies_ms.setdefault(action.__name__, []).append(duration_ms)
        self.event(
            "operation_complete", action=action.__name__, vm_id=vm_id,
            duration_ms=round(duration_ms, 3), operations=self.operations,
            snapshots=self.snapshots,
        )

    def run(self) -> None:
        self.assert_storage_headroom()
        for index in range(self.args.anchors):
            self.create_anchor(index)
            self.assert_storage_headroom()
        if self.args.epoch_hibernate_min_seconds:
            self.start_epoch_hibernation()
            self.assert_global_invariants()
        if self.args.hibernate_hold_seconds:
            self.long_hibernate_clock_timer(next(iter(self.anchors)))
            self.assert_global_invariants()
        if self.args.sibling_fork_timer_seconds:
            self.sibling_fork_timers(next(iter(self.anchors)))
            self.assert_global_invariants()
        anchor_ids = list(self.anchors)
        required_actions = [
            (self.fork_anchor, anchor_ids[0]),
            (self.snapshot_restore, anchor_ids[1]),
            (self.concurrent_guest_work, anchor_ids[2 % len(anchor_ids)]),
            (self.contended_guest_agent_exec, anchor_ids[0]),
        ]
        for action, vm_id in required_actions:
            self.run_action(action, vm_id)
        action_by_name = {
            "assert": self.assert_anchor,
            "mutate": self.mutate,
            "fork": self.fork_anchor,
            "hibernate": self.hibernate_resume,
            "pause": self.pause_resume,
            "balloon": self.balloon,
            "guest-work": self.concurrent_guest_work,
            "contended-exec": self.contended_guest_agent_exec,
            "snapshot": self.snapshot_restore,
        }
        if self.args.actions:
            actions = [action_by_name[name] for name in self.args.actions]
        else:
            actions = [
                self.assert_anchor, self.assert_anchor, self.mutate, self.fork_anchor,
                self.hibernate_resume, self.pause_resume, self.balloon,
                self.concurrent_guest_work, self.contended_guest_agent_exec,
            ]
        deadline = (
            time.monotonic() + self.args.duration_seconds
            if self.args.duration_seconds is not None else None
        )
        completed_steps = 0
        while (
            time.monotonic() < deadline
            if deadline is not None else completed_steps < self.args.steps
        ):
            vm_id = self.random.choice(list(self.anchors))
            available_actions = list(actions)
            if not self.args.actions and self.snapshots < self.args.max_snapshots:
                available_actions.append(self.snapshot_restore)
            action = self.random.choice(available_actions)
            self.run_action(action, vm_id)
            completed_steps += 1
            time.sleep(self.args.interval_seconds)
        if self.sentinel:
            self.finish_epoch_hibernation()
            self.assert_global_invariants()
        minimum_age = min(time.monotonic() - anchor.created_at for anchor in self.anchors.values())
        self.event(
            "soak_pass", seed=self.args.seed, operations=self.operations,
            snapshots=self.snapshots, anchors=len(self.anchors),
            minimum_anchor_age_s=round(minimum_age, 3),
            mode="duration" if deadline is not None else "steps",
            completed_steps=completed_steps,
            actions=self.action_summary(),
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
    parser.add_argument(
        "--seeds", default="7",
        help="comma-separated reproducible seeds; duration mode accepts one",
    )
    parser.add_argument(
        "--steps", type=int, default=20,
        help="randomized actions per seed when duration mode is not selected",
    )
    parser.add_argument(
        "--duration-seconds", type=int,
        help="run continuously for this duration instead of using --steps",
    )
    parser.add_argument("--interval-seconds", type=float, default=1.0)
    parser.add_argument("--anchors", type=int, default=3)
    parser.add_argument("--anchor-vcpus", default="1,2,4")
    parser.add_argument(
        "--actions",
        help=(
            "comma-separated loop actions: assert,mutate,fork,hibernate,pause,"
            "balloon,guest-work,contended-exec,snapshot"
        ),
    )
    parser.add_argument("--hibernate-hold-seconds", type=int, default=0)
    parser.add_argument("--guest-timer-seconds", type=int, default=5)
    parser.add_argument("--sibling-fork-timer-seconds", type=int, default=0)
    parser.add_argument("--epoch-hibernate-min-seconds", type=int, default=0)
    parser.add_argument("--storage-path")
    parser.add_argument("--min-free-bytes", type=int, default=0)
    parser.add_argument("--expected-kernel-prefix")
    parser.add_argument("--expected-os-id")
    parser.add_argument(
        "--status-file", default=os.environ.get("TARIT_LIFECYCLE_STATUS_FILE"),
    )
    parser.add_argument(
        "--case-name", default=os.environ.get("TARIT_LIFECYCLE_CASE_NAME"),
    )
    parser.add_argument(
        "--epoch", type=int, default=int(os.environ.get("TARIT_LIFECYCLE_EPOCH", "0")),
    )
    args = parser.parse_args()
    seed_values = args.seeds.split(",")
    if any(
        not value or not value.isascii() or not value.isdecimal()
        for value in seed_values
    ):
        parser.error("seeds must contain 1 to 64 comma-separated unsigned 64-bit integers")
    args.seeds = [int(value) for value in seed_values]
    if (
        not args.seeds or len(args.seeds) > 64
        or any(value < 0 or value > (1 << 64) - 1 for value in args.seeds)
    ):
        parser.error("seeds must contain 1 to 64 comma-separated unsigned 64-bit integers")
    try:
        args.anchor_vcpus = [int(value) for value in args.anchor_vcpus.split(",")]
    except ValueError:
        parser.error("anchor vCPU counts must be comma-separated integers")
    if not args.anchor_vcpus or any(value < 1 or value > 8 for value in args.anchor_vcpus):
        parser.error("anchor vCPU counts must be between one and eight")
    allowed_actions = {
        "assert", "mutate", "fork", "hibernate", "pause", "balloon",
        "guest-work", "contended-exec", "snapshot",
    }
    if args.actions:
        action_values = args.actions.split(",")
        if any(not value.strip() for value in action_values):
            parser.error("actions must be a comma-separated list without empty entries")
        args.actions = [value.strip() for value in action_values]
        invalid_actions = sorted(set(args.actions) - allowed_actions)
        if not args.actions or invalid_actions:
            parser.error(f"unknown loop actions: {','.join(invalid_actions)}")
    else:
        args.actions = []
    if args.steps < 1 or args.steps > 10000:
        parser.error("steps must be between one and 10000")
    if not 0 <= args.interval_seconds <= 60:
        parser.error("action interval must be between zero and 60 seconds")
    if args.duration_seconds is not None and args.duration_seconds < 60:
        parser.error("duration must be at least 60 seconds")
    if args.duration_seconds is not None and len(args.seeds) != 1:
        parser.error("duration mode requires exactly one seed")
    if args.anchors < 2 or args.anchors > args.max_vms - 2:
        parser.error("anchors must leave two transient slots")
    if args.min_free_bytes < 0 or (args.min_free_bytes and not args.storage_path):
        parser.error("a non-negative storage floor requires --storage-path")
    if args.hibernate_hold_seconds and args.hibernate_hold_seconds <= args.guest_timer_seconds + 2:
        parser.error("hibernate hold must exceed the guest timer by at least three seconds")
    if args.guest_timer_seconds < 2:
        parser.error("guest timer must be at least two seconds")
    if args.sibling_fork_timer_seconds and args.sibling_fork_timer_seconds < 10:
        parser.error("sibling fork timer must be at least ten seconds")
    if args.epoch_hibernate_min_seconds < 0:
        parser.error("epoch hibernation minimum must not be negative")
    return args


def main() -> int:
    args = parse_args()
    for seed in args.seeds:
        run_args = argparse.Namespace(**vars(args))
        run_args.seed = seed
        soak = Soak(run_args)
        try:
            soak.run()
        except Exception as error:
            soak.event("soak_failure", error=repr(error), operations=soak.operations)
            raise
        finally:
            soak.cleanup()
    return 0


if __name__ == "__main__":
    sys.exit(main())
