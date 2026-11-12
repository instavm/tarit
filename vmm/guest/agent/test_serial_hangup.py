#!/usr/bin/env python3
"""vmm-agent keeps serving the serial exec channel after its tty is hung up.

systemd hangs up a tty (TTYVHangup=) when it starts a unit on it, for example
console-getty on /dev/console = ttyS0, even when that unit's ExecCondition=
then skips it. A descriptor opened before that returns 0 from every read.
The agent used to retry such a read forever: one vCPU at 100 % and no serial
exec. Here the agent serves a pty (VMM_AGENT_SERIAL), the test hangs the pty
up with TIOCVHANGUP, as systemd does, and then requires a serial readiness
probe and an exec to be answered and the agent to be idle.

Needs Linux and CAP_SYS_ADMIN (TIOCVHANGUP); skips otherwise.
Usage: test_serial_hangup.py ./vmm-agent
"""
import fcntl, os, pty, select, signal, subprocess, sys, time

TIOCVHANGUP = 0x5437


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


def cpu_seconds(pid):
    fields = open('/proc/%d/stat' % pid).read().rsplit(')', 1)[1].split()
    return (int(fields[11]) + int(fields[12])) / os.sysconf('SC_CLK_TCK')


def main(agent):
    if sys.platform != 'linux' or os.geteuid() != 0:
        print('SKIP: needs Linux and root (TIOCVHANGUP)')
        return 0
    master, slave = pty.openpty()
    env = dict(os.environ, VMM_AGENT_SERIAL=os.ttyname(slave))
    proc = subprocess.Popen([agent], env=env, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL, start_new_session=True)
    try:
        read_until(master, b'VMM_AGENT_READY', 10)
        os.write(master, b'VMM_PROBE:00000000000000b1\n')
        read_until(master, b'VMM_PROBE_OK:00000000000000b1', 5)

        fcntl.ioctl(slave, TIOCVHANGUP)
        time.sleep(2.5)

        start = cpu_seconds(proc.pid)
        time.sleep(1.0)
        burned = cpu_seconds(proc.pid) - start
        assert burned < 0.2, 'agent burned %.2f s of CPU in 1 s after the hangup (spinning)' % burned

        os.write(master, b'\nVMM_PROBE:00000000000000a2\n')
        read_until(master, b'VMM_PROBE_OK:00000000000000a2', 5)
        os.write(master, b'\nVMM_EXEC:echo serial-ok\n')
        out = read_until(master, b'VMM_EXEC_EXIT=0', 10)
        assert b'serial-ok' in out, out
        print('PASS: serial exec answered after a tty hangup; agent CPU %.3f s/s' % burned)
        return 0
    finally:
        os.killpg(proc.pid, signal.SIGKILL)
        proc.wait()
        os.close(master)
        os.close(slave)


if __name__ == '__main__':
    sys.exit(main(sys.argv[1] if len(sys.argv) > 1 else './vmm-agent'))
