#!/usr/bin/env python3
"""Drive `ssh` through a real PTY to test the taritd SSH gateway interactively.

Emulates an actual terminal (real winsize, interactive session) instead of a
piped/batch stdin, which is the real use case for `ssh vm_id@gateway`.

Usage: ssh_pty_test.py KEYFILE PORT VM_ID HOST
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
    argv = [
        "ssh", "-tt", "-p", port, "-i", keyfile,
        "-o", "StrictHostKeyChecking=no",
        "-o", "UserKnownHostsFile=/dev/null",
        "-o", "PreferredAuthentications=publickey",
        "-o", "IdentitiesOnly=yes",
        "-o", "LogLevel=ERROR",
        f"{user}@{host}",
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
    sent = False
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

    try:
        os.waitpid(pid, 0)
    except OSError:
        pass

    text = out.decode(errors="replace")
    sys.stdout.write(text)
    ok = "SSH_GW_OK_MARK" in text
    sys.stdout.write("\n---\nSSH_GW_PASS\n" if ok else "\n---\nSSH_GW_FAIL\n")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
