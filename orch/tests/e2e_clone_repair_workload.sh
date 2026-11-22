#!/usr/bin/env bash
# Real-KVM qualification for application state repair across clone and resume.
set -Eeuo pipefail
umask 077

ROOT="${ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}"
SOCKET_ROOT="${TARIT_TEST_SOCKET_ROOT:-${TMPDIR:-/tmp}}"

for required in gcc mktemp; do
  command -v "$required" >/dev/null || { echo "FAIL: missing $required" >&2; exit 1; }
done

DIR=$(mktemp -d "$SOCKET_ROOT/tarit-clone-workload-build.XXXXXX")
cleanup() {
  find "$DIR" -depth -delete 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

gcc -std=c11 -O2 -Wall -Wextra -Werror -pedantic -static \
  "$ROOT/orch/tests/clone_repair_workload.c" -o "$DIR/clone-repair-workload"

# Exercise the signal boundary with an accepted client that has no data yet.
# A private /run keeps the fixture's socket, pid and marker off the host.
unshare --mount --propagation private sh -ec '
  mount -t tmpfs tmpfs /run
  exec python3 - "$1"
' sh "$DIR/clone-repair-workload" <<'PY'
from pathlib import Path
import socket
import subprocess
import sys
import time

def wait_until(predicate):
    deadline = time.monotonic() + 5
    while not predicate():
        if time.monotonic() >= deadline:
            raise TimeoutError("workload did not become ready")
        time.sleep(0.01)

binary = sys.argv[1]
runtime = Path("/run/tarit")
clone_id = "ab" * 16
server = subprocess.Popen([binary, "serve"])
try:
    wait_until((runtime / "clone-workload.pid").exists)
    (runtime / "clone-id").write_text(clone_id)
    with socket.socket(socket.AF_UNIX) as client:
        client.settimeout(5)
        client.connect(str(runtime / "clone-workload.sock"))
        fds = Path(f"/proc/{server.pid}/fd")
        wait_until(lambda: sum(fd.is_socket() for fd in fds.iterdir()) >= 2)
        subprocess.run([binary, "repair-signal", clone_id], check=True, timeout=5)
        client.sendall(b"STATE\n")
        assert client.recv(1024).startswith(f"clone={clone_id} ".encode())
finally:
    server.terminate()
    server.wait(timeout=5)
PY

TARIT_TEST_CLONE_WORKLOAD_BIN="$DIR/clone-repair-workload" \
  bash "$ROOT/orch/tests/e2e_live_fork_hibernate.sh"
