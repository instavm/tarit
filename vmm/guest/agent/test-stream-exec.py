#!/usr/bin/env python3
"""Linux-only real guest-handler regressions; no root, vsock or KVM required."""
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time


def send(sock, kind, payload=b''):
    sock.sendall(struct.pack('>BI', kind, len(payload)) + payload)


def exact(sock, length):
    output = b''
    while len(output) < length:
        part = sock.recv(length - len(output))
        if not part:
            raise AssertionError('unexpected stream EOF')
        output += part
    return output


def receive(sock):
    kind, size = struct.unpack('>BI', exact(sock, 5))
    assert size <= 16 * 1024 * 1024
    return kind, exact(sock, size)


def start(binary, command, kind=5):
    host, guest = socket.socketpair()
    host.settimeout(10)
    proc = subprocess.Popen([str(binary), str(guest.fileno())], pass_fds=[guest.fileno()])
    guest.close()
    send(host, kind, json.dumps({'shell': command, 'cols': 80, 'rows': 24}).encode())
    return host, proc


def collect(sock):
    output, errors = bytearray(), bytearray()
    while True:
        kind, payload = receive(sock)
        if kind == 0:
            output.extend(payload)
        elif kind == 7:
            errors.extend(payload)
        elif kind == 2:
            return bytes(output), bytes(errors), json.loads(payload)['exit_code']
        else:
            raise AssertionError((kind, payload))


def main():
    if not sys.platform.startswith('linux'):
        raise SystemExit('This test requires Linux headers and pipe2; run on an isolated Linux host.')
    source = Path(__file__).with_name('vmm-agent.c').resolve()
    with tempfile.TemporaryDirectory(prefix='tarit-exec-test-') as directory:
        wrapper = Path(directory) / 'handler.c'
        wrapper.write_text('#define main guest_agent_main\n#include "' + str(source) +
                           '"\n#undef main\nint main(int argc, char **argv) {\n'
                           '    if (argc != 2) { return 2; }\n'
                           '    signal(SIGPIPE, SIG_IGN);\n'
                           '    handle_pty_client(atoi(argv[1]));\n'
                           '    return 0;\n}\n')
        binary = Path(directory) / 'handler'
        subprocess.run([os.environ.get('CC', 'cc'), '-O2', '-Wall', '-Wextra', '-Werror',
                        str(wrapper), '-lutil', '-o', str(binary)], check=True)
        data = bytes(range(256)) * 2048
        sock, proc = start(binary, '# ' + 'x' * 8192 + '\ncat; printf "stderr\\n" >&2; exit 7')
        failures = []

        def write_input():
            try:
                for offset in range(0, len(data), 32768):
                    send(sock, 0, data[offset:offset + 32768])
                send(sock, 6)
            except BaseException as error:
                failures.append(error)

        writer = threading.Thread(target=write_input)
        writer.start()
        output, errors, status = collect(sock)
        writer.join(10)
        assert not writer.is_alive() and not failures, failures
        assert (output, errors, status) == (data, b'stderr\n', 7)
        sock.close()
        assert proc.wait(timeout=5) == 0
        print('PASS binary stdin/stdout, stdin EOF, separated stderr, exit7, command >4KiB')

        sock, proc = start(binary, 'printf "terminal\\n"; exit 3', kind=4)
        output, errors, status = collect(sock)
        assert (output, errors, status) == (b'terminal\r\n', b'', 3)
        sock.close()
        assert proc.wait(timeout=5) == 0
        print('PASS interactive PTY CRLF behavior and exit status unchanged')

        sock, proc = start(binary, "trap '' HUP TERM; echo $$; while :; do sleep 1; done")
        kind, payload = receive(sock)
        assert kind == 0
        child = int(payload.strip())
        sock.close()
        assert proc.wait(timeout=5) == 0
        try:
            os.kill(child, 0)
        except ProcessLookupError:
            pass
        else:
            os.kill(child, 9)
            raise AssertionError('disconnected command survived')
        print('PASS disconnect kills and reaps command ignoring HUP/TERM')


if __name__ == '__main__':
    main()
