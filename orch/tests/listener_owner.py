#!/usr/bin/env python3
"""Verify that a Linux process owns the requested IPv4 loopback listener."""

import argparse
from pathlib import Path


def owns_listener(pid, port, proc=Path('/proc')):
    process = proc / str(pid)
    try:
        sockets = set()
        for descriptor in (process / 'fd').iterdir():
            try:
                target = descriptor.readlink().as_posix()
            except FileNotFoundError:
                continue
            if target.startswith('socket:[') and target.endswith(']'):
                sockets.add(target[8:-1])
        address = f'0100007F:{port:04X}'
        for line in (process / 'net/tcp').read_text().splitlines()[1:]:
            fields = line.split()
            if len(fields) >= 10 and fields[1] == address and fields[3] == '0A':
                if fields[9] in sockets:
                    return True
    except (FileNotFoundError, PermissionError, ProcessLookupError):
        return False
    return False


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('pid', type=int)
    parser.add_argument('port', type=int)
    args = parser.parse_args()
    raise SystemExit(0 if owns_listener(args.pid, args.port) else 1)
