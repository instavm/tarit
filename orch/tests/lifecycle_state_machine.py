#!/usr/bin/env python3
"""Model-based, real-KVM lifecycle test for the Tarit sandbox API."""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import os
import random
import re
import sqlite3
import urllib.error
import urllib.request
import uuid
from dataclasses import dataclass


PUBLIC_VM_FIELDS = {
    "id", "status", "revision", "startup_path", "memory_mib", "vcpus",
    "created_at", "updated_at",
}
PRIVATE_VM_FIELDS = {
    "host_id", "owner_key", "api_key_id", "kernel_path", "rootfs_path",
    "cmdline", "runtime_layout", "runtime_overlay_path", "runtime_jail_path",
    "runtime_artifact_paths", "socket_path", "pid",
}
SHA256 = re.compile(r"^sha256:[0-9a-f]{64}$")
RESIDENT = {"running", "paused", "suspended"}


@dataclass
class Vm:
    status: str
    proof: str
    balloon_target_mib: int = 0


class Gate:
    def __init__(self, base_url: str, api_key: str, database: str, vmm: str,
                 jail_uid_base: int, jail_uid_count: int, max_vms: int,
                 max_snapshots: int) -> None:
        self.base_url = base_url.rstrip("/")
        self.api_key = api_key
        self.database = database
        self.vmm = os.path.realpath(vmm)
        self.jail_uid_base = jail_uid_base
        self.jail_uid_count = jail_uid_count
        self.max_vms = max_vms
        self.max_snapshots = max_snapshots
        self.vms: dict[str, Vm] = {}
        self.deleted: set[str] = set()
        self.snapshots: dict[str, str] = {}
        self.snapshot_balloon_targets: dict[str, int] = {}
        self.seed_snapshots: dict[str, str] = {}
        self.seed_snapshot_balloon_targets: dict[str, int] = {}
        self.operations = 0

    def request(self, method: str, path: str, body: object | None = None,
                expected: int | set[int] = 200, timeout: int = 180):
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(
            self.base_url + path,
            data=data,
            method=method,
            headers={"X-API-Key": self.api_key, "Content-Type": "application/json"},
        )
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                status = response.status
                payload = response.read()
        except urllib.error.HTTPError as error:
            status = error.code
            payload = error.read()
        allowed = {expected} if isinstance(expected, int) else expected
        if status not in allowed:
            raise AssertionError(
                f"{method} {path}: expected HTTP {sorted(allowed)}, got {status}: "
                f"{payload.decode(errors='replace')}"
            )
        self.operations += 1
        if not payload:
            return status, None
        try:
            return status, json.loads(payload)
        except json.JSONDecodeError as error:
            raise AssertionError(f"{method} {path}: non-JSON response: {payload!r}") from error

    @staticmethod
    def check_public_vm(row: dict, expected_status: str | None = None) -> None:
        assert isinstance(row, dict), row
        assert not (set(row) & PRIVATE_VM_FIELDS), row
        assert set(row) <= PUBLIC_VM_FIELDS, row
        uuid.UUID(row["id"])
        assert row["revision"] >= 1, row
        assert row["memory_mib"] == 256 and row["vcpus"] == 1, row
        if expected_status is not None:
            assert row["status"] == expected_status, row

    def create(self, proof: str, vm_id: str | None = None) -> str:
        body = {"memory_mib": 256, "vcpus": 1}
        if vm_id is not None:
            body["id"] = vm_id
        status, row = self.request("POST", "/v1/vms", body, 201)
        assert status == 201
        self.check_public_vm(row, "running")
        vm_id = row["id"]
        assert vm_id not in self.vms
        self.vms[vm_id] = Vm("running", "")
        self.write_proof(vm_id, proof)
        return vm_id

    def exec(self, vm_id: str, command: str, expected_http: int = 200,
             expected_exit: int = 0):
        status, row = self.request(
            "POST", "/v1/execute",
            {"vm_id": vm_id, "command": command, "timeout_ms": 30000},
            expected_http,
        )
        if expected_http != 200:
            return row
        assert status == 200 and row["status"] == "completed", row
        assert row["exit_code"] == expected_exit and not row.get("error"), row
        return row

    def write_proof(self, vm_id: str, proof: str) -> None:
        assert re.fullmatch(r"[a-z0-9-]+", proof), proof
        row = self.exec(vm_id, f"printf %s {proof} > /root/tarit-state-proof; sync")
        assert row["exit_code"] == 0, row
        self.vms[vm_id].status = "running"
        self.vms[vm_id].proof = proof

    def assert_proof(self, vm_id: str, expected: str) -> None:
        row = self.exec(vm_id, "cat /root/tarit-state-proof")
        # MarkerV1 is line-framed while ChunkedV2 preserves an unterminated
        # final line. Accept only that protocol-level CR/LF suffix variance.
        assert (row.get("stdout") or "").rstrip("\r\n") == expected, (expected, row)
        self.vms[vm_id].status = "running"

    def transition(self, vm_id: str, action: str) -> int:
        before = self.vms[vm_id].status
        valid = {
            "pause": before in {"running", "paused"},
            "suspend": before in {"running", "paused", "suspended"},
            "resume": before in {"running", "paused", "suspended", "hibernated"},
            "hibernate": before == "running",
        }[action]
        expected = 200 if valid else 409
        status, row = self.request("POST", f"/v1/vms/{vm_id}/{action}", {}, expected)
        if valid:
            after = {
                "pause": "paused", "suspend": "suspended",
                "resume": "running", "hibernate": "hibernated",
            }[action]
            self.check_public_vm(row, after)
            self.vms[vm_id].status = after
        return status

    def snapshot(self, vm_id: str, diff: bool = False) -> str | None:
        current = self.vms[vm_id].status
        expected = 422 if diff else (200 if current in {"running", "paused"} else 409)
        _, row = self.request(
            "POST", f"/v1/vms/{vm_id}/snapshot", {"diff": diff}, expected
        )
        if expected != 200:
            return None
        assert set(row) == {"snapshot_id"}, row
        snapshot_id = str(uuid.UUID(row["snapshot_id"]))
        self.snapshots[snapshot_id] = self.vms[vm_id].proof
        self.snapshot_balloon_targets[snapshot_id] = self.vms[vm_id].balloon_target_mib
        return snapshot_id

    def balloon(self, vm_id: str, target_mib: int | None = None) -> int:
        current = self.vms[vm_id].status
        valid = current in {"running", "paused"}
        if target_mib is None:
            _, row = self.request(
                "GET", f"/v1/vms/{vm_id}/balloon", expected=200 if valid else 409
            )
        else:
            expected = 400 if target_mib > 256 else (200 if valid else 409)
            _, row = self.request(
                "PUT", f"/v1/vms/{vm_id}/balloon",
                {"target_mib": target_mib}, expected=expected,
            )
        if valid and target_mib is not None and target_mib <= 256:
            self.vms[vm_id].balloon_target_mib = target_mib
        if valid and (target_mib is None or target_mib <= 256):
            assert row["target_mib"] == self.vms[vm_id].balloon_target_mib, row
            assert row["target_pages"] == row["target_mib"] * 256, row
            assert 0 <= row["actual_pages"] <= 256 * 256, row
        return row.get("target_mib", -1) if isinstance(row, dict) else -1

    def fork(self, source_id: str) -> str | None:
        expected = 201 if self.vms[source_id].status == "running" else 409
        _, row = self.request("POST", f"/v1/vms/{source_id}/fork", {}, expected)
        if expected != 201:
            return None
        assert row["source_vm_id"] == source_id, row
        child = row["vm"]
        self.check_public_vm(child, "running")
        child_id = child["id"]
        self.vms[child_id] = Vm(
            "running", self.vms[source_id].proof,
            self.vms[source_id].balloon_target_mib,
        )
        self.assert_proof(child_id, self.vms[source_id].proof)
        return child_id

    def restore(self, snapshot_id: str) -> str:
        _, row = self.request("POST", "/v1/restore", {"snapshot_id": snapshot_id}, 201)
        self.check_public_vm(row, "running")
        vm_id = row["id"]
        self.vms[vm_id] = Vm(
            "running", self.snapshots[snapshot_id],
            self.snapshot_balloon_targets[snapshot_id],
        )
        self.assert_proof(vm_id, self.snapshots[snapshot_id])
        return vm_id

    def delete(self, vm_id: str) -> None:
        self.request("DELETE", f"/v1/vms/{vm_id}", expected=204)
        del self.vms[vm_id]
        self.deleted.add(vm_id)

    def repeat_delete(self, vm_id: str) -> None:
        self.request("DELETE", f"/v1/vms/{vm_id}", expected=404)

    def assert_deleted_terminal(self, vm_id: str) -> None:
        """A deleted identity must never be usable through another VM route."""
        assert vm_id in self.deleted and vm_id not in self.vms
        self.request("GET", f"/v1/vms/{vm_id}", expected=404)
        self.request("GET", f"/v1/vms/{vm_id}/status", expected=404)
        self.request("GET", f"/v1/vms/{vm_id}/balloon", expected=404)
        self.request("POST", f"/v1/vms/{vm_id}/pause", {}, 404)
        self.request("POST", f"/v1/vms/{vm_id}/resume", {}, 404)
        self.request("POST", f"/v1/vms/{vm_id}/suspend", {}, 404)
        self.request("POST", f"/v1/vms/{vm_id}/hibernate", {}, 404)
        self.request("POST", f"/v1/vms/{vm_id}/snapshot", {"diff": False}, 404)
        self.request("POST", f"/v1/vms/{vm_id}/fork", {}, 404)
        self.exec(vm_id, "true", expected_http=404)

    def concurrent_delete(self, vm_id: str) -> None:
        """A same-identity delete race has one winner and a terminal result."""
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            futures = [
                pool.submit(
                    self.request, "DELETE", f"/v1/vms/{vm_id}", None,
                    {204, 404}, 240,
                )
                for _ in range(2)
            ]
            results = [future.result(timeout=300) for future in futures]
        statuses = sorted(status for status, _ in results)
        assert statuses == [204, 404], (vm_id, results)
        del self.vms[vm_id]
        self.deleted.add(vm_id)
        self.assert_deleted_terminal(vm_id)

    def concurrent_resume(self, vm_id: str) -> None:
        """Concurrent wake requests must converge on one resident VMM."""
        assert self.vms[vm_id].status == "hibernated"
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            futures = [
                pool.submit(
                    self.request, "POST", f"/v1/vms/{vm_id}/resume", {}, 200, 300
                )
                for _ in range(2)
            ]
            results = [future.result(timeout=360) for future in futures]
        for status, row in results:
            assert status == 200
            self.check_public_vm(row, "running")
            assert row["id"] == vm_id, row
        self.vms[vm_id].status = "running"
        self.assert_proof(vm_id, self.vms[vm_id].proof)

    def concurrent_fork_fanout(self, source_id: str) -> list[str]:
        """Two live forks must have isolated children and preserve the source."""
        assert self.vms[source_id].status == "running"
        source_proof = self.vms[source_id].proof
        balloon_target = self.vms[source_id].balloon_target_mib
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            futures = [
                pool.submit(
                    self.request, "POST", f"/v1/vms/{source_id}/fork", {}, 201, 300
                )
                for _ in range(2)
            ]
            results = [future.result(timeout=360) for future in futures]
        child_ids: list[str] = []
        for status, row in results:
            assert status == 201 and row["source_vm_id"] == source_id, row
            child = row["vm"]
            self.check_public_vm(child, "running")
            child_id = child["id"]
            assert child_id != source_id and child_id not in child_ids, results
            child_ids.append(child_id)
            self.vms[child_id] = Vm("running", source_proof, balloon_target)
            self.assert_proof(child_id, source_proof)
        self.write_proof(child_ids[0], "fanout-child-a")
        self.write_proof(child_ids[1], "fanout-child-b")
        self.assert_proof(source_id, source_proof)
        self.assert_proof(child_ids[0], "fanout-child-a")
        self.assert_proof(child_ids[1], "fanout-child-b")
        return child_ids

    def concurrent_duplicate_create(self) -> str:
        """Exactly one caller may win a same-id create race."""
        vm_id = str(uuid.uuid4())
        body = {"id": vm_id, "memory_mib": 256, "vcpus": 1}
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            futures = [
                pool.submit(self.request, "POST", "/v1/vms", body, {201, 409})
                for _ in range(2)
            ]
            results = [future.result(timeout=240) for future in futures]
        statuses = sorted(status for status, _ in results)
        assert statuses == [201, 409], (vm_id, results)
        winner = next(row for status, row in results if status == 201)
        self.check_public_vm(winner, "running")
        assert winner["id"] == vm_id, winner
        self.vms[vm_id] = Vm("running", "")
        self.write_proof(vm_id, "duplicate-create-winner")
        return vm_id

    def concurrent_snapshot_delete(self, vm_id: str) -> str | None:
        """Delete must be terminal even when a snapshot races it."""
        proof = self.vms[vm_id].proof
        balloon_target = self.vms[vm_id].balloon_target_mib
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            snapshot_future = pool.submit(
                self.request, "POST", f"/v1/vms/{vm_id}/snapshot",
                {"diff": False}, {200, 404, 409}, 240,
            )
            delete_future = pool.submit(
                self.request, "DELETE", f"/v1/vms/{vm_id}", None, 204, 240,
            )
            snapshot_status, snapshot_row = snapshot_future.result(timeout=300)
            delete_status, _ = delete_future.result(timeout=300)
        assert delete_status == 204
        del self.vms[vm_id]
        self.deleted.add(vm_id)
        self.repeat_delete(vm_id)
        if snapshot_status != 200:
            return None
        assert set(snapshot_row) == {"snapshot_id"}, snapshot_row
        snapshot_id = str(uuid.UUID(snapshot_row["snapshot_id"]))
        self.snapshots[snapshot_id] = proof
        self.snapshot_balloon_targets[snapshot_id] = balloon_target
        return snapshot_id

    def database_rows(self, query: str, parameters=()):
        with sqlite3.connect(self.database, timeout=10) as database:
            database.row_factory = sqlite3.Row
            return [dict(row) for row in database.execute(query, parameters)]

    @staticmethod
    def process_exists(pid: int) -> bool:
        try:
            os.kill(pid, 0)
            return True
        except ProcessLookupError:
            return False

    def assert_invariants(self, label: str) -> None:
        _, listed = self.request("GET", "/v1/vms", expected=200)
        api = {row["id"]: row for row in listed}
        assert set(api) == set(self.vms), (label, api, self.vms)
        for vm_id, model in self.vms.items():
            self.check_public_vm(api[vm_id], model.status)
            _, fetched = self.request("GET", f"/v1/vms/{vm_id}", expected=200)
            self.check_public_vm(fetched, model.status)
            self.balloon(vm_id)

        rows = self.database_rows(
            "select id,status,pid,socket_path,runtime_overlay_path,runtime_jail_path "
            "from vms"
        )
        active = {row["id"]: row for row in rows if row["status"] != "stopped"}
        assert set(active) == set(self.vms), (label, active, self.vms)
        resident_pids: set[int] = set()
        for vm_id, model in self.vms.items():
            row = active[vm_id]
            assert row["status"] == model.status, (label, row, model)
            if model.status == "hibernated":
                assert all(row[field] is None for field in (
                    "pid", "socket_path", "runtime_overlay_path", "runtime_jail_path"
                )), (label, row)
                continue
            assert model.status in RESIDENT and row["pid"], (label, row)
            pid = int(row["pid"])
            assert pid not in resident_pids and self.process_exists(pid), (label, row)
            resident_pids.add(pid)
            assert os.path.realpath(f"/proc/{pid}/exe") == self.vmm, (label, pid)
            euid = os.stat(f"/proc/{pid}").st_uid
            assert self.jail_uid_base <= euid < self.jail_uid_base + self.jail_uid_count, (
                label, pid, euid
            )
            jail = row["runtime_jail_path"]
            assert jail and os.path.isdir(jail), (label, row)
            assert not os.path.exists(os.path.join(jail, "root", "dev", "kvm")), (label, jail)

        hibernated = {row["vm_id"] for row in self.database_rows("select vm_id from hibernations")}
        expected_hibernated = {vm_id for vm_id, vm in self.vms.items()
                               if vm.status == "hibernated"}
        assert hibernated == expected_hibernated, (label, hibernated, expected_hibernated)

        for row in self.database_rows(
            "select snapshot_id,owner_key,content_digest,size_bytes from snapshots"
        ):
            uuid.UUID(row["snapshot_id"])
            assert row["owner_key"] and row["content_digest"] and row["size_bytes"] > 0, row
        for row in self.database_rows(
            "select artifact_id,status,content_digest,size_bytes,replication_state,"
            "reference_count from artifacts"
        ):
            uuid.UUID(row["artifact_id"])
            assert row["status"] == "available", row
            assert SHA256.fullmatch(row["content_digest"]), row
            assert row["size_bytes"] >= 0 and row["reference_count"] >= 0, row
            assert row["replication_state"] in {"ready", "pending"}, row
        for row in self.database_rows(
            "select status,content_digest,size_bytes from artifact_replicas"
        ):
            assert row["status"] == "available" and SHA256.fullmatch(row["content_digest"]), row
            assert row["size_bytes"] >= 0, row

        proc_vmm: dict[int, int] = {}
        for entry in os.listdir("/proc"):
            if not entry.isdigit():
                continue
            try:
                if os.path.realpath(f"/proc/{entry}/exe") == self.vmm:
                    pid = int(entry)
                    with open(f"/proc/{entry}/status", encoding="utf-8") as status_file:
                        status_fields = dict(
                            line.split(":", 1) for line in status_file if ":" in line
                        )
                    proc_vmm[pid] = int(status_fields["PPid"].strip())
                    euid = int(status_fields["Uid"].split()[1])
                    assert self.jail_uid_base <= euid < self.jail_uid_base + self.jail_uid_count, (
                        label, pid, euid
                    )
            except (FileNotFoundError, PermissionError):
                pass
        assert resident_pids <= set(proc_vmm), (label, proc_vmm, resident_pids)
        for pid in proc_vmm:
            ancestor = pid
            visited = set()
            while ancestor not in resident_pids and ancestor in proc_vmm and ancestor not in visited:
                visited.add(ancestor)
                ancestor = proc_vmm[ancestor]
            assert ancestor in resident_pids, (label, pid, proc_vmm, resident_pids)

    def deterministic(self) -> None:
        print("== deterministic lifecycle transition table ==", flush=True)
        source = self.create("source-v1")
        assert self.balloon(source, 257) == -1
        assert self.balloon(source, 32) == 32
        self.assert_invariants("create")
        assert self.transition(source, "pause") == 200
        assert self.transition(source, "pause") == 200
        paused_snapshot = self.snapshot(source)
        assert paused_snapshot
        assert self.fork(source) is None
        assert self.transition(source, "suspend") == 200
        assert self.balloon(source) == -1
        assert self.transition(source, "suspend") == 200
        assert self.transition(source, "pause") == 409
        self.exec(source, "true", expected_http=409)
        assert self.transition(source, "resume") == 200
        assert self.transition(source, "resume") == 200
        child = self.fork(source)
        assert child
        assert self.balloon(child) == 32
        self.write_proof(source, "source-v2")
        self.write_proof(child, "child-v2")
        self.assert_proof(source, "source-v2")
        self.assert_proof(child, "child-v2")
        running_snapshot = self.snapshot(source)
        assert running_snapshot
        # Randomized seeds must start from an identical artifact set. Do not
        # let the optional winner of the later snapshot/delete race, or
        # snapshots created by an earlier seed, perturb a subsequent seed's
        # action space.
        self.seed_snapshots = {
            paused_snapshot: self.snapshots[paused_snapshot],
            running_snapshot: self.snapshots[running_snapshot],
        }
        self.seed_snapshot_balloon_targets = {
            paused_snapshot: self.snapshot_balloon_targets[paused_snapshot],
            running_snapshot: self.snapshot_balloon_targets[running_snapshot],
        }
        assert self.snapshot(source, diff=True) is None
        assert self.transition(source, "hibernate") == 200
        assert self.transition(source, "hibernate") == 409
        assert self.transition(source, "pause") == 409
        assert self.snapshot(source) is None
        self.assert_invariants("scale-to-zero")
        self.assert_proof(source, "source-v2")  # HTTP execute wakes the same id.
        assert self.balloon(source) == 32
        self.assert_invariants("http-wake")
        self.delete(child)
        self.repeat_delete(child)
        restored_paused = self.restore(paused_snapshot)
        restored_running = self.restore(running_snapshot)
        self.assert_proof(restored_paused, "source-v1")
        self.assert_proof(restored_running, "source-v2")
        # Keep this regression runnable at the minimum meaningful capacity
        # (three VMs: source plus two concurrent fork children). The original
        # source is no longer used until cleanup, so scale it to zero before
        # asking a restored VM to fork.
        assert self.transition(source, "hibernate") == 200
        # Regression: a balloon discard racing the UFFD handler used to
        # deadlock the bulk-copy phase when forking a lazily restored VM.
        assert self.balloon(restored_paused) == 32
        restored_fork = self.fork(restored_paused)
        assert restored_fork
        self.assert_proof(restored_fork, "source-v1")
        self.delete(restored_fork)
        self.assert_invariants("restore")
        # Regression: a restored VM uses a jailed lazy-CoW upper. It must be
        # snapshot-able again for scale-to-zero, then resume with its state.
        assert self.transition(restored_running, "hibernate") == 200
        self.assert_invariants("restored-vm-scale-to-zero")
        self.assert_proof(restored_running, "source-v2")
        assert self.transition(restored_paused, "hibernate") == 200
        self.delete(restored_paused)
        self.delete(restored_running)
        self.delete(source)
        self.assert_invariants("deterministic-clean")

        print("== concurrent create/snapshot/delete linearizability ==", flush=True)
        raced = self.concurrent_duplicate_create()
        raced_snapshot = self.concurrent_snapshot_delete(raced)
        self.assert_invariants("snapshot-delete-race")
        if raced_snapshot is not None:
            restored_race = self.restore(raced_snapshot)
            self.assert_proof(restored_race, "duplicate-create-winner")
            self.delete(restored_race)
        self.assert_invariants("concurrent-clean")

        print("== concurrent fork fan-out and terminal delete ==", flush=True)
        fanout_source = self.create("fanout-source")
        fanout_children = self.concurrent_fork_fanout(fanout_source)
        self.assert_invariants("concurrent-fork-fanout")
        self.concurrent_delete(fanout_children[0])
        self.delete(fanout_children[1])
        self.delete(fanout_source)
        self.assert_invariants("fanout-clean")

        print("== scale-to-zero capacity release and single-flight resume ==", flush=True)
        capacity_vms = [self.create(f"capacity-{index}") for index in range(self.max_vms)]
        hibernated = capacity_vms[0]
        assert self.transition(hibernated, "hibernate") == 200
        replacement = self.create("capacity-replacement")
        # All resident slots are full again. A resume must fail without changing
        # the logical hibernated state or leaking a second process.
        self.request("POST", f"/v1/vms/{hibernated}/resume", {}, 429)
        self.assert_invariants("resume-capacity-rejected")
        self.delete(replacement)
        self.concurrent_resume(hibernated)
        self.assert_invariants("resume-single-flight")
        for vm_id in list(self.vms):
            self.delete(vm_id)
        self.assert_invariants("capacity-clean")

    def randomized(self, seed: int, steps: int) -> None:
        print(f"== randomized state machine seed={seed} steps={steps} ==", flush=True)
        rng = random.Random(seed)
        for step in range(steps):
            choices = ["transition", "exec", "balloon", "delete"] if self.vms else []
            if self.vms and len(self.snapshots) < self.max_snapshots:
                choices.append("snapshot")
            if len(self.vms) < self.max_vms:
                choices.append("create")
                if self.snapshots:
                    choices.append("restore")
                if any(vm.status == "running" for vm in self.vms.values()):
                    choices.append("fork")
            action = rng.choice(choices or ["create"])
            print(
                f"seed={seed} step={step} action={action} "
                f"vms={{{', '.join(f'{vm_id[:8]}:{vm.status}' for vm_id, vm in self.vms.items())}}} "
                f"snapshots={len(self.snapshots)}",
                flush=True,
            )
            if action == "create":
                self.create(f"seed-{seed}-step-{step}")
            elif action == "restore" and self.snapshots:
                snapshot_id = rng.choice(list(self.snapshots))
                print(f"seed={seed} step={step} restore={snapshot_id}", flush=True)
                self.restore(snapshot_id)
            elif action == "fork":
                candidates = [vm_id for vm_id, vm in self.vms.items() if vm.status == "running"]
                if candidates:
                    source_id = rng.choice(candidates)
                    print(f"seed={seed} step={step} fork_source={source_id}", flush=True)
                    self.fork(source_id)
            else:
                vm_id = rng.choice(list(self.vms))
                if action == "transition":
                    transition = rng.choice(["pause", "suspend", "resume", "hibernate"])
                    print(
                        f"seed={seed} step={step} vm={vm_id} transition={transition}",
                        flush=True,
                    )
                    self.transition(vm_id, transition)
                elif action == "exec":
                    current = self.vms[vm_id].status
                    if current in {"running", "hibernated"}:
                        self.assert_proof(vm_id, self.vms[vm_id].proof)
                    else:
                        self.exec(vm_id, "true", expected_http=409)
                elif action == "snapshot":
                    self.snapshot(vm_id, diff=(rng.randrange(5) == 0))
                elif action == "balloon":
                    target_mib = rng.choice([0, 16, 32])
                    print(
                        f"seed={seed} step={step} vm={vm_id} "
                        f"balloon_target_mib={target_mib}",
                        flush=True,
                    )
                    self.balloon(vm_id, target_mib)
                elif action == "delete":
                    self.delete(vm_id)
            self.assert_invariants(f"seed={seed} step={step} action={action}")

        for vm_id in list(self.vms):
            self.delete(vm_id)
        self.assert_invariants(f"seed={seed} cleanup")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--api-key", required=True)
    parser.add_argument("--database", required=True)
    parser.add_argument("--vmm", required=True)
    parser.add_argument("--jail-uid-base", type=int, required=True)
    parser.add_argument("--jail-uid-count", type=int, required=True)
    parser.add_argument("--max-vms", type=int, default=4)
    parser.add_argument("--max-snapshots", type=int, default=8)
    parser.add_argument("--seeds", default="7,202609,424242")
    parser.add_argument("--steps", type=int, default=20)
    args = parser.parse_args()
    gate = Gate(args.base_url, args.api_key, args.database, args.vmm,
                args.jail_uid_base, args.jail_uid_count, args.max_vms,
                args.max_snapshots)
    gate.deterministic()
    for seed in (int(value) for value in args.seeds.split(",") if value):
        gate.snapshots = dict(gate.seed_snapshots)
        gate.snapshot_balloon_targets = dict(gate.seed_snapshot_balloon_targets)
        gate.randomized(seed, args.steps)
    print(
        f"LIFECYCLE_STATE_MACHINE_PASS operations={gate.operations} "
        f"seeds={args.seeds} steps_per_seed={args.steps}",
        flush=True,
    )


if __name__ == "__main__":
    main()
