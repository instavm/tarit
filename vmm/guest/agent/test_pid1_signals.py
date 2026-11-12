#!/usr/bin/env python3
"""As PID 1, vmm-agent must not act on SIGTERM or SIGINT.

The kernel drops those signals for init unless init handles them. If the agent
took one and returned from main, init would exit and the guest kernel would
panic. Here the agent runs as PID 1 of a new PID namespace, where the same rule
applies to signals sent from outside it. Its PID 1 setup (mounts, loopback,
resolv.conf) stays inside new user, mount and network namespaces, with a tmpfs
over /etc. The test sends SIGTERM and SIGINT and then requires the agent to be
alive and still answering serial probes.

Needs Linux, root and util-linux unshare; skips otherwise.
Usage: test_pid1_signals.py ./vmm-agent
"""
import os, pty, select, shutil, signal, subprocess, sys, tempfile, time

# The agent mounts over /dev/pts, /tmp and /run as PID 1, so it gets its serial
# pty through a bind mount placed elsewhere.
SETUP = 'mount --bind "$1" "$2" && mount -t tmpfs tmpfs /etc && exec "$3"'


def read_until(fd, marker, timeout):
    buf = b''
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        ready, _, _ = select.select([fd], [], [], max(0.0, deadline - time.monotonic()))
        if ready:
            try:
                chunk = os.read(fd, 4096)
            except OSError:
                chunk = b''
            buf += chunk
            if marker in buf:
                return buf
    raise AssertionError('no %r within %.0fs; got %r' % (marker, timeout, buf[-300:]))


def namespace_init(unshare_pid):
    """Host PID of the process unshare forked: PID 1 inside the namespace."""
    for name in os.listdir('/proc'):
        if not name.isdigit():
            continue
        try:
            fields = open('/proc/%s/stat' % name).read().rsplit(')', 1)[1].split()
        except OSError:
            continue
        if int(fields[1]) == unshare_pid:
            return int(name)
    raise AssertionError('unshare %d has no child' % unshare_pid)


def namespace_pid(pid):
    for line in open('/proc/%d/status' % pid):
        if line.startswith('NSpid:'):
            return int(line.split()[-1])
    raise AssertionError('no NSpid for %d' % pid)


def main(agent):
    if sys.platform != 'linux' or os.geteuid() != 0 or shutil.which('unshare') is None:
        print('SKIP: needs Linux, root and unshare')
        return 0
    agent = os.path.abspath(agent)
    master, slave = pty.openpty()
    workdir = tempfile.mkdtemp(prefix='vmm-agent-pid1-', dir='/var/tmp')
    serial = os.path.join(workdir, 'serial')
    open(serial, 'w').close()
    env = dict(os.environ, VMM_AGENT_SERIAL=serial)
    proc = subprocess.Popen(
        ['unshare', '--user', '--map-root-user', '--pid', '--fork', '--kill-child',
         '--mount', '--mount-proc', '--net',
         'sh', '-c', SETUP, 'sh', os.ttyname(slave), serial, agent],
        env=env, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL, start_new_session=True)
    init = None
    try:
        read_until(master, b'VMM_AGENT_READY', 10)
        init = namespace_init(proc.pid)
        assert namespace_pid(init) == 1, 'agent is not PID 1 in its namespace'
        assert open('/proc/%d/comm' % init).read().strip() == 'vmm-agent'

        os.kill(init, signal.SIGTERM)
        os.kill(init, signal.SIGINT)
        time.sleep(1.0)
        assert proc.poll() is None, 'PID 1 agent exited after SIGTERM/SIGINT (guest would panic)'
        assert namespace_init(proc.pid) == init

        os.write(master, b'\nVMM_PROBE:00000000000000d4\n')
        read_until(master, b'VMM_PROBE_OK:00000000000000d4', 5)
        print('PASS: agent as PID 1 ignored SIGTERM and SIGINT and kept serving')
        return 0
    finally:
        if init is not None:
            try:
                os.kill(init, signal.SIGKILL)
            except ProcessLookupError:
                pass
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        proc.wait()
        os.close(master)
        os.close(slave)
        os.unlink(serial)
        os.rmdir(workdir)


if __name__ == '__main__':
    sys.exit(main(sys.argv[1] if len(sys.argv) > 1 else './vmm-agent'))
