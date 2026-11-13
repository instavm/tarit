#!/usr/bin/env python3
"""The vmm-agent supervisor sleeps while idle, restarts a killed worker, runs
commands with an empty signal mask, and stops cleanly on SIGTERM when it is not
PID 1.

Runs the real agent with its serial channel on a pty (VMM_AGENT_SERIAL); the
vsock workers just keep retrying outside a guest. Needs Linux (/proc); no root.
Usage: test_worker_supervisor.py ./vmm-agent
"""
import os, pty, re, select, signal, subprocess, sys, time


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


def stat_fields(pid):
    return open('/proc/%d/stat' % pid).read().rsplit(')', 1)[1].split()


def wakeups(pid):
    counts = {}
    for line in open('/proc/%d/status' % pid):
        key, _, value = line.partition(':')
        if key in ('voluntary_ctxt_switches', 'nonvoluntary_ctxt_switches'):
            counts[key] = int(value)
    return sum(counts.values())


def session_pids(sid):
    pids = []
    for name in os.listdir('/proc'):
        if name.isdigit():
            try:
                fields = stat_fields(int(name))
            except OSError:
                continue
            if int(fields[3]) == sid and fields[0] != 'Z':
                pids.append(int(name))
    return pids


def serial_worker(supervisor, tty):
    for pid in session_pids(supervisor):
        if pid == supervisor or int(stat_fields(pid)[1]) != supervisor:
            continue
        try:
            fds = os.listdir('/proc/%d/fd' % pid)
            if any(os.readlink('/proc/%d/fd/%s' % (pid, fd)) == tty for fd in fds):
                return pid
        except OSError:
            continue
    raise AssertionError('no serial worker holds %s' % tty)


def main(agent):
    if sys.platform != 'linux':
        print('SKIP: needs Linux')
        return 0
    master, slave = pty.openpty()
    tty = os.ttyname(slave)
    env = dict(os.environ, VMM_AGENT_SERIAL=tty)
    proc = subprocess.Popen([agent], env=env, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL, start_new_session=True)
    try:
        read_until(master, b'VMM_AGENT_READY', 10)
        time.sleep(0.5)

        # Idle: the supervisor must block, not poll (the old loop woke every 100 ms).
        start = wakeups(proc.pid)
        time.sleep(2.0)
        woke = wakeups(proc.pid) - start
        assert woke <= 2, 'idle supervisor woke %d times in 2 s (polling)' % woke

        # A killed worker is replaced without waiting for a poll tick.
        worker = serial_worker(proc.pid, tty)
        os.kill(worker, signal.SIGKILL)
        read_until(master, b'VMM_AGENT_READY', 5)
        replacement = serial_worker(proc.pid, tty)
        assert replacement != worker
        os.write(master, b'\nVMM_PROBE:00000000000000c3\n')
        read_until(master, b'VMM_PROBE_OK:00000000000000c3', 5)

        # The supervisor blocks the signals it waits for; a command must not
        # inherit that mask (it would run with SIGCHLD and SIGTERM blocked).
        os.write(master, b'\nVMM_EXEC:grep SigBlk /proc/self/status\n')
        out = read_until(master, b'VMM_EXEC_EXIT=0', 10)
        blocked = re.search(rb'SigBlk:\s*([0-9a-f]+)', out)
        assert blocked, out
        assert int(blocked.group(1), 16) == 0, 'command inherited blocked signals: %r' % out

        # Not PID 1: SIGTERM stops the supervisor and every worker.
        os.kill(proc.pid, signal.SIGTERM)
        proc.wait(timeout=5)
        deadline = time.monotonic() + 5
        while session_pids(proc.pid) and time.monotonic() < deadline:
            time.sleep(0.1)
        left = session_pids(proc.pid)
        assert not left, 'workers survived SIGTERM: %r' % left
        print('PASS: supervisor idle wakeups %d in 2 s, worker restart, clean command mask, '
              'SIGTERM shutdown' % woke)
        return 0
    finally:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        proc.wait()
        os.close(master)
        os.close(slave)


if __name__ == '__main__':
    sys.exit(main(sys.argv[1] if len(sys.argv) > 1 else './vmm-agent'))
