#!/usr/bin/env python3
"""Minimal WebSocket PTY client for e2e validation.

Connects to taritd's PTY WebSocket, resizes, runs a couple of commands, and
checks for faithful output. Protocol: binary frames = PTY bytes, text frames =
JSON control ({"type":"resize"|"exit"}).
"""
import asyncio
import json
import sys

import websockets


async def main(url: str) -> int:
    out = b""
    exit_code = None
    try:
        async with websockets.connect(url, max_size=None) as ws:
            await ws.send(json.dumps({"type": "resize", "cols": 120, "rows": 40}))
            await asyncio.sleep(0.3)
            await ws.send(b"stty size; echo WS_PTY_OK_MARK; id -u; exit\n")
            while True:
                try:
                    msg = await asyncio.wait_for(ws.recv(), timeout=20)
                except asyncio.TimeoutError:
                    print("TIMEOUT waiting for data", file=sys.stderr)
                    break
                if isinstance(msg, (bytes, bytearray)):
                    out += bytes(msg)
                else:
                    try:
                        j = json.loads(msg)
                    except Exception:
                        continue
                    if j.get("type") == "exit":
                        exit_code = j.get("exit_code")
                        break
    except websockets.ConnectionClosed as e:
        print("WS CLOSED code=%s reason=%r" % (e.code, e.reason), file=sys.stderr)
    except Exception as e:
        print("WS ERROR %r" % (e,), file=sys.stderr)

    text = out.decode(errors="replace")
    sys.stdout.write(text)
    ok = ("WS_PTY_OK_MARK" in text) and ("40 120" in text)
    sys.stdout.write(
        "\n---\nexit_code=%s ws_marker=%s winsize_ok=%s\n"
        % (exit_code, "WS_PTY_OK_MARK" in text, "40 120" in text)
    )
    sys.stdout.write("WS_PTY_PASS\n" if ok else "WS_PTY_FAIL\n")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(asyncio.run(main(sys.argv[1])))
