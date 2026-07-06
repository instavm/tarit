#!/usr/bin/env bash
set -uo pipefail

BASE="${BASE:-$HOME/membench}"
VMM="${VMM:-$HOME/membench-vmm}"
KERNEL="${KERNEL:-/tmp/vmlinux.microvm}"
ROOTFS="${ROOTFS:-/tmp/agent-rootfs.ext4}"
RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)-$$"
RUN_DIR="$BASE/run-$RUN_ID"
SOCK="$RUN_DIR/vmm.sock"
LOG="$RUN_DIR/vmm.log"
API="$RUN_DIR/vmm_api.py"
METRICS="$RUN_DIR/snapshots.tsv"
VMM_PID=""
FAILS=0
PASSES=0
SNAP_PATH=""
export KERNEL ROOTFS

mkdir -p "$RUN_DIR"
printf 'test\tname\tdiff\tpath\tbytes\tduration_ms\n' > "$METRICS"

cat > "$API" <<'PY'
#!/usr/bin/env python3
import json, os, socket, struct, sys

def recvall(sock, n):
    data = b""
    while len(data) < n:
        chunk = sock.recv(n - len(data))
        if not chunk:
            raise RuntimeError("short read from VMM API")
        data += chunk
    return data

def api(sock_path, obj, timeout=300):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(timeout)
    s.connect(sock_path)
    body = json.dumps(obj).encode()
    s.sendall(struct.pack(">I", len(body)) + body)
    n = struct.unpack(">I", recvall(s, 4))[0]
    resp = json.loads(recvall(s, n).decode())
    s.close()
    return resp

op = sys.argv[1]
sock = sys.argv[2]
cmdline = "console=ttyS0 reboot=k panic=1 pci=off i8042.noaux random.trust_cpu=on nowatchdog nokaslr root=/dev/vda rw"
if op == "create":
    obj = {"op":"create","config":{"kernel":{"path":os.environ.get("KERNEL", "/tmp/vmlinux.microvm"),"cmdline":cmdline,"initramfs":None},"memory":{"size_mib":1024},"vcpus":{"count":1},"volumes":[{"path":os.environ.get("ROOTFS", "/tmp/agent-rootfs.ext4"),"read_only":False}],"net":[]}}
elif op == "snapshot":
    obj = {"op":"snapshot","diff":sys.argv[3].lower() == "true"}
elif op == "stop":
    obj = {"op":"stop"}
elif op == "restore":
    obj = {"op":"restore","snapshot_path":sys.argv[3]}
else:
    raise SystemExit("unknown op " + op)
print(json.dumps(api(sock, obj), sort_keys=True))
PY
chmod +x "$API"

json_field() {
  local key="$1"
  python3 -c 'import json,sys; d=json.load(sys.stdin); v=d.get(sys.argv[1], ""); sys.stdout.write(str(v) if v is not None else "")' "$key"
}
now_ms() { date +%s%3N; }
api_raw() { python3 "$API" "$@"; }

cleanup() {
  if [[ -n "${VMM_PID:-}" ]] && kill -0 "$VMM_PID" 2>/dev/null; then
    echo "Cleaning VMM pid=$VMM_PID"
    kill "$VMM_PID" 2>/dev/null || true
    wait "$VMM_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

install_guest_service() {
  local helper_dir="$RUN_DIR/rootfs-helpers" mnt="$RUN_DIR/rootfs-mnt"
  mkdir -p "$helper_dir" "$mnt"
  cat > "$helper_dir/livesnap_guest.py" <<'PY'
#!/usr/bin/env python3
import hashlib, multiprocessing as mp, os, subprocess, sys, time, traceback
LOG = "/root/livesnap-guest.log"
RESULTS = "/root/livesnap-results.txt"

def emit(msg):
    line = "MB " + msg
    with open(LOG, "a") as f:
        f.write(line + "\n"); f.flush()
    with open(RESULTS, "a") as f:
        f.write(line + "\n"); f.flush()
    for dev in ("/dev/ttyS0", "/dev/console"):
        try:
            with open(dev, "w") as c:
                c.write(line + "\n"); c.flush()
        except Exception:
            pass
    print(line, flush=True)

def pattern(block, epoch):
    seed = hashlib.sha256((str(epoch) + ":" + str(block)).encode()).digest()
    return (seed * (1024 * 1024 // len(seed) + 1))[:1024 * 1024]

def fill(path, mib, epoch):
    h = hashlib.sha256()
    with open(path, "wb") as f:
        for block in range(mib):
            data = pattern(block, epoch)
            f.write(data); h.update(data)
    return h.hexdigest()

def sha(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while True:
            data = f.read(1024 * 1024)
            if not data: break
            h.update(data)
    return h.hexdigest()

def verify(path, expected):
    actual = sha(path)
    if actual != expected:
        emit("VERIFY_FAIL path=%s expected=%s actual=%s" % (path, expected, actual))
        raise RuntimeError("checksum mismatch")
    return actual

def loop_verify(path, mib, seconds, result):
    try:
        end = time.time() + seconds
        epoch = 0
        while time.time() < end:
            expected = fill(path, mib, epoch)
            actual = sha(path)
            if actual != expected:
                with open(result, "w") as f: f.write("FAIL epoch=%d expected=%s actual=%s\n" % (epoch, expected, actual))
                return 2
            epoch += 1
        with open(result, "w") as f: f.write("OK epochs=%d\n" % epoch)
        return 0
    except BaseException as e:
        with open(result, "w") as f: f.write("EXCEPTION %r\n" % (e,))
        return 3

def main():
    open(LOG, "w").close(); open(RESULTS, "w").close()
    emit("START pid=%d" % os.getpid())
    workdir = "/mnt/livesnap"
    os.makedirs(workdir, exist_ok=True)
    try:
        subprocess.run(["/bin/mount", "-t", "tmpfs", "-o", "size=192m", "tmpfs", workdir], check=False)
    except Exception as e:
        emit("WARN tmpfs_mount_failed=%r" % (e,))

    # TEST A: byte exact tmpfs RAM file across full snapshot/restore.
    a_path = workdir + "/testA.bin"
    a_sha = fill(a_path, 64, 100)
    emit("TEST_A_READY sha=%s" % a_sha)
    time.sleep(30)
    verify(a_path, a_sha)
    emit("TEST_A_PASS sha=%s" % a_sha)

    # TEST B: live self-verifying mutator across full+diff snapshots, then fresh verify.
    live_path = workdir + "/live.bin"
    live_result = "/root/livesnap-live-result.txt"
    p = mp.Process(target=loop_verify, args=(live_path, 32, 70, live_result))
    p.start()
    emit("TEST_B_STARTED pid=%d workload=python-fallback mib=32 seconds=70" % p.pid)
    p.join()
    rc = p.exitcode
    text = open(live_result).read().strip() if os.path.exists(live_result) else "missing"
    emit("TEST_B_LIVE_RESULT rc=%s result=%s" % (rc, text))
    if rc != 0 or not text.startswith("OK"):
        raise RuntimeError("live workload failed")
    fresh_result = "/root/livesnap-fresh-result.txt"
    fresh_rc = loop_verify(workdir + "/fresh.bin", 16, 20, fresh_result)
    fresh_text = open(fresh_result).read().strip()
    emit("TEST_B_FRESH_RESULT rc=%s result=%s" % (fresh_rc, fresh_text))
    if fresh_rc != 0:
        raise RuntimeError("fresh workload failed")
    emit("TEST_B_PASS")

    # TEST C: full + two diffs, restore last diff, verify last checkpoint.
    c_path = workdir + "/chain.bin"
    c0 = fill(c_path, 32, 0); emit("TEST_C_STAGE0 sha=%s" % c0); time.sleep(20)
    c1 = fill(c_path, 32, 1); emit("TEST_C_STAGE1 sha=%s" % c1); time.sleep(20)
    c2 = fill(c_path, 32, 2); emit("TEST_C_STAGE2 sha=%s" % c2); time.sleep(20)
    verify(c_path, c2)
    emit("TEST_C_PASS sha=%s" % c2)

    # TEST D: 20 restore cycles; each cycle snapshots a known checksum then verifies after restore.
    d_path = workdir + "/soak.bin"
    for i in range(1, 21):
        d_sha = fill(d_path, 8, i)
        emit("TEST_D_READY cycle=%d sha=%s" % (i, d_sha))
        time.sleep(10)
        verify(d_path, d_sha)
        emit("TEST_D_VERIFIED cycle=%d sha=%s" % (i, d_sha))
    emit("TEST_D_PASS cycles=20")
    emit("ALL_PASS")

try:
    main()
except BaseException:
    emit("FAIL exception=" + traceback.format_exc().replace("\n", " | "))
    sys.exit(1)
PY
  cat > "$helper_dir/livesnap-membench.service" <<'UNIT'
[Unit]
Description=Live snapshot memory consistency guest workload
After=multi-user.target

[Service]
Type=simple
ExecStart=/usr/bin/python3 /usr/local/bin/livesnap_guest.py
StandardOutput=journal+console
StandardError=journal+console
Restart=no

[Install]
WantedBy=multi-user.target
UNIT
  chmod +x "$helper_dir/livesnap_guest.py"
  echo "Installing guest auto-workload service into rootfs"
  sudo mount -o loop "$ROOTFS" "$mnt" || return 1
  sudo mkdir -p "$mnt/usr/local/bin" "$mnt/etc/systemd/system/multi-user.target.wants" || { sudo umount "$mnt"; return 1; }
  sudo cp "$helper_dir/livesnap_guest.py" "$mnt/usr/local/bin/livesnap_guest.py" || { sudo umount "$mnt"; return 1; }
  sudo cp "$helper_dir/livesnap-membench.service" "$mnt/etc/systemd/system/livesnap-membench.service" || { sudo umount "$mnt"; return 1; }
  sudo chmod +x "$mnt/usr/local/bin/livesnap_guest.py" || { sudo umount "$mnt"; return 1; }
  sudo ln -sfn ../livesnap-membench.service "$mnt/etc/systemd/system/multi-user.target.wants/livesnap-membench.service" || { sudo umount "$mnt"; return 1; }
  sudo rm -f "$mnt/root/livesnap-guest.log" "$mnt/root/livesnap-results.txt" "$mnt/root/livesnap-live-result.txt" "$mnt/root/livesnap-fresh-result.txt" || true
  sync
  sudo umount "$mnt" || return 1
}

start_vmm() {
  echo "Using frozen VMM: $VMM"
  echo "Run dir: $RUN_DIR"
  "$VMM" serve --socket "$SOCK" >"$LOG" 2>&1 &
  VMM_PID=$!
  echo "Started VMM serve pid=$VMM_PID socket=$SOCK log=$LOG"
  for _ in $(seq 1 100); do [[ -S "$SOCK" ]] && return 0; sleep 0.1; done
  return 1
}

create_guest() {
  local resp status
  resp="$(api_raw create "$SOCK")" || return 1
  status="$(printf '%s' "$resp" | json_field status)"
  [[ "$status" == "ok" ]] || { echo "create failed: $resp"; return 1; }
}

wait_marker() {
  local marker="$1" timeout="$2" start now
  start=$(date +%s)
  while true; do
    if grep -F "$marker" "$LOG" >/dev/null 2>&1; then
      grep -F "$marker" "$LOG" | tail -1
      return 0
    fi
    if grep -F "MB FAIL" "$LOG" >/dev/null 2>&1; then
      grep -F "MB FAIL" "$LOG" | tail -1
      return 1
    fi
    now=$(date +%s)
    if (( now - start > timeout )); then
      echo "timeout waiting for marker: $marker" >&2
      tail -80 "$LOG" >&2
      return 1
    fi
    sleep 1
  done
}

snapshot_take() {
  local test="$1" name="$2" diff="$3" start end resp path bytes dur status
  start="$(now_ms)"
  resp="$(api_raw snapshot "$SOCK" "$diff")" || return 1
  end="$(now_ms)"
  status="$(printf '%s' "$resp" | json_field status)"
  path="$(printf '%s' "$resp" | json_field path)"
  [[ "$status" == "snapshot" && -n "$path" ]] || { echo "snapshot failed: $resp"; return 1; }
  bytes="$(stat -c%s "$path" 2>/dev/null || echo 0)"
  dur=$((end - start))
  printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$test" "$name" "$diff" "$path" "$bytes" "$dur" >> "$METRICS"
  echo "SNAPSHOT test=$test name=$name diff=$diff path=$path bytes=$bytes duration_ms=$dur"
  SNAP_PATH="$path"
}

stop_restore() {
  local snap="$1" resp status
  resp="$(api_raw stop "$SOCK")" || return 1
  status="$(printf '%s' "$resp" | json_field status)"
  [[ "$status" == "ok" ]] || { echo "stop failed: $resp"; return 1; }
  resp="$(api_raw restore "$SOCK" "$snap")" || return 1
  status="$(printf '%s' "$resp" | json_field status)"
  [[ "$status" == "restored" ]] || { echo "restore failed: $resp"; return 1; }
}

pass() { echo "PASS $1"; PASSES=$((PASSES+1)); }
fail() { echo "FAIL $1"; FAILS=$((FAILS+1)); }

test_a() {
  wait_marker "MB TEST_A_READY" 120 || return 1
  sleep 5
  snapshot_take A full-ram-tmpfs false || return 1
  stop_restore "$SNAP_PATH" || return 1
  wait_marker "MB TEST_A_PASS" 60 || return 1
}

test_b() {
  wait_marker "MB TEST_B_STARTED" 120 || return 1
  sleep 5
  snapshot_take B live-full false || return 1
  sleep 4
  snapshot_take B live-diff-1 true || return 1
  sleep 4
  snapshot_take B live-diff-2 true || return 1
  stop_restore "$SNAP_PATH" || return 1
  wait_marker "MB TEST_B_PASS" 180 || return 1
}

test_c() {
  wait_marker "MB TEST_C_STAGE0" 180 || return 1
  sleep 5
  snapshot_take C chain-full false || return 1
  wait_marker "MB TEST_C_STAGE1" 80 || return 1
  sleep 5
  snapshot_take C chain-diff-1 true || return 1
  wait_marker "MB TEST_C_STAGE2" 80 || return 1
  sleep 5
  snapshot_take C chain-diff-2 true || return 1
  stop_restore "$SNAP_PATH" || return 1
  wait_marker "MB TEST_C_PASS" 80 || return 1
}

test_d() {
  local i diff
  for i in $(seq 1 20); do
    wait_marker "MB TEST_D_READY cycle=$i" 120 || return 1
    sleep 3
    if (( i == 1 || i % 5 == 0 )); then diff=false; else diff=true; fi
    snapshot_take D "soak-$i" "$diff" || return 1
    stop_restore "$SNAP_PATH" || return 1
    wait_marker "MB TEST_D_VERIFIED cycle=$i" 80 || return 1
  done
  wait_marker "MB TEST_D_PASS" 80 || return 1
}

main() {
  echo "livesnap-membench start UTC=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "Host=$(hostname) kernel=$(uname -a)"
  install_guest_service || exit 2
  start_vmm || exit 2
  create_guest || exit 2
  if test_a; then pass "TEST A quiescent RAM checksum"; else fail "TEST A quiescent RAM checksum"; fi
  if test_b; then pass "TEST B live mutation verify restore"; else fail "TEST B live mutation verify restore"; fi
  if test_c; then pass "TEST C incremental chain"; else fail "TEST C incremental chain"; fi
  if test_d; then pass "TEST D 20-cycle soak"; else fail "TEST D 20-cycle soak"; fi
  echo "Snapshot metrics: $METRICS"
  cat "$METRICS"
  echo "OVERALL passes=$PASSES fails=$FAILS"
  echo "livesnap-membench end UTC=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  (( FAILS == 0 ))
}

main "$@"
