#!/usr/bin/env python3
"""Measure and enforce the lazy live-fork path through the public HTTP API."""

from __future__ import annotations

import argparse
import json
import math
import os
import pathlib
import statistics
import tempfile
import time
import urllib.error
import urllib.request
import uuid


class GateFailure(RuntimeError):
    pass


def percentile(values: list[int], fraction: float) -> int:
    if not values:
        raise GateFailure("cannot summarize an empty sample set")
    ordered = sorted(values)
    return ordered[max(0, math.ceil(fraction * len(ordered)) - 1)]


def summary(values: list[int], expected: int) -> dict[str, int | float]:
    if len(values) != expected or any(value <= 0 for value in values):
        raise GateFailure(
            f"expected {expected} positive samples, received {len(values)}"
        )
    return {
        "p50": statistics.median(values),
        "p95": percentile(values, 0.95),
        "p99": percentile(values, 0.99),
        "max": max(values),
    }


def bounded_ratio(large: int | float, small: int | float) -> float:
    if small <= 0:
        raise GateFailure("small-case latency must be positive")
    return float(large) / float(small)


class Client:
    def __init__(self, base_url: str, api_key: str, timeout: float) -> None:
        self.base_url = base_url.rstrip("/")
        self.api_key = api_key
        self.timeout = timeout

    def request(
        self,
        method: str,
        path: str,
        body: dict[str, object] | None = None,
        expected: tuple[int, ...] = (200,),
    ) -> dict[str, object] | None:
        data = None if body is None else json.dumps(body).encode()
        headers = {"X-API-Key": self.api_key}
        if data is not None:
            headers["Content-Type"] = "application/json"
        request = urllib.request.Request(
            f"{self.base_url}{path}", data=data, headers=headers, method=method
        )
        try:
            with urllib.request.urlopen(request, timeout=self.timeout) as response:
                payload = response.read()
                status = response.status
        except urllib.error.HTTPError as error:
            detail = error.read().decode(errors="replace")
            raise GateFailure(
                f"{method} {path}: HTTP {error.code}: {detail}"
            ) from error
        except OSError as error:
            raise GateFailure(f"{method} {path}: {error}") from error
        if status not in expected:
            raise GateFailure(f"{method} {path}: unexpected HTTP {status}")
        if not payload:
            return None
        try:
            decoded = json.loads(payload)
        except json.JSONDecodeError as error:
            raise GateFailure(f"{method} {path}: malformed JSON response") from error
        if not isinstance(decoded, dict):
            raise GateFailure(f"{method} {path}: response is not an object")
        return decoded

    def create(self, memory_mib: int, vcpus: int) -> str:
        response = self.request(
            "POST",
            "/v1/vms",
            {"memory_mib": memory_mib, "vcpus": vcpus},
            (201,),
        )
        assert response is not None
        vm_id = response.get("id")
        if response.get("status") != "running" or not isinstance(vm_id, str):
            raise GateFailure(f"create returned an invalid VM record: {response}")
        return vm_id

    def execute(self, vm_id: str, command: str) -> dict[str, object]:
        response = self.request(
            "POST",
            "/v1/execute",
            {"vm_id": vm_id, "command": command, "timeout_ms": 90_000},
        )
        assert response is not None
        if response.get("status") != "completed" or response.get("exit_code") != 0:
            raise GateFailure(f"execute failed for VM {vm_id}: {response}")
        return response

    def fork(self, source_id: str, child_id: str) -> dict[str, object]:
        response = self.request(
            "POST", f"/v1/vms/{source_id}/fork", {"id": child_id}, (201,)
        )
        assert response is not None
        vm = response.get("vm")
        if (
            response.get("source_vm_id") != source_id
            or not isinstance(vm, dict)
            or vm.get("id") != child_id
            or vm.get("status") != "running"
        ):
            raise GateFailure(f"fork returned an invalid child: {response}")
        return response

    def delete(self, vm_id: str) -> None:
        self.request("DELETE", f"/v1/vms/{vm_id}", expected=(200, 204))


def positive_metric(metrics: dict[str, object], name: str) -> int:
    value = metrics.get(name)
    if not isinstance(value, int) or value <= 0:
        raise GateFailure(f"fork metric {name} is missing or non-positive: {metrics}")
    return value


def run_case(
    client: Client, memory_mib: int, vcpus: int, iterations: int
) -> dict[str, object]:
    source_id = client.create(memory_mib, vcpus)
    child_id: str | None = None
    samples: dict[str, list[int]] = {
        "wall_ready_us": [],
        "total_us": [],
        "snapshot_artifact_us": [],
        "child_ready_us": [],
        "downtime_us": [],
    }
    try:
        memory = client.execute(source_id, "awk '/MemTotal/ {print $2}' /proc/meminfo")
        stdout = memory.get("stdout")
        if not isinstance(stdout, str):
            raise GateFailure("guest memory probe omitted stdout")
        try:
            guest_memory_kib = int(stdout.strip())
        except ValueError as error:
            raise GateFailure(
                f"guest memory probe was not numeric: {stdout!r}"
            ) from error
        lower_bound = memory_mib * 1024 * 3 // 4
        upper_bound = memory_mib * 1024 * 5 // 4
        if not lower_bound <= guest_memory_kib <= upper_bound:
            raise GateFailure(
                f"guest memory {guest_memory_kib} KiB does not match {memory_mib} MiB case"
            )
        client.execute(
            source_id,
            """sh -c '(i=0; while :; do i=$((i+1)); printf %s "$i" > """
            """/tmp/.tarit-fork-perf; mv /tmp/.tarit-fork-perf """
            """/tmp/tarit-fork-perf; done) >/dev/null 2>&1 &'""",
        )
        for index in range(iterations):
            child_id = str(uuid.uuid4())
            started = time.monotonic_ns()
            response = client.fork(source_id, child_id)
            client.execute(child_id, "test -s /tmp/tarit-fork-perf")
            samples["wall_ready_us"].append(
                max(1, math.ceil((time.monotonic_ns() - started) / 1_000))
            )
            metrics = response.get("metrics")
            if not isinstance(metrics, dict):
                raise GateFailure(f"fork response omitted phase metrics: {response}")
            samples["total_us"].append(positive_metric(metrics, "total_us"))
            samples["snapshot_artifact_us"].append(
                positive_metric(metrics, "snapshot_artifact_us")
            )
            samples["child_ready_us"].append(positive_metric(metrics, "child_ready_us"))
            live = metrics.get("live_snapshot")
            if not isinstance(live, dict):
                raise GateFailure(
                    f"fork response omitted live-snapshot metrics: {metrics}"
                )
            samples["downtime_us"].append(positive_metric(live, "downtime_us"))
            client.delete(child_id)
            child_id = None
            print(
                f"fork-performance memory_mib={memory_mib} "
                f"iteration={index + 1}/{iterations}",
                flush=True,
            )
        return {
            "memory_mib": memory_mib,
            "guest_memory_kib": guest_memory_kib,
            "iterations": iterations,
            "metrics_us": {
                name: summary(values, iterations) for name, values in samples.items()
            },
        }
    finally:
        if child_id is not None:
            try:
                client.delete(child_id)
            except GateFailure:
                pass
        client.delete(source_id)


def write_report(path: pathlib.Path, report: dict[str, object]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    descriptor, temporary = tempfile.mkstemp(
        prefix=f".{path.name}.", suffix=".tmp", dir=path.parent
    )
    try:
        os.fchmod(descriptor, 0o600)
        with os.fdopen(descriptor, "w", encoding="utf-8") as output:
            json.dump(report, output, indent=2, sort_keys=True)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
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


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--api-key", required=True)
    parser.add_argument("--report", type=pathlib.Path, required=True)
    parser.add_argument("--iterations", type=int, default=100)
    parser.add_argument("--small-memory-mib", type=int, default=256)
    parser.add_argument("--large-memory-mib", type=int, default=4096)
    parser.add_argument("--vcpus", type=int, default=1)
    parser.add_argument("--request-timeout-seconds", type=float, default=180)
    parser.add_argument("--max-p99-total-us", type=int, default=8_000_000)
    parser.add_argument("--max-p99-downtime-us", type=int, default=50_000)
    parser.add_argument("--max-large-small-p99-ratio", type=float, default=1.25)
    args = parser.parse_args()
    if args.iterations < 100:
        parser.error("--iterations must be at least 100 for a p99 gate")
    if not 128 <= args.small_memory_mib < args.large_memory_mib:
        parser.error("memory sizes must satisfy 128 <= small < large")
    if not 1 <= args.vcpus <= 32:
        parser.error("--vcpus must be between 1 and 32")
    if args.request_timeout_seconds <= 0:
        parser.error("--request-timeout-seconds must be positive")
    if args.max_large_small_p99_ratio < 1:
        parser.error("--max-large-small-p99-ratio must be at least 1")
    return args


def main() -> None:
    args = parse_args()
    client = Client(args.base_url, args.api_key, args.request_timeout_seconds)
    small = run_case(client, args.small_memory_mib, args.vcpus, args.iterations)
    large = run_case(client, args.large_memory_mib, args.vcpus, args.iterations)
    small_metrics = small["metrics_us"]
    large_metrics = large["metrics_us"]
    assert isinstance(small_metrics, dict) and isinstance(large_metrics, dict)
    ratios = {}
    for name in ("total_us", "snapshot_artifact_us", "child_ready_us", "wall_ready_us"):
        small_stat = small_metrics[name]
        large_stat = large_metrics[name]
        assert isinstance(small_stat, dict) and isinstance(large_stat, dict)
        ratios[name] = bounded_ratio(large_stat["p99"], small_stat["p99"])
    report = {
        "schema_version": 1,
        "iterations_per_size": args.iterations,
        "small": small,
        "large": large,
        "large_small_p99_ratio": ratios,
        "limits": {
            "max_p99_total_us": args.max_p99_total_us,
            "max_p99_downtime_us": args.max_p99_downtime_us,
            "max_large_small_p99_ratio": args.max_large_small_p99_ratio,
        },
    }
    write_report(args.report, report)
    failures = []
    for case in (small, large):
        metrics = case["metrics_us"]
        assert isinstance(metrics, dict)
        total = metrics["total_us"]
        downtime = metrics["downtime_us"]
        assert isinstance(total, dict) and isinstance(downtime, dict)
        if total["p99"] > args.max_p99_total_us:
            failures.append(
                f"{case['memory_mib']} MiB total p99 {total['p99']} us exceeds "
                f"{args.max_p99_total_us} us"
            )
        if downtime["p99"] > args.max_p99_downtime_us:
            failures.append(
                f"{case['memory_mib']} MiB downtime p99 {downtime['p99']} us exceeds "
                f"{args.max_p99_downtime_us} us"
            )
    for name, ratio in ratios.items():
        if ratio > args.max_large_small_p99_ratio:
            failures.append(
                f"{name} large/small p99 ratio {ratio:.3f} exceeds "
                f"{args.max_large_small_p99_ratio:.3f}"
            )
    if failures:
        raise GateFailure("; ".join(failures))
    print(
        "FORK_LAZY_PERFORMANCE_PASS "
        f"iterations_per_size={args.iterations} report={args.report}"
    )


if __name__ == "__main__":
    main()
