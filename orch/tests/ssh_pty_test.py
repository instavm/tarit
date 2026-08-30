#!/usr/bin/env python3
"""Drive `ssh` through a real PTY to test the taritd SSH gateway.

Emulates an actual terminal and sends an SSH exec request. Command mode avoids
depending on a particular guest shell prompt while still exercising Tarit's
authenticated SSH-to-guest-PTY bridge.

Usage: ssh_pty_test.py KEYFILE PORT VM_ID HOST [COMMAND [EXPECTED_MARKER]]
"""
import fcntl
import os
import pty
import select
import struct
import sys
import termios
import time


def main() -> int:
    keyfile, port, user, host = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
    command = sys.argv[5] if len(sys.argv) > 5 else "echo SSH_GW_OK_MARK; id -u"
    expected_marker = sys.argv[6] if len(sys.argv) > 6 else "SSH_GW_OK_MARK"
    argv = [
        "ssh", "-tt", "-p", port, "-i", keyfile,
        "-o", "StrictHostKeyChecking=no",
        "-o", "UserKnownHostsFile=/dev/null",
        "-o", "PreferredAuthentications=publickey",
        "-o", "IdentitiesOnly=yes",
        "-o", "LogLevel=VERBOSE",
        f"{user}@{host}",
        command,
    ]

    pid, fd = pty.fork()
    if pid == 0:
        # Child: give our controlling tty a real size, then become ssh.
        try:
            fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
        except Exception:
            pass
        os.execvp(argv[0], argv)
        os._exit(127)

    out = b""
    sent = True
    deadline = time.time() + 30
    while time.time() < deadline:
        r, _, _ = select.select([fd], [], [], 0.5)
        if fd in r:
            try:
                d = os.read(fd, 4096)
            except OSError:
                break
            if not d:
                break
            out += d
            if not sent and (b"$ " in out or b"# " in out or b":/" in out):
                os.write(fd, b"echo SSH_GW_OK_MARK; id -u; exit\n")
                sent = True
        elif not sent and out:
            os.write(fd, b"echo SSH_GW_OK_MARK; id -u; exit\n")
            sent = True

    # Never let a broken gateway/client handshake turn this bounded acceptance
    # probe into an unbounded CI hang. Reap a completed child, otherwise
    # terminate it and escalate after a short grace period.
    try:
        waited, _ = os.waitpid(pid, os.WNOHANG)
        if waited == 0:
            os.kill(pid, 15)
            grace = time.time() + 2
            while time.time() < grace:
                waited, _ = os.waitpid(pid, os.WNOHANG)
                if waited == pid:
                    break
                time.sleep(0.05)
            else:
                os.kill(pid, 9)
                os.waitpid(pid, 0)
    except (ChildProcessError, ProcessLookupError, OSError):
        pass

    text = out.decode(errors="replace")
    sys.stdout.write(text)
    ok = expected_marker in text
    sys.stdout.write("\n---\nSSH_GW_PASS\n" if ok else "\n---\nSSH_GW_FAIL\n")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
