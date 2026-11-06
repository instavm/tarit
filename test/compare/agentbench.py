#!/usr/bin/env python3
"""Agent-sandbox workloads on tarit and Firecracker, side by side.

Each scenario drives both VMMs through the same guest image, kernel, command
line and guest agent, records latency, and checks correctness: a scenario is
only as good as the checks it passes. Networking is real: every VM gets its
own TAP in a rootless user+network namespace (the "lab") whose traffic is
NAT-ed to the internet through slirp4netns, so no host configuration changes
are needed and both VMMs see identical networks.

Scenarios (see README.md for what each verifies):
  session  coding-agent session: egress, pip install, git clone, tests, build
  resume   repeated snapshot -> stop -> restore with live processes and servers
  fork     N clones of one snapshot: uniqueness, isolation, networking
  burst    N sandboxes restored at once, each running many short commands
  disk     many small files, archives, a compile, large sequential writes
  soak     sustained exec load with pause/resume, watching for resource growth
"""
import argparse, hashlib, json, os, random, shutil, signal, socket, subprocess, sys
import tempfile, threading, time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import vmbench as vb  # noqa: E402  (shared agent protocol and helpers)

GUEST_BOOT = (vb.CMDLINE + " root=/dev/vda rw init=/usr/sbin/vmm-agent")
LAB_DNS = "10.0.2.3"          # slirp4netns resolver
HOST_HTTP_PORT = 8080         # lab-side HTTP server guests can reach
BULK_HTTP_PORT = 8090         # lab-side static server for bulk transfers
BULK_MIB = 256
NAME_RETRY = 3


def now():
    return time.perf_counter()


def sh(cmd, check=True, **kw):
    return subprocess.run(cmd, check=check, capture_output=True, text=True, **kw)


# ------------------------------------------------------------------ the lab
class Lab:
    """Rootless user+network namespace with internet egress via slirp4netns.

    Slot N has TAP `tapN` with host address 172.16.N.1/24; its guest uses
    172.16.N.2. A small HTTP server in the lab answers guests on
    172.16.N.1:8080 and logs which guest asked.
    """

    def __init__(self, work):
        self.work = work
        self.holder = None
        self.slirp = None
        self.http = None
        self.bulk = None
        self.taps = set()

    def start(self):
        self.holder = subprocess.Popen(["unshare", "-Urn", "--fork", "sleep", "infinity"],
                                       start_new_session=True)
        # The namespaced sleep is the holder's child.
        deadline = now() + 5
        self.pid = None
        while self.pid is None and now() < deadline:
            kids = sh(["pgrep", "-P", str(self.holder.pid)], check=False).stdout.split()
            self.pid = int(kids[0]) if kids else None
            time.sleep(0.02)
        if self.pid is None:
            raise RuntimeError("lab namespace did not start")
        self.prefix = ["nsenter", "--preserve-credentials", "-U", "-n", "-t", str(self.pid)]
        self.slirp = subprocess.Popen(
            ["slirp4netns", "--configure", "--mtu=65520", "--disable-host-loopback",
             str(self.pid), "tap-slirp"],
            stdout=open(os.path.join(self.work, "slirp.log"), "wb"), stderr=subprocess.STDOUT)
        # slirp4netns creates tap-slirp asynchronously; until it exists the
        # query fails, so keep polling instead of treating that as an error.
        deadline = now() + 10
        while "10.0.2.100" not in self.run("ip -4 -br addr show tap-slirp", check=False).stdout:
            if now() > deadline:
                raise RuntimeError("slirp4netns did not configure tap-slirp; see slirp.log")
            time.sleep(0.05)
        self.run("sysctl -qw net.ipv4.ip_forward=1")
        self.run("nft add table ip lab && "
                 "nft add chain ip lab post '{ type nat hook postrouting priority 100; }' && "
                 "nft add rule ip lab post oifname tap-slirp masquerade")
        server = os.path.join(os.path.dirname(os.path.abspath(__file__)), "agentbench_http.py")
        self.http_log = os.path.join(self.work, "lab-http.log")
        self.http = subprocess.Popen(self.prefix + [sys.executable, server, str(HOST_HTTP_PORT),
                                                    self.http_log],
                                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        return self

    def run(self, script, check=True):
        return sh(self.prefix + ["sh", "-c", script], check=check)

    def tap(self, slot):
        name = "tap%d" % slot
        if name not in self.taps:
            self.run("ip tuntap add dev {n} mode tap && ip addr add 172.16.{s}.1/24 dev {n} && "
                     "ip link set {n} up".format(n=name, s=slot))
            self.taps.add(name)
        return name

    def bulk_file(self):
        """A BULK_MIB random file served on every lab gateway, and its
        SHA-256. Created and served on first use."""
        if self.bulk is None:
            directory = os.path.join(self.work, "bulk")
            os.makedirs(directory, exist_ok=True)
            digest = hashlib.sha256()
            with open(os.path.join(directory, "bulk.bin"), "wb") as f:
                for _ in range(BULK_MIB):
                    chunk = os.urandom(1 << 20)
                    digest.update(chunk)
                    f.write(chunk)
            server = subprocess.Popen(
                self.prefix + [sys.executable, "-m", "http.server", str(BULK_HTTP_PORT),
                               "--bind", "0.0.0.0", "--directory", directory],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            self.run("ip link set lo up")
            deadline = now() + 10
            while self.run("curl -sf -o /dev/null -r 0-0 http://127.0.0.1:%d/bulk.bin" % BULK_HTTP_PORT,
                           check=False).returncode != 0:
                if now() > deadline:
                    server.kill()
                    raise RuntimeError("bulk file server did not start")
                time.sleep(0.05)
            self.bulk = (server, digest.hexdigest())
        return self.bulk[1]

    def http_hits(self, marker):
        try:
            with open(self.http_log) as f:
                return sum(1 for line in f if marker in line)
        except FileNotFoundError:
            return 0

    def stop(self):
        for p in (self.http, self.slirp, self.bulk[0] if self.bulk else None):
            if p and p.poll() is None:
                p.kill()
                p.wait()
        if self.holder and self.holder.poll() is None:
            os.killpg(self.holder.pid, signal.SIGKILL)
            self.holder.wait()


def guest_ip(slot):
    return "172.16.%d.2" % slot


# ---------------------------------------------------------------- VM adapters
def stops_on_failure(method):
    """A boot or restore that fails must not leave its VMM running."""
    def wrapper(self, *args, **kwargs):
        try:
            return method(self, *args, **kwargs)
        except BaseException:
            self.stop()
            raise
    return wrapper


class TaritVM:
    kind = "tarit"

    def __init__(self, a, lab, tag):
        self.a, self.lab, self.tag = a, lab, tag
        self.sock = os.path.join(vb.RUN, "a-%s.sock" % tag)
        self.proc = None

    def _spawn(self):
        if os.path.exists(self.sock):
            os.unlink(self.sock)
        self.log = open(os.path.join(self.a.work, "t-%s.log" % self.tag), "wb")
        self.proc = subprocess.Popen(self.lab.prefix + [self.a.tarit, "serve", "--socket", self.sock],
                                     stdout=self.log, stderr=subprocess.STDOUT)
        vb.wait_path(self.sock, 10)

    def _call(self, req, timeout=120.0):
        t = vb.Tarit(self.a.tarit, None, None, 0, 0)
        t.sock = self.sock
        return t._call(req, timeout)

    @stops_on_failure
    def boot(self, slot, mem):
        self._spawn()
        overlay_dir = os.path.join(self.a.work, "overlays")
        os.makedirs(overlay_dir, mode=0o700, exist_ok=True)
        net = [{"tap": self.lab.tap(slot), "guest_ip": guest_ip(slot)}]
        r = self._call({"op": "create", "config": {
            "kernel": {"path": self.a.kernel, "initramfs": None,
                       "cmdline": GUEST_BOOT + " tarit.dns=" + LAB_DNS},
            "memory": {"size_mib": mem}, "vcpus": {"count": self.a.vcpus},
            "volumes": [{"path": self.a.rootfs, "read_only": False, "inherited_fd": None,
                         "overlay": os.path.join(overlay_dir, "%s.cow" % self.tag)}],
            "net": net}})
        assert r.get("status") == "ok", r
        self.slot = slot
        self.wait_ready()

    def wait_ready(self, deadline=60.0):
        end = now() + deadline
        while True:
            try:
                if self.exec("true", 2000)[0] == 0:
                    return
            except Exception:  # noqa: BLE001
                pass
            if now() > end:
                raise TimeoutError("%s: guest agent never answered" % self.tag)
            time.sleep(0.005)

    def exec(self, command, timeout_ms=60000):
        r = self._call({"op": "exec", "command": command, "timeout_ms": timeout_ms},
                       timeout=timeout_ms / 1000 + 30)
        if r.get("status") != "exec":
            raise RuntimeError(r)
        return r["exit_code"], r.get("stdout", "")

    def pause(self):
        assert self._call({"op": "pause"}).get("status") in ("ok", "paused"), "pause"

    def resume(self):
        assert self._call({"op": "resume"}).get("status") in ("ok", "resumed"), "resume"

    def snapshot(self, out_dir):
        out = sh([self.a.tarit, "--socket", self.sock, "snapshot"], timeout=300)
        r = json.loads(out.stdout)
        assert r.get("status") == "snapshot", (r, out.stderr)
        keep = os.path.join(out_dir, "%s.snap" % self.tag)
        os.rename(r["path"], keep)
        # Captured disks are found beside a moved snapshot.
        disks = []
        for path in r.get("volume_paths", []):
            dest = os.path.join(out_dir, os.path.basename(path))
            os.rename(path, dest)
            disks.append(dest)
        return {"path": keep, "disks": disks}

    @stops_on_failure
    def restore(self, snap, slot):
        self._spawn()
        r = self._call({"op": "restore", "snapshot_path": snap["path"],
                        "net": [{"tap": self.lab.tap(slot), "guest_ip": guest_ip(slot)}]})
        assert r.get("status") in ("restored", "ok"), r
        self.slot = slot
        r = self._call({"op": "repair_guest_network", "network": {
            "addr": guest_ip(slot), "prefix": 24, "gateway": "172.16.%d.1" % slot,
            "dns_servers": [LAB_DNS]}})
        assert r.get("status") in ("ok", "repaired", "guest_network_repaired"), r

    def pid(self):
        return self.proc.pid

    def stop(self):
        if self.proc is None:
            return
        try:
            self._call({"op": "stop"}, timeout=5)
        except Exception:  # noqa: BLE001
            pass
        self.proc.terminate()
        try:
            self.proc.wait(10)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
        self.proc = None


class FirecrackerVM:
    kind = "firecracker"

    def __init__(self, a, lab, tag):
        self.a, self.lab, self.tag = a, lab, tag
        self.dir = os.path.join(a.work, "fc", tag)
        self.proc = None
        self.agent = None
        self.listener = None

    def _launch(self, bind_over=None, config=None):
        os.makedirs(self.dir, exist_ok=True)
        for name in ("api.sock", "v.sock"):
            p = os.path.join(self.dir, name)
            if os.path.exists(p):
                os.unlink(p)
        self.api = os.path.join(self.dir, "api.sock")
        self.listener = vb.AgentListener(os.path.join(self.dir, "v.sock_1024"))
        self.log = open(os.path.join(self.dir, "fc.log"), "wb")
        fc = "%s --api-sock %s --level Warning%s" % (
            self.a.firecracker, self.api, (" --config-file " + config) if config else "")
        # A restored clone reopens the disk path baked into the snapshot;
        # show it this clone's own disk through a private mount namespace.
        script = ("mount --bind %s %s && exec %s" % (self.dir, bind_over, fc)) if bind_over \
            else "exec " + fc
        self.proc = subprocess.Popen(self.lab.prefix + ["unshare", "-m", "sh", "-c", script],
                                     stdout=self.log, stderr=subprocess.STDOUT)
        vb.wait_path(self.api, 10)

    def _api(self, method, path, body=None):
        f = vb.Firecracker(self.a.firecracker, None, None, 0, 0)
        f.api = self.api
        return f._api(method, path, body)

    @stops_on_failure
    def boot(self, slot, mem):
        os.makedirs(self.dir, exist_ok=True)
        disk = os.path.join(self.dir, "rootfs.ext4")
        t = now()
        vb.fresh_rootfs(self.a.rootfs, disk)
        self.prep_ms = (now() - t) * 1000
        ip = "ip=%s::172.16.%d.1:255.255.255.0::eth0:off" % (guest_ip(slot), slot)
        cfg = {
            "boot-source": {"kernel_image_path": self.a.kernel,
                            "boot_args": GUEST_BOOT + " " + ip + " tarit.dns=" + LAB_DNS},
            "drives": [{"drive_id": "rootfs", "path_on_host": disk, "is_root_device": True,
                        "is_read_only": False, "cache_type": vb.FC_CACHE_TYPE}],
            "machine-config": {"vcpu_count": self.a.vcpus, "mem_size_mib": mem},
            "network-interfaces": [{"iface_id": "eth0", "host_dev_name": self.lab.tap(slot),
                                    "guest_mac": "02:fc:00:00:%02x:02" % slot}],
            "vsock": {"guest_cid": 3, "uds_path": os.path.join(self.dir, "v.sock")},
        }
        cfg_path = os.path.join(self.dir, "config.json")
        with open(cfg_path, "w") as f:
            json.dump(cfg, f)
        self._launch(config=cfg_path)
        self.slot = slot
        self.wait_ready()

    def wait_ready(self, deadline=60.0):
        self.agent = self.listener.accept(timeout=deadline)
        assert self.exec("true")[0] == 0

    def exec(self, command, timeout_ms=60000):
        self.agent.sock.settimeout(timeout_ms / 1000 + 30)
        code, out = self.agent.exec(command)
        return code, out.decode(errors="replace")

    def pause(self):
        self._api("PATCH", "/vm", {"state": "Paused"})

    def resume(self):
        self._api("PATCH", "/vm", {"state": "Resumed"})

    def snapshot(self, out_dir):
        """Firecracker leaves the disk to the caller: copy it in the same pause."""
        snap = os.path.join(out_dir, "%s.vmstate" % self.tag)
        mem = os.path.join(out_dir, "%s.mem" % self.tag)
        disk = os.path.join(out_dir, "%s.ext4" % self.tag)
        self.pause()
        self._api("PUT", "/snapshot/create", {"snapshot_type": "Full", "snapshot_path": snap,
                                              "mem_file_path": mem})
        vb.fresh_rootfs(os.path.join(self.dir, "rootfs.ext4"), disk)
        self.resume()
        return {"vmstate": snap, "mem": mem, "disk": disk, "disk_dir": self.dir}

    @stops_on_failure
    def restore(self, snap, slot):
        os.makedirs(self.dir, exist_ok=True)
        t = now()
        vb.fresh_rootfs(snap["disk"], os.path.join(self.dir, "rootfs.ext4"))
        self.prep_ms = (now() - t) * 1000
        self._launch(bind_over=snap["disk_dir"] if snap["disk_dir"] != self.dir else None)
        self._api("PUT", "/snapshot/load", {
            "snapshot_path": snap["vmstate"],
            "mem_backend": {"backend_type": "File", "backend_path": snap["mem"]},
            "network_overrides": [{"iface_id": "eth0", "host_dev_name": self.lab.tap(slot)}],
            "vsock_override": {"uds_path": os.path.join(self.dir, "v.sock")},
            "resume_vm": True})
        self.slot = slot
        self.wait_ready()
        # Firecracker restores the snapshot's guest address; move the clone to
        # its own slot the way a Firecracker operator has to, from inside.
        code, out = self.exec(
            "ip addr flush dev eth0 && ip addr add {ip}/24 dev eth0 && "
            "ip route replace default via 172.16.{s}.1 && ip neigh flush all".format(
                ip=guest_ip(slot), s=slot))
        assert code == 0, out

    def pid(self):
        return self.proc.pid

    def stop(self):
        if self.agent:
            try:
                self.agent.sock.close()
            except Exception:  # noqa: BLE001
                pass
            self.agent = None
        if self.proc is not None:
            self.proc.kill()
            self.proc.wait()
            self.proc = None
        if self.listener:
            self.listener.close()
            self.listener = None


def vm(kind, a, lab, tag):
    return (TaritVM if kind == "tarit" else FirecrackerVM)(a, lab, tag)


# ------------------------------------------------------------------ checks
class Report:
    def __init__(self):
        self.checks = []
        self.metrics = {}

    def check(self, name, ok, detail=""):
        self.checks.append({"check": name, "ok": bool(ok), "detail": str(detail)[:300]})
        return ok

    def metric(self, name, value):
        self.metrics[name] = value

    def to_dict(self):
        return {"checks": self.checks, "metrics": self.metrics,
                "passed": sum(c["ok"] for c in self.checks), "total": len(self.checks)}


def try_exec(v, cmd, timeout_ms=120000):
    try:
        return v.exec(cmd, timeout_ms)
    except Exception as e:  # noqa: BLE001
        return -1, "EXC %r" % e


# A guest-side "REPL": a long-lived python process that keeps state in memory
# and answers requests through a FIFO, like a notebook kernel or agent tool
# server. Its in-memory state must survive snapshots.
REPL_START = r"""mkfifo /run/repl.in 2>/dev/null; cat > /run/repl.py <<'EOF'
import random, uuid, os, json
state = {"counter": 0}
while True:
    with open("/run/repl.in") as f:
        for line in f:
            cmd = line.strip()
            if cmd == "inc":
                state["counter"] += 1
                out = state["counter"]
            elif cmd == "rand":
                out = [random.random(), str(uuid.uuid4()), os.urandom(8).hex()]
            else:
                out = None
            with open("/run/repl.out.tmp", "w") as o:
                json.dump(out, o)
            os.rename("/run/repl.out.tmp", "/run/repl.out")
EOF
setsid python3 /run/repl.py </dev/null >/dev/null 2>&1 &
sleep 0.3; echo started"""


def repl(v, cmd):
    code, out = try_exec(v, "rm -f /run/repl.out; echo %s > /run/repl.in; "
                            "for i in $(seq 200); do [ -f /run/repl.out ] && break; sleep 0.01; done; "
                            "cat /run/repl.out" % cmd)
    if code != 0:
        return None
    try:
        return json.loads(out)
    except ValueError:
        return None


def ingress(lab, slot, path="/"):
    r = lab.run("curl -s -m 5 -o /dev/null -w '%%{http_code}' http://%s:8000%s" % (guest_ip(slot), path),
                check=False)
    return r.stdout.strip()


def clock_skew(v):
    code, out = try_exec(v, "date +%s.%N")
    if code != 0:
        return None
    return abs(float(out.strip()) - time.time())


# ---------------------------------------------------------------- scenarios
def scen_session(kind, a, lab):
    """A coding-agent session: fetch, install, clone, test, build, serve."""
    rep = Report()
    v = vm(kind, a, lab, "sess")
    t = now()
    v.boot(slot=1, mem=a.mem)
    rep.metric("boot_to_ready_ms", (now() - t) * 1000)
    if kind == "firecracker":
        rep.metric("disk_prep_ms", v.prep_ms)
    try:
        steps = [
            ("egress https", "curl -sS -m 20 -o /dev/null -w '%{http_code}' https://pypi.org/simple/six/",
             lambda o: o.strip() == "200"),
            ("dns + lab gateway", "curl -sS -m 5 http://172.16.1.1:%d/session-%s" % (HOST_HTTP_PORT, kind),
             lambda o: "ok" in o),
            ("pip install", "pip install -q --no-cache-dir --disable-pip-version-check six==1.16.0 "
                            "&& python3 -c 'import six; print(six.__version__)'",
             lambda o: "1.16.0" in o),
            ("git clone", "git clone -q --depth 1 https://github.com/pallets/itsdangerous /root/repo "
                          "&& ls /root/repo/src/itsdangerous | wc -l", lambda o: int(o.strip() or 0) > 3),
            ("install test deps", "cd /root/repo && pip install -q --no-cache-dir --disable-pip-version-check "
                                  "pytest freezegun -e . 2>&1 | tail -1; python3 -c 'import itsdangerous'",
             lambda o: True),
            ("run test suite", "cd /root/repo && python3 -m pytest -q -p no:cacheprovider tests 2>&1 | tail -1",
             lambda o: "passed" in o and "failed" not in o),
            ("compile C", "printf '#include <stdio.h>\\nint main(){long s=0;for(long i=0;i<50000000;i++)s+=i%%7;"
                          "printf(\"%%ld\\\\n\",s);}' > /root/t.c && gcc -O2 /root/t.c -o /root/t && /root/t",
             lambda o: o.strip() == "149999997"),
            ("serve + ingress", "cd /root/repo && (setsid python3 -m http.server 8000 >/dev/null 2>&1 &) ; "
                                "sleep 0.5; curl -s -o /dev/null -w '%{http_code}' localhost:8000/",
             lambda o: o.strip() == "200"),
        ]
        for name, cmd, ok in steps:
            t = now()
            code, out = try_exec(v, cmd, 300000)
            ms = (now() - t) * 1000
            good = code == 0 and ok(out)
            rep.check(name, good, "" if good else "exit=%s out=%r" % (code, out[-200:]))
            rep.metric("step_ms/" + name, ms)
        rep.check("host reaches guest server", ingress(lab, 1) == "200")
        lat = []
        failures = 0
        for i in range(a.short_cmds):
            cmd = random.choice(["ls /root/repo", "grep -rn 'def ' /root/repo/src | wc -l",
                                 "cat /root/repo/README.md | head -3", "python3 -c 'print(1+1)'",
                                 "git -C /root/repo status --short | wc -l", "echo $((%d*3))" % i])
            t = now()
            code, _ = try_exec(v, cmd)
            lat.append((now() - t) * 1000)
            failures += code != 0
        rep.check("%d short agent commands" % a.short_cmds, failures == 0, "%d failed" % failures)
        rep.metric("short_cmd_ms", vb.summary(lat))
    finally:
        v.stop()
    return rep.to_dict()


def scen_resume(kind, a, lab):
    """Checkpoint -> stop -> restore repeatedly, with live state and servers."""
    rep = Report()
    snapdir = os.path.join(a.work, "snaps-resume-%s" % kind)
    os.makedirs(snapdir, exist_ok=True)
    v = vm(kind, a, lab, "res0")
    v.boot(slot=2, mem=a.mem)
    try_exec(v, REPL_START)
    try_exec(v, "(setsid python3 -m http.server 8000 --directory /etc >/dev/null 2>&1 &); "
                "echo gen0 > /root/gen; for i in $(seq 100); do "
                "curl -s -o /dev/null localhost:8000/ && break; sleep 0.05; done")
    expect = repl(v, "inc")
    rep.check("repl starts", expect == 1, expect)
    rep.check("guest server reachable before any snapshot", ingress(lab, 2, "/hostname") == "200")
    snap_ms, restore_ms, skews = [], [], []
    failures = []
    for i in range(1, a.cycles + 1):
        t = now()
        try:
            snap = v.snapshot(snapdir)
        except Exception as e:  # noqa: BLE001
            failures.append("cycle %d snapshot: %r" % (i, e))
            break
        snap_ms.append((now() - t) * 1000)
        v.stop()
        v = vm(kind, a, lab, "res%d" % i)
        t = now()
        try:
            v.restore(snap, slot=2)
        except Exception as e:  # noqa: BLE001
            failures.append("cycle %d restore: %r" % (i, e))
            break
        restore_ms.append((now() - t) * 1000)
        skew = clock_skew(v)
        if skew is not None:
            skews.append(skew)
        got = repl(v, "inc")
        if got != expect + 1:
            failures.append("cycle %d repl counter %r != %r" % (i, got, expect + 1))
        expect = got if isinstance(got, int) else expect + 1
        code, out = try_exec(v, "cat /root/gen && echo gen%d > /root/gen" % i)
        if code != 0 or out.strip() != "gen%d" % (i - 1):
            failures.append("cycle %d disk state %r" % (i, out.strip()))
        code = ingress(lab, 2, "/hostname")
        if code != "200":
            failures.append("cycle %d guest server from host: http %s" % (i, code))
        code, out = try_exec(v, "curl -s -m 5 http://172.16.2.1:%d/resume-%s-%d" % (HOST_HTTP_PORT, kind, i))
        if "ok" not in out:
            failures.append("cycle %d guest->host network %r" % (i, out[-80:]))
        for f in os.listdir(snapdir):
            os.unlink(os.path.join(snapdir, f)) if not f.startswith(v.tag) else None
    rep.check("%d checkpoint/restore cycles with live state" % a.cycles, not failures,
              "; ".join(failures[:3]))
    rep.check("guest clock within 1 s of host after restore",
              skews and max(skews) < 1.0, "max skew %.3fs" % (max(skews) if skews else -1))
    rep.metric("snapshot_ms", vb.summary(snap_ms))
    rep.metric("restore_to_ready_ms", vb.summary(restore_ms))
    rep.metric("max_clock_skew_s", max(skews) if skews else None)
    v.stop()
    shutil.rmtree(snapdir, ignore_errors=True)
    return rep.to_dict()


def scen_fork(kind, a, lab):
    """Fork one warmed sandbox N ways (tree search / parallel attempts)."""
    rep = Report()
    snapdir = os.path.join(a.work, "snaps-fork-%s" % kind)
    os.makedirs(snapdir, exist_ok=True)
    parent = vm(kind, a, lab, "fparent")
    parent.boot(slot=3, mem=a.fork_mem)
    try_exec(parent, REPL_START)
    try_exec(parent, "echo parent > /root/marker && sync")
    repl(parent, "inc")
    t = now()
    snap = parent.snapshot(snapdir)
    rep.metric("snapshot_ms", (now() - t) * 1000)
    parent.stop()
    n = a.forks
    clones = [vm(kind, a, lab, "fork%d" % i) for i in range(n)]
    results = [None] * n
    ready = [None] * n
    prep = [0.0] * n

    def one(i):
        c = clones[i]
        slot = 20 + i
        try:
            t = now()
            c.restore(snap, slot)
            ready[i] = (now() - t) * 1000
            prep[i] = getattr(c, "prep_ms", 0.0)
            info = {}
            info["marker"] = try_exec(c, "cat /root/marker")[1].strip()
            try_exec(c, "echo clone%d > /root/mine && sync" % i)
            info["boot_id"] = try_exec(c, "cat /proc/sys/kernel/random/boot_id")[1].strip()
            info["urandom"] = try_exec(c, "head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \\n'")[1].strip()
            info["uuid"] = try_exec(c, "python3 -c 'import uuid; print(uuid.uuid4())'")[1].strip()
            info["repl"] = repl(c, "rand")
            info["egress"] = try_exec(c, "curl -s -m 5 http://172.16.%d.1:%d/fork-%s-%d"
                                         % (slot, HOST_HTTP_PORT, kind, i))[1].strip()
            info["ingress_ok"] = lab.run("ping -c1 -W2 %s" % guest_ip(slot), check=False).returncode == 0
            results[i] = info
        except Exception as e:  # noqa: BLE001
            results[i] = {"error": repr(e)[:200]}

    threads = [threading.Thread(target=one, args=(i,)) for i in range(n)]
    t0 = now()
    for th in threads:
        th.start()
    for th in threads:
        th.join()
    all_ready = (now() - t0) * 1000
    ok = [r for r in results if r and "error" not in r]
    rep.check("%d clones restored" % n, len(ok) == n,
              "; ".join(r["error"] for r in results if r and "error" in r)[:300])
    rep.check("clones see parent's disk", all(r["marker"] == "parent" for r in ok))
    others = []
    for i, c in enumerate(clones):
        if results[i] and "error" not in results[i]:
            code, out = try_exec(c, "cat /root/mine")
            others.append(out.strip() == "clone%d" % i)
    rep.check("clone disks are isolated", others and all(others))
    for key, label in (("boot_id", "boot_id unique per clone"),
                       ("urandom", "kernel RNG output unique per clone"),
                       ("uuid", "uuid4 in a new process unique per clone")):
        vals = [r[key] for r in ok]
        rep.check(label, len(set(vals)) == len(vals) and all(vals), "%d distinct of %d" % (len(set(vals)), len(vals)))
    pre = [json.dumps(r["repl"]) for r in ok if r["repl"]]
    rep.metric("pre_snapshot_process_rng_distinct", "%d of %d" % (len(set(pre)), len(pre)))
    rep.check("each clone reaches the lab network", all("ok" in r["egress"] for r in ok))
    rep.check("each clone reachable at its own address", all(r["ingress_ok"] for r in ok))
    rep.check("lab saw every clone", all(lab.http_hits("fork-%s-%d" % (kind, i)) >= 1 for i in range(n)))
    rep.metric("all_ready_ms", all_ready)
    rep.metric("per_clone_ready_ms", vb.summary([x for x in ready if x is not None]))
    rep.metric("per_clone_disk_prep_ms", vb.summary(prep))
    pss = [vb.pss_kib(c.pid()).get("Pss", 0) / 1024.0 for c in clones if c.proc]
    rep.metric("total_vmm_pss_mib", sum(pss))
    for c in clones:
        c.stop()
    shutil.rmtree(snapdir, ignore_errors=True)
    shutil.rmtree(os.path.join(a.work, "fc"), ignore_errors=True)
    return rep.to_dict()


def scen_burst(kind, a, lab):
    """Many sandboxes restored at once, each running a code-interpreter burst."""
    rep = Report()
    snapdir = os.path.join(a.work, "snaps-burst-%s" % kind)
    os.makedirs(snapdir, exist_ok=True)
    tmpl = vm(kind, a, lab, "btmpl")
    tmpl.boot(slot=4, mem=a.fork_mem)
    try_exec(tmpl, "python3 -c 'import json, re, math, statistics'")
    snap = tmpl.snapshot(snapdir)
    tmpl.stop()
    before = leak_probe(kind, a)
    n = a.burst
    sandboxes = [vm(kind, a, lab, "burst%d" % i) for i in range(n)]
    first_cmd = [None] * n
    errors = []

    def one(i):
        s = sandboxes[i]
        try:
            t = now()
            s.restore(snap, 40 + i)
            first_cmd[i] = (now() - t) * 1000
            for j in range(a.burst_cmds):
                code, out = s.exec("python3 -c 'print(sum(range(%d)))'" % (j + 10))
                if code != 0 or out.strip() != str(sum(range(j + 10))):
                    errors.append("sandbox %d cmd %d: %r" % (i, j, out[-80:]))
                    break
        except Exception as e:  # noqa: BLE001
            errors.append("sandbox %d: %r" % (i, e))

    threads = [threading.Thread(target=one, args=(i,)) for i in range(n)]
    t0 = now()
    for th in threads:
        th.start()
    for th in threads:
        th.join()
    rep.metric("all_done_ms", (now() - t0) * 1000)
    rep.check("%d sandboxes x %d commands" % (n, a.burst_cmds), not errors, "; ".join(errors[:3]))
    rep.metric("time_to_first_command_ms", vb.summary([x for x in first_cmd if x is not None]))
    for s in sandboxes:
        s.stop()
    shutil.rmtree(snapdir, ignore_errors=True)
    shutil.rmtree(os.path.join(a.work, "fc"), ignore_errors=True)
    time.sleep(0.5)
    after = leak_probe(kind, a)
    rep.check("nothing left behind after teardown", after == before,
              "before %s after %s" % (before, after))
    return rep.to_dict()


def leak_probe(kind, a):
    procs = sh(["pgrep", "-c", "-x", "vmm" if kind == "tarit" else "firecracker-v1."], check=False)
    # The VMM keeps its runtime files under $TMPDIR, like tempfile does.
    root = os.path.join(tempfile.gettempdir(), ".vmm-runtime")
    runtime = os.listdir(root) if os.path.isdir(root) else []
    overlays = len(os.listdir(os.path.join(a.work, "overlays"))) \
        if os.path.isdir(os.path.join(a.work, "overlays")) else 0
    return {"vmm_processes": int(procs.stdout.strip() or 0),
            "runtime_dirs_with_files": sum(
                1 for d in runtime if os.listdir(os.path.join(root, d))),
            "overlays": overlays}


def scen_disk(kind, a, lab):
    """Package-manager and build style disk work, with integrity checks."""
    rep = Report()
    v = vm(kind, a, lab, "disk")
    v.boot(slot=5, mem=a.mem)
    try:
        steps = [
            ("20k small files", "python3 - <<'EOF'\nimport os,hashlib\nos.makedirs('/root/many',exist_ok=True)\n"
                                "h=hashlib.sha256()\nfor i in range(20000):\n d=f'/root/many/{i%100}'\n"
                                " os.makedirs(d,exist_ok=True)\n b=os.urandom(64)+str(i).encode()\n"
                                " open(f'{d}/{i}','wb').write(b); h.update(b)\nprint(h.hexdigest())\nEOF",
             None),
            ("tar + untar", "cd /root && tar cf many.tar many && mkdir -p un && tar xf many.tar -C un && "
                            "diff -r many un/many && echo same", lambda o: o.strip().endswith("same")),
            ("stat + unlink 20k", "find /root/many -type f | wc -l && rm -rf /root/many /root/un && echo gone",
             lambda o: "20000" in o and "gone" in o),
            ("256 MiB write + fsync", "dd if=/dev/urandom of=/root/big bs=1M count=256 conv=fsync 2>/dev/null"
                                      " && sha256sum /root/big | cut -c1-16", None),
            ("drop caches + reread", "sync; echo 3 > /proc/sys/vm/drop_caches; sha256sum /root/big | cut -c1-16",
             None),
            ("compile sqlite-sized C", "python3 - <<'EOF'\nsrc=['#include <stdio.h>']\nfor i in range(3000):\n"
                                       " src.append(f'int f{i}(int x){{return x*{i}+{i%7};}}')\n"
                                       "src.append('int main(){long s=0;'+''.join(f's+=f{i}(1);' for i in range(3000))"
                                       "+'printf(\"%ld\\\\n\",s);}')\nopen('/root/big.c','w').write('\\n'.join(src))\nEOF\n"
                                       "gcc -O1 /root/big.c -o /root/bigc && /root/bigc", None),
        ]
        outs = {}
        for name, cmd, ok in steps:
            t = now()
            code, out = try_exec(v, cmd, 600000)
            rep.metric("step_ms/" + name, (now() - t) * 1000)
            outs[name] = out.strip()
            rep.check(name, code == 0 and (ok is None or ok(out)), "" if code == 0 else out[-200:])
        rep.check("data identical after cache drop",
                  outs.get("256 MiB write + fsync") == outs.get("drop caches + reread"))
        rep.check("compiled program correct",
                  outs.get("compile sqlite-sized C") == str(sum(i + i % 7 for i in range(3000))))
    finally:
        v.stop()
    return rep.to_dict()


def scen_soak(kind, a, lab):
    """Sustained exec load with periodic pause/resume; watch for growth."""
    rep = Report()
    v = vm(kind, a, lab, "soak")
    v.boot(slot=6, mem=a.fork_mem)
    end = now() + a.soak_secs
    lat, fails, pauses = [], 0, 0
    first = resource_probe(v.pid())
    next_pause = now() + 5
    try:
        while now() < end:
            t = now()
            code, out = try_exec(v, "echo $RANDOM >/dev/null; cat /proc/loadavg")
            lat.append((now() - t) * 1000)
            fails += code != 0
            if now() >= next_pause:
                v.pause()
                time.sleep(0.05)
                v.resume()
                pauses += 1
                next_pause = now() + 5
        last = resource_probe(v.pid())
    finally:
        v.stop()
    rep.check("%d execs over %ds with %d pause/resume, none failed" % (len(lat), a.soak_secs, pauses),
              fails == 0, "%d failed" % fails)
    rep.check("VMM fd count stable", last["fds"] <= first["fds"] + 2, "%s -> %s" % (first["fds"], last["fds"]))
    rep.check("VMM thread count stable", last["threads"] <= first["threads"] + 2,
              "%s -> %s" % (first["threads"], last["threads"]))
    rep.check("VMM RSS stable (< 8 MiB growth)", last["rss_kib"] - first["rss_kib"] < 8192,
              "%s -> %s KiB" % (first["rss_kib"], last["rss_kib"]))
    rep.metric("exec_ms", vb.summary(lat))
    rep.metric("resources_start", first)
    rep.metric("resources_end", last)
    return rep.to_dict()


def resource_probe(pid):
    """Resources of the VMM itself (the lab wraps it in nsenter/unshare)."""
    target = pid
    for _ in range(3):
        kids = sh(["pgrep", "-P", str(target)], check=False).stdout.split()
        if not kids:
            break
        target = int(kids[0])
    try:
        fds = len(os.listdir("/proc/%d/fd" % target))
        threads = len(os.listdir("/proc/%d/task" % target))
    except FileNotFoundError:
        fds = threads = -1
    return {"pid": target, "fds": fds, "threads": threads,
            "rss_kib": vb.pss_kib(target).get("Rss", 0)}


def scen_net(kind, a, lab):
    """Bulk download from the host gateway, verified byte for byte."""
    rep = Report()
    digest = lab.bulk_file()
    v = vm(kind, a, lab, "net")
    v.boot(slot=7, mem=a.mem)
    try:
        speeds, intact = [], 0
        for _ in range(3):
            code, out = try_exec(
                v, "curl -sS -m 120 -o /tmp/bulk -w '%%{speed_download}\\n' http://172.16.7.1:%d/bulk.bin"
                   " && sha256sum /tmp/bulk | cut -c1-64; rm -f /tmp/bulk" % BULK_HTTP_PORT, 180000)
            lines = out.split() if code == 0 else []
            if len(lines) == 2:
                speeds.append(float(lines[0]) / 1e6)
                intact += lines[1] == digest
        rep.check("%d MiB download x3, contents intact" % BULK_MIB, intact == 3, "%d of 3 intact" % intact)
        if speeds:
            rep.metric("download_MBps", sorted(speeds)[len(speeds) // 2])
    finally:
        v.stop()
    return rep.to_dict()


SCEN = {"session": scen_session, "resume": scen_resume, "fork": scen_fork,
        "burst": scen_burst, "disk": scen_disk, "net": scen_net, "soak": scen_soak}


def print_table(results):
    kinds = [k for k in ("tarit", "firecracker") if k in results]
    print("\n%-10s %-52s %s" % ("scenario", "check", "  ".join("%-11s" % k for k in kinds)))
    for s in SCEN:
        rows = {}
        for k in kinds:
            r = results[k].get(s)
            if not r:
                continue
            if "error" in r:
                rows.setdefault("scenario ran", {})[k] = "ERROR"
                continue
            for c in r["checks"]:
                rows.setdefault(c["check"], {})[k] = "pass" if c["ok"] else "FAIL"
        for check, by_kind in rows.items():
            print("%-10s %-52s %s" % (s, check[:52], "  ".join("%-11s" % by_kind.get(k, "-") for k in kinds)))


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--tarit", required=True)
    p.add_argument("--firecracker", required=True)
    p.add_argument("--kernel", required=True)
    p.add_argument("--rootfs", required=True, help="agent image (vmm pull ... --agent)")
    p.add_argument("--work", default="/var/tmp/agentbench/work")
    p.add_argument("--out", required=True)
    p.add_argument("--only", default="tarit,firecracker")
    p.add_argument("--scenarios", default="session,resume,fork,burst,disk,net,soak")
    p.add_argument("--vcpus", type=int, default=1)
    p.add_argument("--mem", type=int, default=1024)
    p.add_argument("--fork-mem", type=int, default=512)
    p.add_argument("--cycles", type=int, default=10)
    p.add_argument("--forks", type=int, default=8)
    p.add_argument("--burst", type=int, default=10)
    p.add_argument("--burst-cmds", type=int, default=30)
    p.add_argument("--short-cmds", type=int, default=100)
    p.add_argument("--soak-secs", type=int, default=120)
    a = p.parse_args()
    for path in ("tarit", "firecracker", "kernel", "rootfs"):
        setattr(a, path, os.path.abspath(getattr(a, path)))
    os.makedirs(a.work, exist_ok=True)
    os.makedirs(vb.RUN, exist_ok=True)
    # Turn SIGTERM into an exception so the lab namespace is always torn down.
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
    lab = Lab(a.work).start()
    results = {k: {} for k in a.only.split(",")}
    try:
        for s in a.scenarios.split(","):
            for kind in a.only.split(","):
                print("== %s %s" % (kind, s), file=sys.stderr, flush=True)
                t = now()
                try:
                    results[kind][s] = SCEN[s](kind, a, lab)
                except Exception as e:  # noqa: BLE001
                    results[kind][s] = {"error": repr(e)[:400]}
                results[kind][s]["wall_s"] = round(now() - t, 1)
                r = results[kind][s]
                print("   %s" % ("ERROR " + r["error"] if "error" in r else
                                 "%d/%d checks passed" % (r["passed"], r["total"])),
                      file=sys.stderr, flush=True)
                with open(a.out, "w") as f:
                    json.dump({**results, "firecracker_cache_type": vb.FC_CACHE_TYPE}, f, indent=2)
    finally:
        lab.stop()
    print_table(results)


if __name__ == "__main__":
    main()
