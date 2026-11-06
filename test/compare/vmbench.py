#!/usr/bin/env python3
"""Head-to-head tarit vs Firecracker benchmark on identical guests.

Both VMMs boot the same kernel + rootfs + cmdline with the same vCPU/memory
shape and run the same in-guest agent (vmm-agent, exec protocol v2 over vsock).
Each measurement is wall time from the host's point of view through the VMM's
real control path (tarit: UDS JSON API; Firecracker: HTTP API + hybrid vsock).
"""
import argparse, http.client, json, os, shutil, signal, socket, statistics
import struct, subprocess, sys, tempfile, threading, time

RUN = "/run/user/%d/vmbench" % os.getuid()
# Firecracker's default drive cache_type, "Unsafe", ignores guest flushes, so
# a guest fsync is not durable on the host. tarit always honours flushes, so
# compare against Firecracker's durable "Writeback" mode unless asked not to.
FC_CACHE_TYPE = os.environ.get("VMBENCH_FC_CACHE", "Writeback")
VERBOSE = int(os.environ.get("VMBENCH_TARIT_VERBOSE", "0"))
# Files `vmm snapshot` released to us. They live in the VMM's runtime directory
# and belong to the caller, so each scenario removes whatever it did not move.
RELEASED = []


def remove_released():
    while RELEASED:
        try:
            os.unlink(RELEASED.pop())
        except FileNotFoundError:
            pass
GDB = os.environ.get("VMBENCH_GDB") == "1"
CMDLINE = ("console=ttyS0 quiet loglevel=0 reboot=k panic=-1 nomodule pci=off "
           "i8042.noaux i8042.nomux i8042.nopnp i8042.dumbkbd swiotlb=noforce "
           "cryptomgr.notests random.trust_cpu=on tsc=reliable no_timer_check "
           "nowatchdog nokaslr")


def now():
    return time.perf_counter()


def pct(xs, p):
    xs = sorted(xs)
    if not xs:
        return float("nan")
    k = (len(xs) - 1) * p / 100.0
    lo, hi = int(k), min(int(k) + 1, len(xs) - 1)
    return xs[lo] + (xs[hi] - xs[lo]) * (k - lo)


def summary(xs):
    return {"n": len(xs), "p50": pct(xs, 50), "p90": pct(xs, 90), "p99": pct(xs, 99),
            "min": min(xs) if xs else None, "max": max(xs) if xs else None,
            "mean": statistics.fmean(xs) if xs else None}


def wait_path(path, timeout=5.0):
    end = now() + timeout
    while not os.path.exists(path):
        if now() > end:
            raise TimeoutError(path)
        time.sleep(0.0005)


def fresh_rootfs(src, dst):
    subprocess.run(["cp", "--sparse=always", "--reflink=auto", src, dst], check=True)
    # Write the copy back before timing starts. Otherwise the guest's first
    # flush (ext4 journal at mount) makes a VMM that honours VIRTIO_BLK_F_FLUSH
    # pay for writing back the whole freshly copied image.
    fd = os.open(dst, os.O_RDONLY)
    try:
        os.fdatasync(fd)
    finally:
        os.close(fd)


def pss_kib(pid):
    try:
        with open("/proc/%d/smaps_rollup" % pid) as f:
            out = {}
            for line in f:
                k, _, v = line.partition(":")
                if k in ("Rss", "Pss", "Private_Dirty", "Shared_Clean"):
                    out[k] = int(v.split()[0])
            return out
    except FileNotFoundError:
        return {}


# ---------------------------------------------------------------- agent proto
def agent_frame(req_id, command):
    cmd = command.encode()
    payload = struct.pack(">QI", req_id, len(cmd)) + cmd
    return b"VEX2" + bytes([2, 1]) + struct.pack(">I", len(payload)) + payload


def recv_exact(sock, n):
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise ConnectionError("eof")
        buf += chunk
    return buf


class AgentConn:
    """Host end of the guest agent's vsock exec channel (Firecracker path)."""

    def __init__(self, sock):
        self.sock = sock
        self.req = 0
        # The agent announces itself with two text lines before framing starts.
        buf = b""
        while buf.count(b"\n") < 2:
            chunk = sock.recv(256)
            if not chunk:
                raise ConnectionError("agent closed before ready")
            buf += chunk

    def exec(self, command):
        self.req += 1
        self.sock.sendall(agent_frame(self.req, command))
        out = b""
        while True:
            hdr = recv_exact(self.sock, 10)
            assert hdr[:4] == b"VEX2", hdr
            kind = hdr[5]
            (ln,) = struct.unpack(">I", hdr[6:10])
            body = recv_exact(self.sock, ln) if ln else b""
            if kind == 3:
                out += body[8:]
            elif kind == 5:
                (code,) = struct.unpack(">i", body[8:12])
                return code, out
            elif kind == 6:
                raise RuntimeError("agent error: %r" % body[8:])


class AgentListener:
    def __init__(self, path):
        self.path = path
        if os.path.exists(path):
            os.unlink(path)
        self.lsock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.lsock.bind(path)
        self.lsock.listen(4)

    def accept(self, timeout=30.0):
        self.lsock.settimeout(timeout)
        c, _ = self.lsock.accept()
        c.settimeout(timeout)
        return AgentConn(c)

    def close(self):
        self.lsock.close()
        try:
            os.unlink(self.path)
        except FileNotFoundError:
            pass


# ---------------------------------------------------------------- tarit
class Tarit:
    name = "tarit"

    def __init__(self, vmm, kernel, rootfs, mem, vcpus):
        self.vmm, self.kernel, self.rootfs, self.mem, self.vcpus = vmm, kernel, rootfs, mem, vcpus
        self.proc = None

    def _call(self, req, timeout=60.0):
        end = now() + 2.0
        while True:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.settimeout(timeout)
            try:
                # The socket path exists between bind() and listen().
                s.connect(self.sock)
                break
            except ConnectionRefusedError:
                s.close()
                if now() > end:
                    raise
                time.sleep(0.001)
        body = json.dumps(req).encode()
        s.sendall(struct.pack(">I", len(body)) + body)
        (n,) = struct.unpack(">I", recv_exact(s, 4))
        resp = json.loads(recv_exact(s, n))
        s.close()
        return resp

    def _spawn(self, tag):
        self.sock = os.path.join(RUN, "t-%s.sock" % tag)
        if os.path.exists(self.sock):
            os.unlink(self.sock)
        self.log = open(os.path.join(RUN, "t-%s.log" % tag), "wb")
        args = [self.vmm] + (["-" + "v" * VERBOSE] if VERBOSE else []) + ["serve", "--socket", self.sock]
        if GDB:
            args = ["gdb", "-q", "-batch", "-ex", "set pagination off",
                    "-ex", "handle SIGPIPE SIGUSR1 SIGUSR2 SIGCHLD nostop noprint pass",
                    "-ex", "run", "-ex", "thread apply all bt 40", "-ex", "kill", "--args"] + args
        elif os.environ.get("VMBENCH_STRACE"):
            stacks = ["-k"] if os.environ.get("VMBENCH_STRACE_K") else []
            args = ["strace", "-f", "-tt", "-T"] + stacks + ["-e", "trace=" + os.environ["VMBENCH_STRACE"],
                    "-e", "signal=SIGSYS", "-o", os.path.join(RUN, "t-%s.strace" % tag)] + args
        self.proc = subprocess.Popen(args, stdout=self.log, stderr=subprocess.STDOUT)
        wait_path(self.sock, timeout=120 if GDB else 5)

    def dump_threads(self):
        """GDB mode: interrupt the inferior so gdb prints every thread's stack."""
        if not GDB or self.proc is None:
            return
        try:
            with open("/proc/%d/task/%d/children" % (self.proc.pid, self.proc.pid)) as f:
                kids = [int(x) for x in f.read().split()]
        except OSError:
            kids = []
        for k in kids:
            os.kill(k, signal.SIGTRAP)
        try:
            self.proc.wait(timeout=60)
        except subprocess.TimeoutExpired:
            self.proc.kill()
        self.proc = None

    def boot(self, tag, disk):
        self._spawn(tag)
        cfg = {"op": "create", "config": {
            "kernel": {"path": self.kernel, "cmdline": "root=/dev/vda rw " + CMDLINE, "initramfs": None},
            "memory": {"size_mib": self.mem}, "vcpus": {"count": self.vcpus},
            "volumes": [{"path": disk, "read_only": False, "overlay": None, "inherited_fd": None}],
            "net": []}}
        r = self._call(cfg)
        assert r.get("status") == "ok", r

    def exec(self, command, timeout_ms=10000):
        r = self._call({"op": "exec", "command": command, "timeout_ms": timeout_ms})
        if r.get("status") != "exec":
            raise RuntimeError(r)
        return r["exit_code"], r.get("stdout", "")

    def wait_exec(self, command="true", deadline=30.0):
        end = now() + deadline
        while True:
            try:
                code, _ = self.exec(command, timeout_ms=2000)
                if code == 0:
                    return
            except (RuntimeError, ConnectionError, socket.timeout):
                pass
            if now() > end:
                raise TimeoutError("tarit exec never succeeded")

    def diff_snapshot(self, out_dir, step):
        out = subprocess.run([self.vmm, "--socket", self.sock, "snapshot", "--diff"],
                             capture_output=True, text=True, timeout=120)
        r = json.loads(out.stdout)
        assert r.get("status") == "snapshot", (r, out.stderr)
        RELEASED.extend(r.get("volume_paths", []))
        dest = os.path.join(out_dir, "tarit-d%d.snap" % step)
        os.rename(r["path"], dest)
        return dest

    def snapshot(self, out_dir):
        # The CLI also releases the scratch file to the caller (it needs the
        # file's birth time, which Python 3.10 cannot read); an unreleased
        # snapshot is deleted with the VMM's runtime dir when the VMM exits.
        out = subprocess.run([self.vmm, "--socket", self.sock, "snapshot"],
                             capture_output=True, text=True, timeout=120)
        r = json.loads(out.stdout)
        assert r.get("status") == "snapshot", (r, out.stderr)
        RELEASED.extend([r["path"]] + r.get("volume_paths", []))
        return r

    def restore(self, tag, snap, policy="auto"):
        self._spawn(tag)
        req = {"op": "restore", "snapshot_path": snap}
        if policy != "auto":
            req["memory_policy"] = policy
        r = self._call(req, timeout=120)
        assert r.get("status") in ("restored", "ok"), r

    def pid(self):
        return self.proc.pid

    def stop(self):
        if self.proc is None:
            return
        try:
            self._call({"op": "stop"}, timeout=5)
        except Exception:
            pass
        # `serve` keeps listening after a stop; SIGTERM shuts it down cleanly.
        self.proc.terminate()
        try:
            self.proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
        self.proc = None


# ---------------------------------------------------------------- firecracker
class UHTTP(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=60)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(self.path)


class Firecracker:
    name = "firecracker"

    def __init__(self, fc, kernel, rootfs, mem, vcpus):
        self.fc, self.kernel, self.rootfs, self.mem, self.vcpus = fc, kernel, rootfs, mem, vcpus
        self.proc = None
        self.agent = None
        self.listener = None

    def _api(self, method, path, body=None):
        # The socket path exists between bind() and listen(); retry a refused
        # connect exactly like the tarit client does.
        end = now() + 2.0
        while True:
            c = UHTTP(self.api)
            try:
                c.request(method, path, json.dumps(body) if body is not None else None,
                          {"Content-Type": "application/json"})
                break
            except ConnectionRefusedError:
                c.close()
                if now() > end:
                    raise
                time.sleep(0.001)
        r = c.getresponse()
        data = r.read()
        c.close()
        if r.status >= 300:
            raise RuntimeError("%s %s -> %d %s" % (method, path, r.status, data))
        return data

    def _spawn(self, tag, config=None):
        self.api = os.path.join(RUN, "f-%s.api" % tag)
        self.vsock = os.path.join(RUN, "f-%s.vsock" % tag)
        for p in (self.api, self.vsock):
            if os.path.exists(p):
                os.unlink(p)
        self.listener = AgentListener(self.vsock + "_1024")
        self.log = open(os.path.join(RUN, "f-%s.log" % tag), "wb")
        args = [self.fc, "--api-sock", self.api, "--level", "Warning"]
        if config:
            args += ["--config-file", config]
        self.proc = subprocess.Popen(args, stdout=self.log, stderr=subprocess.STDOUT)

    def boot(self, tag, disk):
        cfg = {
            "boot-source": {"kernel_image_path": self.kernel, "boot_args": CMDLINE},
            "drives": [{"drive_id": "rootfs", "path_on_host": disk, "is_root_device": True,
                        "is_read_only": False, "cache_type": FC_CACHE_TYPE}],
            "machine-config": {"vcpu_count": self.vcpus, "mem_size_mib": self.mem},
            "vsock": {"guest_cid": 3, "uds_path": None},
        }
        cfg_path = os.path.join(RUN, "f-%s.json" % tag)
        # uds_path is filled after _spawn picks names; write config then launch.
        self.api = os.path.join(RUN, "f-%s.api" % tag)
        cfg["vsock"]["uds_path"] = os.path.join(RUN, "f-%s.vsock" % tag)
        with open(cfg_path, "w") as f:
            json.dump(cfg, f)
        self._spawn(tag, cfg_path)

    def wait_exec(self, command="true", deadline=30.0):
        self.agent = self.listener.accept(timeout=deadline)
        code, _ = self.agent.exec(command)
        assert code == 0

    def exec(self, command, timeout_ms=10000):
        return self.agent.exec(command)

    def snapshot(self, out_dir):
        snap = os.path.join(out_dir, "fc.snap")
        mem = os.path.join(out_dir, "fc.mem")
        self._api("PATCH", "/vm", {"state": "Paused"})
        self._api("PUT", "/snapshot/create", {"snapshot_type": "Full", "snapshot_path": snap,
                                              "mem_file_path": mem})
        return {"snapshot_path": snap, "mem_path": mem}

    def diff_snapshot(self, out_dir, step, prev):
        """Diff snapshot, then merge it into the previous memory file with
        rebase-snap: Firecracker cannot load a diff chain directly. The
        in-place merge is the cheapest option and overwrites the older
        checkpoint's memory."""
        snap = os.path.join(out_dir, "fc-d%d.vmstate" % step)
        diff_mem = os.path.join(out_dir, "fc-d%d.diffmem" % step)
        self._api("PATCH", "/vm", {"state": "Paused"})
        self._api("PUT", "/snapshot/create", {"snapshot_type": "Diff", "snapshot_path": snap,
                                              "mem_file_path": diff_mem})
        rebase = os.path.join(os.path.dirname(self.fc), os.path.basename(self.fc).replace(
            "firecracker-", "rebase-snap-"))
        subprocess.run([rebase, "--base-file", prev["mem_path"], "--diff-file", diff_mem],
                       check=True, capture_output=True)
        os.unlink(diff_mem)
        return {"snapshot_path": snap, "mem_path": prev["mem_path"], "vsock": prev["vsock"],
                "track_dirty_pages": True, "diff_bytes": 0}

    def restore(self, tag, snap, policy="auto"):
        # The snapshot pins the original vsock uds_path; listen there.
        self.api = os.path.join(RUN, "f-%s.api" % tag)
        for p in (self.api, snap["vsock"]):
            if os.path.exists(p):
                os.unlink(p)
        self.listener = AgentListener(snap["vsock"] + "_1024")
        self.log = open(os.path.join(RUN, "f-%s.log" % tag), "wb")
        self.proc = subprocess.Popen([self.fc, "--api-sock", self.api, "--level", "Warning"],
                                     stdout=self.log, stderr=subprocess.STDOUT)
        wait_path(self.api)
        self._api("PUT", "/snapshot/load", {
            "snapshot_path": snap["snapshot_path"],
            "mem_backend": {"backend_type": "File", "backend_path": snap["mem_path"]},
            "track_dirty_pages": bool(snap.get("track_dirty_pages")),
            "resume_vm": True})

    def pid(self):
        return self.proc.pid

    def stop(self):
        if self.agent:
            try:
                self.agent.sock.close()
            except Exception:
                pass
            self.agent = None
        if self.proc is not None:
            self.proc.kill()
            self.proc.wait()
            self.proc = None
        if self.listener:
            self.listener.close()
            self.listener = None


# ---------------------------------------------------------------- scenarios
def make(kind, a):
    if kind == "tarit":
        return Tarit(a.tarit, a.kernel, a.rootfs, a.mem, a.vcpus)
    return Firecracker(a.firecracker, a.kernel, a.rootfs, a.mem, a.vcpus)


def scen_cold(kind, a, work):
    """Spawn VMM -> guest booted -> first exec of `true` returns 0."""
    tti, mem = [], []
    for i in range(a.iters):
        disk = os.path.join(work, "%s-cold-%d.ext4" % (kind, i))
        fresh_rootfs(a.rootfs, disk)
        vm = make(kind, a)
        t0 = now()
        vm.boot("cold%d" % i, disk)
        vm.wait_exec()
        tti.append((now() - t0) * 1000)
        mem.append(pss_kib(vm.pid()).get("Pss", 0) / 1024.0)
        vm.stop()
        os.unlink(disk)
    return {"time_to_interactive_ms": summary(tti), "vmm_pss_mib_after_boot": summary(mem)}


def scen_exec(kind, a, work):
    """Round-trip latency of exec on an already-running VM."""
    disk = os.path.join(work, "%s-exec.ext4" % kind)
    fresh_rootfs(a.rootfs, disk)
    vm = make(kind, a)
    vm.boot("exec", disk)
    vm.wait_exec()
    res = {}
    for label, cmd, n in (("true", "true", a.exec_iters),
                          ("echo_4k", "head -c 4096 /dev/zero | tr '\\0' a", a.exec_iters),
                          ("python3", "python3 -c 'print(1)'", max(10, a.exec_iters // 10))):
        xs = []
        for _ in range(n):
            t0 = now()
            code, _ = vm.exec(cmd)
            xs.append((now() - t0) * 1000)
            assert code == 0
        res[label + "_ms"] = summary(xs)
    vm.stop()
    os.unlink(disk)
    return res


def scen_snapshot(kind, a, work):
    """Full snapshot of a booted VM, then restore -> first exec, repeatedly."""
    disk = os.path.join(work, "%s-snap.ext4" % kind)
    fresh_rootfs(a.rootfs, disk)
    snapdir = os.path.join(work, "%s-snapdir" % kind)
    os.makedirs(snapdir, exist_ok=True)
    vm = make(kind, a)
    vm.boot("snapsrc", disk)
    vm.wait_exec()
    # Touch some guest memory so the snapshot is not trivially empty.
    vm.exec("python3 -c 'b=bytearray(64<<20)'")
    t0 = now()
    snap = vm.snapshot(snapdir)
    snap_ms = (now() - t0) * 1000
    if kind == "firecracker":
        snap["vsock"] = vm.vsock
        vm.stop()
        snap_arg = snap
    else:
        snap_arg = snap["path"] if "path" in snap else snap.get("snapshot_path")
        vm.stop()
    xs, mem = [], []
    for i in range(a.iters):
        vm = make(kind, a)
        t0 = now()
        vm.restore("rest%d" % i, snap_arg)
        vm.wait_exec()
        xs.append((now() - t0) * 1000)
        mem.append(pss_kib(vm.pid()).get("Pss", 0) / 1024.0)
        vm.stop()
    shutil.rmtree(snapdir, ignore_errors=True)
    os.unlink(disk)
    return {"snapshot_create_ms": snap_ms, "restore_to_exec_ms": summary(xs),
            "vmm_pss_mib_after_restore": summary(mem)}


def scen_burst(kind, a, work):
    """Boot N VMs concurrently; time until every one has run an exec."""
    disks = []
    for i in range(a.burst):
        d = os.path.join(work, "%s-burst-%d.ext4" % (kind, i))
        fresh_rootfs(a.rootfs, d)
        disks.append(d)
    vms = [make(kind, a) for _ in range(a.burst)]
    per = [None] * a.burst
    errs = []
    t0 = now()

    def one(i):
        try:
            vms[i].boot("burst%d" % i, disks[i])
            vms[i].wait_exec(deadline=60)
            per[i] = (now() - t0) * 1000
        except Exception as e:  # noqa: BLE001
            errs.append(repr(e))

    th = [threading.Thread(target=one, args=(i,)) for i in range(a.burst)]
    for t in th:
        t.start()
    for t in th:
        t.join()
    total = (now() - t0) * 1000
    pss = sum(pss_kib(v.pid()).get("Pss", 0) for v in vms if v.proc) / 1024.0
    for v in vms:
        v.stop()
    for d in disks:
        os.unlink(d)
    ok = [x for x in per if x is not None]
    return {"n": a.burst, "ok": len(ok), "errors": errs[:3], "all_ready_ms": total,
            "per_vm_ready_ms": summary(ok), "total_vmm_pss_mib": pss}


def scen_reliab(kind, a, work):
    """Rounds of concurrent boots; count VMs whose first exec never succeeds."""
    fails, total, failed_tags = 0, 0, []
    for r in range(a.rounds):
        disks, vms, tags = [], [], []
        for i in range(a.burst):
            d = os.path.join(work, "%s-rel-%d.ext4" % (kind, i))
            fresh_rootfs(a.rootfs, d)
            disks.append(d)
            vms.append(make(kind, a))
            tags.append("rel%d_%d" % (r, i))
        ok = [False] * a.burst

        def one(i):
            try:
                vms[i].boot(tags[i], disks[i])
                vms[i].wait_exec(deadline=a.deadline)
                ok[i] = True
            except Exception:  # noqa: BLE001
                pass

        th = [threading.Thread(target=one, args=(i,)) for i in range(a.burst)]
        for t in th:
            t.start()
        for t in th:
            t.join()
        for i, v in enumerate(vms):
            if not ok[i] and hasattr(v, "dump_threads"):
                v.dump_threads()
            v.stop()
            total += 1
            if not ok[i]:
                fails += 1
                failed_tags.append(tags[i])
        for d in disks:
            os.unlink(d)
    return {"vms": total, "exec_dead": fails, "failed": failed_tags[:20]}


def scen_chain(kind, a, work):
    """The agent loop: restore the latest checkpoint, run a step that adds
    state, take an incremental snapshot, repeat. Every step checks that all
    earlier steps' state survived."""
    chain = os.path.join(work, "%s-chain" % kind)
    shutil.rmtree(chain, ignore_errors=True)
    os.makedirs(chain)
    disk = os.path.join(work, "%s-chain.ext4" % kind)
    fresh_rootfs(a.rootfs, disk)
    vm = make(kind, a)
    vm.boot("chainsrc", disk)
    vm.wait_exec()
    code, _ = vm.exec("mkdir -p /run/w && mount -t tmpfs tmpfs /run/w")
    assert code == 0
    if kind == "tarit":
        r = vm.snapshot(chain)
        prev = os.path.join(chain, "tarit-base.snap")
        os.rename(r["path"], prev)
    else:
        prev = vm.snapshot(chain)
        prev["vsock"] = vm.vsock
        prev["track_dirty_pages"] = True
    vm.stop()
    restore_ms, step_ms, snap_ms, snap_bytes, pss = [], [], [], [], []
    for i in range(a.chain_steps):
        vm = make(kind, a)
        t0 = now()
        vm.restore("chain%d" % i, prev)
        vm.wait_exec()
        t1 = now()
        code, out = vm.exec("head -c %d /dev/urandom > /run/w/s%d && ls /run/w | wc -l"
                            % (a.step_bytes, i))
        out = out.decode() if isinstance(out, bytes) else out
        assert code == 0 and out.strip() == str(i + 1), (code, out)
        t2 = now()
        pss.append(pss_kib(vm.pid()).get("Pss", 0) / 1024.0)
        if kind == "tarit":
            nxt = vm.diff_snapshot(chain, i)
            size = os.stat(nxt).st_blocks * 512
        else:
            nxt = vm.diff_snapshot(chain, i, prev)
            size = os.stat(nxt["snapshot_path"]).st_size + os.stat(nxt["mem_path"]).st_blocks * 512
        t3 = now()
        vm.stop()
        restore_ms.append((t1 - t0) * 1000)
        step_ms.append((t2 - t1) * 1000)
        snap_ms.append((t3 - t2) * 1000)
        snap_bytes.append(size)
        prev = nxt
    shutil.rmtree(chain, ignore_errors=True)
    os.unlink(disk)
    return {"steps": a.chain_steps, "restore_to_exec_ms": summary(restore_ms),
            "incremental_snapshot_ms": summary(snap_ms),
            "checkpoint_bytes_on_disk_last": snap_bytes[-1],
            "restore_ms_by_depth": [round(x, 1) for x in restore_ms],
            "snapshot_ms_by_depth": [round(x, 1) for x in snap_ms],
            "vmm_pss_mib": summary(pss)}


def _verify_clone(vm, i):
    code, out = vm.exec("cat /root/who; ls /root | tr '\\n' ' '; echo c%d > /root/me" % i)
    out = out.decode() if isinstance(out, bytes) else out
    words = out.split()
    # Must see the parent's marker and no sibling's file.
    assert code == 0 and words[:1] == ["parent"] and "me" not in words, (code, out)


def scen_fanout(kind, a, work):
    """Fork N clones concurrently from a stopped parent's snapshot. Each clone
    must see the parent's disk marker, write its own file and see no other
    clone's file. Firecracker clones each need their own copy of the disk and
    a mount namespace mapping it onto the path baked into the snapshot."""
    n = a.burst
    root = os.path.join(work, "%s-fan" % kind)
    shutil.rmtree(root, ignore_errors=True)
    os.makedirs(root, mode=0o700)
    prep_ms, ready, pss, errors = [], [None] * n, [0.0] * n, []
    if kind == "tarit":
        base = os.path.join(root, "base.ext4")
        fresh_rootfs(a.rootfs, base)
        vm = make(kind, a)
        vm._spawn("fanparent")
        r = vm._call({"op": "create", "config": {
            "kernel": {"path": a.kernel, "cmdline": "root=/dev/vda rw " + CMDLINE, "initramfs": None},
            "memory": {"size_mib": a.mem}, "vcpus": {"count": a.vcpus},
            "volumes": [{"path": base, "read_only": False,
                         "overlay": os.path.join(root, "parent.cow"), "inherited_fd": None}],
            "net": []}})
        assert r.get("status") == "ok", r
        vm.wait_exec()
        assert vm.exec("echo parent > /root/who && python3 -c 'import json'")[0] == 0
        t0 = now()
        snap = vm.snapshot(root)
        snap_ms = (now() - t0) * 1000
        vm.stop()
        golden = os.path.join(root, "golden.snap")
        os.rename(snap["path"], golden)
        vms = [make(kind, a) for _ in range(n)]

        def one(i):
            try:
                t = now()
                vms[i].restore("fan%d" % i, golden)
                vms[i].wait_exec()
                _verify_clone(vms[i], i)
                ready[i] = (now() - t) * 1000
                pss[i] = pss_kib(vms[i].pid()).get("Pss", 0) / 1024.0
            except Exception as e:  # noqa: BLE001
                errors.append(repr(e)[:200])
    else:
        slot = os.path.join(RUN, "fcfan-slot")
        os.makedirs(slot, exist_ok=True)
        # Firecracker's API and vsock sockets live in each clone's directory;
        # reach it through a short symlink to stay under the AF_UNIX limit.
        short = os.path.join(RUN, "fcf")
        if os.path.islink(short):
            os.unlink(short)
        os.symlink(root, short)
        root_disk, root = root, short
        parent_dir = os.path.join(root, "parent")
        os.makedirs(parent_dir)
        fresh_rootfs(a.rootfs, os.path.join(parent_dir, "rootfs.ext4"))
        fc = make(kind, a)

        def launch(vmobj, cdir, config=None):
            for name in ("api.sock", "v.sock"):
                if os.path.exists(os.path.join(cdir, name)):
                    os.unlink(os.path.join(cdir, name))
            vmobj.api = os.path.join(cdir, "api.sock")
            vmobj.listener = AgentListener(os.path.join(cdir, "v.sock_1024"))
            vmobj.log = open(os.path.join(cdir, "fc.log"), "wb")
            cmd = "mount --bind %s %s && exec %s --api-sock %s --level Warning%s" % (
                cdir, slot, vmobj.fc, vmobj.api, (" --config-file " + config) if config else "")
            vmobj.proc = subprocess.Popen(["unshare", "-Urm", "sh", "-c", cmd],
                                          stdout=vmobj.log, stderr=subprocess.STDOUT)
            wait_path(vmobj.api)

        cfg = {
            "boot-source": {"kernel_image_path": a.kernel, "boot_args": CMDLINE},
            "drives": [{"drive_id": "rootfs", "path_on_host": os.path.join(slot, "rootfs.ext4"),
                        "is_root_device": True, "is_read_only": False, "cache_type": FC_CACHE_TYPE}],
            "machine-config": {"vcpu_count": a.vcpus, "mem_size_mib": a.mem},
            "vsock": {"guest_cid": 3, "uds_path": os.path.join(slot, "v.sock")},
        }
        cfg_path = os.path.join(root, "parent.json")
        with open(cfg_path, "w") as f:
            json.dump(cfg, f)
        launch(fc, parent_dir, cfg_path)
        fc.wait_exec()
        assert fc.exec("echo parent > /root/who && python3 -c 'import json' && sync")[0] == 0
        t0 = now()
        snapf = os.path.join(root, "golden.vmstate")
        memf = os.path.join(root, "golden.mem")
        fc._api("PATCH", "/vm", {"state": "Paused"})
        fc._api("PUT", "/snapshot/create", {"snapshot_type": "Full", "snapshot_path": snapf,
                                            "mem_file_path": memf})
        snap_ms = (now() - t0) * 1000
        fc.stop()
        vms = [make(kind, a) for _ in range(n)]
        dirs = []
        for i in range(n):
            cdir = os.path.join(root, "c%d" % i)
            os.makedirs(cdir)
            t = now()
            fresh_rootfs(os.path.join(parent_dir, "rootfs.ext4"), os.path.join(cdir, "rootfs.ext4"))
            prep_ms.append((now() - t) * 1000)
            dirs.append(cdir)

        def one(i):
            try:
                t = now()
                launch(vms[i], dirs[i])
                vms[i]._api("PUT", "/snapshot/load", {
                    "snapshot_path": snapf,
                    "mem_backend": {"backend_type": "File", "backend_path": memf},
                    "resume_vm": True})
                vms[i].wait_exec()
                _verify_clone(vms[i], i)
                ready[i] = (now() - t) * 1000
                pss[i] = pss_kib(vms[i].pid()).get("Pss", 0) / 1024.0
            except Exception as e:  # noqa: BLE001
                errors.append(repr(e)[:200])

    th = [threading.Thread(target=one, args=(i,)) for i in range(n)]
    t0 = now()
    for t in th:
        t.start()
    for t in th:
        t.join()
    all_ready = (now() - t0) * 1000
    for v in vms:
        v.stop()
    if kind != "tarit":
        os.unlink(root)
        root = root_disk
    disk_bytes = sum(os.stat(os.path.join(dp, f)).st_blocks * 512
                     for dp, _, fs in os.walk(root) for f in fs)
    shutil.rmtree(root, ignore_errors=True)
    ok = [x for x in ready if x is not None]
    return {"n": n, "ok": len(ok), "errors": errors[:3], "snapshot_ms": snap_ms,
            "all_ready_ms": all_ready, "per_clone_ready_ms": summary(ok),
            "per_clone_disk_prep_ms": summary(prep_ms) if prep_ms else None,
            "disk_prep_total_ms": sum(prep_ms),
            "total_pss_mib": sum(pss), "disk_used_mib": disk_bytes / 2**20}


SCEN = {"fanout": scen_fanout, "chain": scen_chain, "cold": scen_cold, "exec": scen_exec, "snapshot": scen_snapshot, "burst": scen_burst,
        "reliab": scen_reliab}


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--tarit", required=True)
    p.add_argument("--firecracker", required=True)
    p.add_argument("--kernel", required=True)
    p.add_argument("--rootfs", required=True)
    p.add_argument("--mem", type=int, default=256)
    p.add_argument("--vcpus", type=int, default=1)
    p.add_argument("--iters", type=int, default=20)
    p.add_argument("--exec-iters", type=int, default=200)
    p.add_argument("--burst", type=int, default=20)
    p.add_argument("--rounds", type=int, default=5)
    p.add_argument("--chain-steps", type=int, default=10)
    p.add_argument("--step-bytes", type=int, default=8 << 20)
    p.add_argument("--deadline", type=float, default=10.0)
    p.add_argument("--work", required=True)
    p.add_argument("--only", default="tarit,firecracker")
    p.add_argument("--scenarios", default="cold,exec,snapshot,burst")
    p.add_argument("--out", required=True)
    a = p.parse_args()
    os.makedirs(RUN, exist_ok=True)
    os.makedirs(a.work, exist_ok=True)
    results = {"host": os.uname().nodename, "kernel": a.kernel, "mem": a.mem, "vcpus": a.vcpus,
               "firecracker_cache_type": FC_CACHE_TYPE}
    kinds = a.only.split(",")
    for kind in kinds:
        results[kind] = {}
    # Alternate the VMMs scenario by scenario so a change in host load during
    # a long run lands on both sides instead of on whichever ran last.
    for s in a.scenarios.split(","):
        for kind in kinds:
            print("== %s %s" % (kind, s), file=sys.stderr, flush=True)
            try:
                try:
                    results[kind][s] = SCEN[s](kind, a, a.work)
                finally:
                    remove_released()
            except Exception as e:  # noqa: BLE001
                results[kind][s] = {"error": repr(e)}
            print(json.dumps(results[kind][s]), file=sys.stderr, flush=True)
            with open(a.out, "w") as f:
                json.dump(results, f, indent=2)


if __name__ == "__main__":
    main()
