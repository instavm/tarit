#!/usr/bin/env python3
"""Tiny HTTP server for agentbench's lab: answers "ok" and logs each path.

Guests reach it on their gateway address, so a hit proves guest -> host
networking and identifies which guest asked.
"""
import http.server, sys


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        with open(LOG, "a") as f:
            f.write("%s %s\n" % (self.client_address[0], self.path))
        body = b"ok\n"
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    LOG = sys.argv[2]
    http.server.ThreadingHTTPServer(("0.0.0.0", int(sys.argv[1])), Handler).serve_forever()
