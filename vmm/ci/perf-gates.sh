#!/usr/bin/env bash
# Hardware-backed lifecycle performance regression gate.
#
# Runs a bounded number of successful end-to-end lifecycle iterations and
# reports median/p95/p99. Invalid API responses, missing artifacts, failed
# ownership transfer, and zero/missing samples always fail. Threshold
# regressions fail when VMM_PERF_STRICT=1.
set -euo pipefail

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
export VMM="${VMM:-$SCRIPT_DIR/../target/release/vmm}"
export KERNEL="${KERNEL:-/tmp/vmlinux.minimal}"
export ROOTFS="${ROOTFS:-/tmp/vsock-rootfs.ext4}"

exec python3 - <<'PY'
import ctypes
import json
import math
import os
import socket
import stat
import statistics
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path

MAX_FRAME = 16 * 1024 * 1024
CMDLINE = (
    "console=ttyS0 quiet loglevel=0 reboot=k panic=-1 nomodule "
    "i8042.noaux swiotlb=noforce random.trust_cpu=on nokaslr pci=off "
    "root=/dev/vda rw init=/usr/sbin/vmm-agent"
)


class InvalidRun(RuntimeError):
    pass


class StatxTimestamp(ctypes.Structure):
    _fields_ = [
        ("seconds", ctypes.c_int64),
        ("nanoseconds", ctypes.c_uint32),
        ("reserved", ctypes.c_int32),
    ]


class Statx(ctypes.Structure):
    _fields_ = [
        ("mask", ctypes.c_uint32),
        ("block_size", ctypes.c_uint32),
        ("attributes", ctypes.c_uint64),
        ("links", ctypes.c_uint32),
        ("uid", ctypes.c_uint32),
        ("gid", ctypes.c_uint32),
        ("mode", ctypes.c_uint16),
        ("spare0", ctypes.c_uint16),
        ("inode", ctypes.c_uint64),
        ("size", ctypes.c_uint64),
        ("blocks", ctypes.c_uint64),
        ("attributes_mask", ctypes.c_uint64),
        ("atime", StatxTimestamp),
        ("birth_time", StatxTimestamp),
        ("ctime", StatxTimestamp),
        ("mtime", StatxTimestamp),
        ("rdev_major", ctypes.c_uint32),
        ("rdev_minor", ctypes.c_uint32),
        ("dev_major", ctypes.c_uint32),
        ("dev_minor", ctypes.c_uint32),
        ("mount_id", ctypes.c_uint64),
        ("dio_mem_align", ctypes.c_uint32),
        ("dio_offset_align", ctypes.c_uint32),
        ("spare3", ctypes.c_uint64 * 12),
    ]


def positive_int(name, default, *, minimum=1, maximum=None):
    raw = os.environ.get(name, str(default))
    try:
        value = int(raw)
    except ValueError as error:
        raise InvalidRun(f"{name} must be an integer, got {raw!r}") from error
    if value < minimum or (maximum is not None and value > maximum):
        bound = f" and <= {maximum}" if maximum is not None else ""
        raise InvalidRun(f"{name} must be >= {minimum}{bound}, got {value}")
    return value


def scratch_identity(path):
    # Rust's Metadata::created() is backed by Linux statx birth time. Capture
    # the same fields so ReleaseScratch transfers the exact artifact identity,
    # not merely a path that could have been replaced.
    statx = getattr(ctypes.CDLL(None, use_errno=True), "statx", None)
    if statx is None:
        raise InvalidRun("libc statx is required for scratch ownership transfer")
    statx.argtypes = [
        ctypes.c_int,
        ctypes.c_char_p,
        ctypes.c_int,
        ctypes.c_uint,
        ctypes.POINTER(Statx),
    ]
    statx.restype = ctypes.c_int
    result = Statx()
    at_fdcwd = -100
    at_symlink_nofollow = 0x100
    statx_basic_stats = 0x7FF
    statx_birth_time = 0x800
    rc = statx(
        at_fdcwd,
        os.fsencode(path),
        at_symlink_nofollow,
        statx_basic_stats | statx_birth_time,
        ctypes.byref(result),
    )
    if rc != 0:
        error_number = ctypes.get_errno()
        raise InvalidRun(f"statx {path}: {os.strerror(error_number)}")
    if not stat.S_ISREG(result.mode):
        raise InvalidRun(f"snapshot is not a regular file: {path}")
    created_secs = None
    created_nanos = None
    if result.mask & statx_birth_time:
        created_secs = result.birth_time.seconds
        created_nanos = result.birth_time.nanoseconds
    identity = {
        "device": os.makedev(result.dev_major, result.dev_minor),
        "inode": result.inode,
        "created_secs": created_secs,
        "created_nanos": created_nanos,
    }
    return identity


# A p99 gate needs at least 100 observations; with fewer samples, nearest-rank
# p99 is only another spelling of "maximum" and is too noisy to compare runs.
ITERATIONS = positive_int("VMM_PERF_ITERATIONS", 100, minimum=100, maximum=500)
EXEC_DEADLINE_S = positive_int("VMM_PERF_EXEC_DEADLINE_S", 30, maximum=60)
STRICT = os.environ.get("VMM_PERF_STRICT", "0") == "1"
CEILINGS = {
    "cold create -> first exec": positive_int("MAX_COLD_EXEC_MS", 700),
    "snapshot (full)": positive_int("MAX_SNAPSHOT_MS", 350),
    "snapshot (diff)": positive_int("MAX_DIFF_MS", 80),
    "restore -> first exec": positive_int("MAX_RESTORE_EXEC_MS", 700),
    "suspend": positive_int("MAX_SUSPEND_MS", 350),
    "resume -> first exec": positive_int("MAX_RESUME_EXEC_MS", 250),
    "suspend -> resume -> first exec": positive_int(
        "MAX_SUSPEND_RESUME_EXEC_MS", 600
    ),
    "VMM RSS": positive_int("MAX_RSS_KB", 131072),
}

VMM = Path(os.environ["VMM"])
KERNEL = Path(os.environ["KERNEL"])
ROOTFS = Path(os.environ["ROOTFS"])
for path in (VMM, KERNEL, ROOTFS):
    if not path.is_file():
        raise InvalidRun(f"required file missing: {path}")
if not os.access(VMM, os.X_OK):
    raise InvalidRun(f"VMM is not executable: {VMM}")


def recv_exact(sock, length):
    chunks = []
    remaining = length
    while remaining:
        chunk = sock.recv(remaining)
        if not chunk:
            raise InvalidRun(
                f"control socket closed with {remaining} response bytes outstanding"
            )
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def frame(sock_path, request):
    body = json.dumps(request, separators=(",", ":")).encode()
    if len(body) > MAX_FRAME:
        raise InvalidRun(f"request frame is too large: {len(body)}")
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.settimeout(60)
        client.connect(sock_path)
        client.sendall(struct.pack(">I", len(body)) + body)
        response_len = struct.unpack(">I", recv_exact(client, 4))[0]
        if response_len > MAX_FRAME:
            raise InvalidRun(f"response frame is too large: {response_len}")
        raw = recv_exact(client, response_len)
    try:
        response = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise InvalidRun(f"malformed API response: {raw!r}") from error
    if not isinstance(response, dict) or not isinstance(response.get("status"), str):
        raise InvalidRun(f"API response has no string status: {response!r}")
    return response


def expect(sock_path, request, expected, label):
    response = frame(sock_path, request)
    if response["status"] != expected:
        raise InvalidRun(
            f"{label} expected status={expected!r}, got {response!r}"
        )
    return response


def elapsed_ms(start_ns):
    elapsed = time.monotonic_ns() - start_ns
    if elapsed <= 0:
        raise InvalidRun("monotonic clock returned a non-positive duration")
    # Round a successful sub-millisecond operation up to 1 ms. A zero sample
    # is reserved for an invalid/missing measurement and is always rejected.
    return max(1, math.ceil(elapsed / 1_000_000))


def wait_first_exec(sock_path, start_ns, command="true"):
    deadline = time.monotonic() + EXEC_DEADLINE_S
    while time.monotonic() < deadline:
        response = frame(
            sock_path,
            {"op": "exec", "command": command, "timeout_ms": 1_000},
        )
        if response["status"] == "exec":
            if response.get("exit_code") != 0:
                raise InvalidRun(
                    f"first exec completed unsuccessfully: {response!r}"
                )
            return elapsed_ms(start_ns)
        if response["status"] != "err":
            raise InvalidRun(f"exec readiness probe returned {response!r}")
        time.sleep(0.015)
    raise InvalidRun(f"first exec did not succeed within {EXEC_DEADLINE_S}s")


def exec_success(sock_path, command, label):
    response = expect(
        sock_path,
        {"op": "exec", "command": command, "timeout_ms": 5_000},
        "exec",
        label,
    )
    if response.get("exit_code") != 0:
        raise InvalidRun(f"{label} completed unsuccessfully: {response!r}")
    return response


def read_rss_kb(pid):
    status_path = Path(f"/proc/{pid}/status")
    try:
        for line in status_path.read_text().splitlines():
            if line.startswith("VmRSS:"):
                value = int(line.split()[1])
                if value <= 0:
                    break
                return value
    except (OSError, ValueError) as error:
        raise InvalidRun(f"could not read VMM RSS from {status_path}: {error}") from error
    raise InvalidRun(f"VMM RSS is missing or zero in {status_path}")


def unlink_owned_snapshot(path, device, inode):
    try:
        current = os.stat(path, follow_symlinks=False)
        if not stat.S_ISREG(current.st_mode) or (current.st_dev, current.st_ino) != (
            device,
            inode,
        ):
            raise InvalidRun(f"refusing to unlink replaced snapshot: {path}")
        os.unlink(path)
    except FileNotFoundError as error:
        raise InvalidRun(f"owned snapshot disappeared before cleanup: {path}") from error
    except OSError as error:
        raise InvalidRun(f"could not remove owned snapshot {path}: {error}") from error


def percentile(sorted_values, fraction):
    # Nearest-rank percentile. The default 100 samples makes p99 a real tail
    # observation instead of silently presenting a tiny-sample maximum as p99.
    return sorted_values[max(0, math.ceil(fraction * len(sorted_values)) - 1)]


def summarize(values):
    if len(values) != ITERATIONS or any(
        not isinstance(value, int) or value <= 0 for value in values
    ):
        raise InvalidRun(
            f"missing/zero samples: expected {ITERATIONS}, got {values!r}"
        )
    ordered = sorted(values)
    return {
        "median": statistics.median(ordered),
        "p95": percentile(ordered, 0.95),
        "p99": percentile(ordered, 0.99),
    }


def fmt_number(value):
    return str(int(value)) if float(value).is_integer() else f"{value:.1f}"


def create_request(rootfs, overlay):
    return {
        "op": "create",
        "config": {
            "kernel": {
                "path": str(KERNEL),
                "cmdline": CMDLINE,
                "initramfs": None,
            },
            "memory": {"size_mib": 256},
            "vcpus": {"count": 1},
            "volumes": [
                {
                    "path": str(rootfs),
                    "read_only": False,
                    "overlay": str(overlay),
                }
            ],
            "net": [],
        },
    }


samples = {name: [] for name in CEILINGS}
completed = 0
owned_artifacts = []  # (path, st_dev, st_ino), transferred via ReleaseScratch
process = None
log_handle = None

try:
    with tempfile.TemporaryDirectory(prefix="tarit-perfgate-", dir="/tmp") as workdir:
        workdir = Path(workdir)
        sock_path = workdir / "vmm.sock"
        log_path = workdir / "vmm.log"
        log_handle = log_path.open("wb")
        environment = os.environ.copy()
        environment["RUST_LOG"] = "error"
        process = subprocess.Popen(
            [str(VMM), "serve", "--socket", str(sock_path)],
            stdout=log_handle,
            stderr=subprocess.STDOUT,
            env=environment,
        )

        ready_deadline = time.monotonic() + 5
        while not sock_path.is_socket() and time.monotonic() < ready_deadline:
            if process.poll() is not None:
                raise InvalidRun("VMM exited before binding its control socket")
            time.sleep(0.01)
        if not sock_path.is_socket():
            raise InvalidRun("VMM control socket was not ready within 5 seconds")

        for iteration in range(1, ITERATIONS + 1):
            golden_overlay = workdir / f"golden-{iteration}.cow"
            clone_overlay = workdir / f"clone-{iteration}.cow"
            start = time.monotonic_ns()
            expect(
                sock_path,
                create_request(ROOTFS, golden_overlay),
                "ok",
                "create",
            )
            samples["cold create -> first exec"].append(
                wait_first_exec(sock_path, start)
            )
            samples["VMM RSS"].append(read_rss_kb(process.pid))
            snapshot_marker = f"snapshot-{iteration}"
            exec_success(
                sock_path,
                "mkdir -p /mnt/tarit-perf && "
                "mount -t tmpfs -o size=8m tmpfs /mnt/tarit-perf && "
                f"printf '{snapshot_marker}\\n' > /mnt/tarit-perf/state",
                "prepare in-memory snapshot state",
            )

            start = time.monotonic_ns()
            snapshot_response = expect(
                sock_path,
                {"op": "snapshot", "diff": False},
                "snapshot",
                "full snapshot",
            )
            samples["snapshot (full)"].append(elapsed_ms(start))
            snapshot_path = snapshot_response.get("path")
            if not isinstance(snapshot_path, str) or not snapshot_path:
                raise InvalidRun(f"snapshot response omitted path: {snapshot_response!r}")
            snapshot_identity = scratch_identity(snapshot_path)
            snapshot_device = snapshot_identity["device"]
            snapshot_inode = snapshot_identity["inode"]

            expect(
                sock_path,
                {
                    "op": "release_scratch",
                    "path": snapshot_path,
                    "identity": snapshot_identity,
                },
                "ok",
                "release snapshot ownership",
            )
            snapshot_owned = (snapshot_path, snapshot_device, snapshot_inode)
            owned_artifacts.append(snapshot_owned)

            golden_identity = scratch_identity(golden_overlay)
            golden_device = golden_identity["device"]
            golden_inode = golden_identity["inode"]
            expect(
                sock_path,
                {
                    "op": "release_scratch",
                    "path": str(golden_overlay),
                    "identity": golden_identity,
                },
                "ok",
                "release golden overlay ownership",
            )
            golden_owned = (str(golden_overlay), golden_device, golden_inode)
            owned_artifacts.append(golden_owned)

            diff_marker = f"diff-{iteration}"
            exec_success(
                sock_path,
                f"printf '{diff_marker}\\n' > /mnt/tarit-perf/state",
                "dirty guest memory before diff snapshot",
            )
            start = time.monotonic_ns()
            diff_response = expect(
                sock_path,
                {"op": "snapshot", "diff": True},
                "snapshot",
                "diff snapshot",
            )
            samples["snapshot (diff)"].append(elapsed_ms(start))
            diff_path = diff_response.get("path")
            if not isinstance(diff_path, str) or not diff_path:
                raise InvalidRun(
                    f"diff snapshot response omitted path: {diff_response!r}"
                )
            scratch_identity(diff_path)

            expect(sock_path, {"op": "stop"}, "ok", "stop before restore")
            if os.path.lexists(diff_path):
                raise InvalidRun(
                    f"VMM-owned diff snapshot survived stop: {diff_path}"
                )
            current = os.stat(snapshot_path, follow_symlinks=False)
            if (current.st_dev, current.st_ino) != (
                snapshot_device,
                snapshot_inode,
            ):
                raise InvalidRun("released snapshot identity changed after stop")
            current_overlay = os.stat(golden_overlay, follow_symlinks=False)
            if (current_overlay.st_dev, current_overlay.st_ino) != (
                golden_device,
                golden_inode,
            ):
                raise InvalidRun("released golden overlay identity changed after stop")

            start = time.monotonic_ns()
            # An empty net override is the explicit same-cardinality rebind for this
            # networkless snapshot. Networked restore remains fail-closed until
            # the guest agent supports address/route rebinding.
            expect(
                sock_path,
                {
                    "op": "restore",
                    "snapshot_path": snapshot_path,
                    "overlay": str(clone_overlay),
                    "net": [],
                },
                "restored",
                "restore",
            )
            samples["restore -> first exec"].append(
                wait_first_exec(
                    sock_path,
                    start,
                    f"grep -qx '{snapshot_marker}' /mnt/tarit-perf/state",
                )
            )

            suspend_marker = f"suspend-{iteration}"
            exec_success(
                sock_path,
                f"printf '{suspend_marker}\\n' > /mnt/tarit-perf/state",
                "prepare in-memory suspend state",
            )
            suspend_resume_start = time.monotonic_ns()
            suspend_start = time.monotonic_ns()
            expect(sock_path, {"op": "suspend"}, "ok", "suspend")
            samples["suspend"].append(elapsed_ms(suspend_start))

            resume_start = time.monotonic_ns()
            expect(sock_path, {"op": "resume"}, "ok", "resume")
            samples["resume -> first exec"].append(
                wait_first_exec(
                    sock_path,
                    resume_start,
                    f"grep -qx '{suspend_marker}' /mnt/tarit-perf/state",
                )
            )
            samples["suspend -> resume -> first exec"].append(
                elapsed_ms(suspend_resume_start)
            )

            expect(sock_path, {"op": "stop"}, "ok", "final stop")
            if os.path.lexists(clone_overlay):
                raise InvalidRun(
                    f"VMM-owned restore overlay survived stop: {clone_overlay}"
                )
            # The released full snapshot is no longer owned by the VMM. Remove
            # it as soon as this iteration is done so a 100-sample p99 run does
            # not retain tens of GiB of memory images until process exit.
            unlink_owned_snapshot(
                snapshot_path, snapshot_device, snapshot_inode
            )
            owned_artifacts.remove(snapshot_owned)
            unlink_owned_snapshot(
                str(golden_overlay), golden_device, golden_inode
            )
            owned_artifacts.remove(golden_owned)
            completed += 1
            print(f"iteration {iteration}/{ITERATIONS}: complete")

        print(
            f"== perf gates (host: {socket.gethostname()}, iterations: {ITERATIONS}) =="
        )
        regression = False
        for name, values in samples.items():
            stats = summarize(values)
            ceiling = CEILINGS[name]
            unit = "KB" if name == "VMM RSS" else "ms"
            passed = stats["p99"] <= ceiling
            regression |= not passed
            result = "PASS" if passed else "FAIL"
            print(
                f"  {result:<4}  {name:<34} "
                f"median={fmt_number(stats['median']):>6} "
                f"p95={stats['p95']:>6} p99={stats['p99']:>6} {unit} "
                f"(p99 <= {ceiling})"
            )
        success_rate = completed * 100.0 / ITERATIONS
        print(
            f"  lifecycle success rate: {completed}/{ITERATIONS} ({success_rate:.1f}%)"
        )
        print(
            "  warm handout: not measured here (warm-pool assignment is an "
            "orchestrator operation, not a VMM API)"
        )
        if regression:
            print("perf: REGRESSION vs p99 ceilings")
            if STRICT:
                raise SystemExit(1)
            print("perf: VMM_PERF_STRICT!=1, not failing threshold regressions")
        else:
            print("perf: all p99 gates within ceilings")
except InvalidRun as error:
    print(f"perf: invalid run: {error}", file=sys.stderr)
    print(
        f"perf: lifecycle success rate before failure: {completed}/{ITERATIONS} "
        f"({completed * 100.0 / ITERATIONS:.1f}%)",
        file=sys.stderr,
    )
    if log_handle is not None:
        log_handle.flush()
    if 'log_path' in locals() and log_path.exists():
        tail = log_path.read_text(errors="replace").splitlines()[-80:]
        if tail:
            print("-- VMM log tail --", file=sys.stderr)
            print("\n".join(tail), file=sys.stderr)
    raise SystemExit(2)
finally:
    if process is not None and process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)
    if log_handle is not None:
        log_handle.close()
    # Remove only exact artifacts whose identity was transferred to this gate.
    for path, device, inode in owned_artifacts:
        try:
            current = os.stat(path, follow_symlinks=False)
            if (
                stat.S_ISREG(current.st_mode)
                and current.st_dev == device
                and current.st_ino == inode
            ):
                os.unlink(path)
        except FileNotFoundError:
            pass
PY
