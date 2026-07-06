#!/usr/bin/env python3
"""Drive an interactive command through a real PTY and check for a marker.

Usage: pty_drive.py MARKER -- <cmd> [args...]
Spawns <cmd> with a real controlling terminal, waits for a shell prompt, sends
`echo MARKER; exit`, and reports DRIVE_PASS if MARKER appears in the output.
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
    marker = sys.argv[1]
    sep = sys.argv.index("--")
    argv = sys.argv[sep + 1:]

    pid, fd = pty.fork()
    if pid == 0:
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
            if not sent and any(p in out for p in (b"$ ", b"# ", b":/", b":~")):
                os.write(fd, ("echo %s; exit\n" % marker).encode())
                sent = True
        elif not sent and out:
            os.write(fd, ("echo %s; exit\n" % marker).encode())
            sent = True

    try:
        os.waitpid(pid, 0)
    except OSError:
        pass

    text = out.decode(errors="replace")
    sys.stdout.write(text)
    ok = marker in text
    sys.stdout.write("\nDRIVE_PASS\n" if ok else "\nDRIVE_FAIL\n")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
