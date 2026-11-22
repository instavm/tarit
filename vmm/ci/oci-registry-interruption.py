#!/usr/bin/env python3
"""Exercise vmm pull against a disposable local Distribution registry (Linux, root).

Requires docker-registry, skopeo, umoci, openssl, mke2fs, debugfs and e2fsck.
Pass the built VMM binary or an executable wrapper as the only argument.
"""

import argparse
from contextlib import ExitStack, closing
import hashlib
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import random
import socket
import ssl
import subprocess
import tempfile
import threading
import time


def run(*args, **kwargs):
    return subprocess.run(args, check=True, timeout=120, **kwargs)


def start_registry(stack, base):
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    config = base / "registry.yml"
    config.write_text(
        "version: 0.1\nlog:\n  level: error\n  accesslog:\n    disabled: true\nstorage:\n  filesystem:\n"
        f"    rootdirectory: {json.dumps(str(base / 'blobs'))}\n"
        f"http:\n  addr: 127.0.0.1:{port}\n"
    )
    registry = stack.enter_context(subprocess.Popen(["docker-registry", "serve", config]))
    stack.callback(registry.terminate)
    for _ in range(150):
        if registry.poll() is not None:
            raise RuntimeError("registry exited during startup")
        with closing(http.client.HTTPConnection("127.0.0.1", port, timeout=1)) as conn:
            try:
                conn.request("GET", "/v2/")
                if conn.getresponse().status == 200:
                    return port
            except ConnectionRefusedError:
                pass
        time.sleep(0.1)
    raise TimeoutError("registry did not start")


class PullProxy(BaseHTTPRequestHandler):
    def forward(self):
        with closing(http.client.HTTPConnection("127.0.0.1", self.server.backend, timeout=15)) as conn:
            conn.request(self.command, self.path, headers={"Accept": self.headers.get("Accept", "*/*")})
            response = conn.getresponse()
            self.send_response(response.status)
            body = response.read()
            for name, value in response.getheaders():
                if name.lower() in {"content-type", "content-length", "docker-content-digest",
                                    "docker-distribution-api-version"}:
                    self.send_header(name, value)
            self.end_headers()
            is_layer = self.command == "GET" and self.path == self.server.layer_path and response.status == 200
            interrupt = self.server.interrupt.is_set()
            if is_layer and interrupt:
                body = body[:len(body) // 2]
            self.close_connection = True
            try:
                if body:
                    self.wfile.write(body)
            except (BrokenPipeError, ConnectionResetError, ssl.SSLEOFError):
                # A failed layer cancels parallel blob requests from the same pull.
                if interrupt:
                    return
                raise
            if is_layer:
                sizes = self.server.cut_sizes if interrupt else self.server.full_sizes
                sizes.append(len(body))

    do_GET = do_HEAD = forward

    def log_message(self, *_args):
        pass


def start_proxy(stack, base, backend):
    cert, key = base / "cert.pem", base / "key.pem"
    run("openssl", "req", "-x509", "-nodes", "-newkey", "rsa:2048", "-days", "1",
        "-subj", "/CN=127.0.0.1", "-addext", "subjectAltName=IP:127.0.0.1",
        "-keyout", key, "-out", cert, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    proxy = stack.enter_context(ThreadingHTTPServer(("127.0.0.1", 0), PullProxy))
    tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    tls.load_cert_chain(cert, key)
    proxy.socket = tls.wrap_socket(proxy.socket, server_side=True)
    proxy.backend = backend
    proxy.layer_path = None
    proxy.interrupt = threading.Event()
    proxy.cut_sizes, proxy.full_sizes = [], []
    thread = threading.Thread(target=proxy.serve_forever)
    thread.start()
    stack.callback(thread.join)
    stack.callback(proxy.shutdown)
    return proxy, dict(os.environ, SSL_CERT_FILE=str(cert))


def push(layout, port):
    digest_file = layout.parent / "manifest-digest"
    run("skopeo", "copy", "--dest-tls-verify=false", "--digestfile", digest_file,
        f"oci:{layout}:fixture", f"docker://127.0.0.1:{port}/tarit/fixture:test",
        stdout=subprocess.DEVNULL)
    digest = digest_file.read_text().strip()
    with closing(http.client.HTTPConnection("127.0.0.1", port, timeout=15)) as conn:
        conn.request("GET", f"/v2/tarit/fixture/manifests/{digest}", headers={"Accept":
                     "application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json"})
        response = conn.getresponse()
        manifest = response.read()
        assert response.status == 200 and digest == "sha256:" + hashlib.sha256(manifest).hexdigest()
    return digest, json.loads(manifest)["layers"][-1]


def check_image(image, payload):
    run("e2fsck", "-fn", image, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    contents = run("debugfs", "-R", "cat /proof.bin", image,
                   stdout=subprocess.PIPE, stderr=subprocess.DEVNULL).stdout
    assert contents == payload, "published filesystem has unexpected bytes"


def qualify(vmm, base, stack):
    backend = start_registry(stack, base)
    proxy, env = start_proxy(stack, base, backend)
    layout, bundle, outputs = base / "layout", base / "bundle", base / "outputs"
    outputs.mkdir()
    image = f"{layout}:fixture"
    run("umoci", "init", "--layout", layout)
    run("umoci", "new", "--image", image)
    run("umoci", "unpack", "--image", image, bundle)
    proof = bundle / "rootfs/proof.bin"

    def publish(payload):
        proof.write_bytes(payload)
        run("umoci", "repack", "--image", image, bundle)
        digest, layer = push(layout, backend)
        return f"docker://127.0.0.1:{proxy.server_port}/tarit/fixture@{digest}", layer

    def pull(ref, output):
        return subprocess.run([vmm, "pull", "--size", "16", "--output", output, ref],
                              env=env, capture_output=True, timeout=120)

    old_data = b"previously published rootfs\n"
    new_data = random.Random(20261122).randbytes(512 * 1024)
    old_ref, _ = publish(old_data)
    existing, fresh = outputs / "existing.ext4", outputs / "fresh.ext4"
    baseline = pull(old_ref, existing)
    assert baseline.returncode == 0, baseline.stderr[-2000:]
    check_image(existing, old_data)
    previous = hashlib.sha256(existing.read_bytes()).digest()
    new_ref, layer = publish(new_data)
    assert new_ref != old_ref
    proxy.layer_path = f"/v2/tarit/fixture/blobs/{layer['digest']}"
    for output in (fresh, existing):
        proxy.cut_sizes.clear()
        proxy.interrupt.set()
        failed = pull(new_ref, output)
        assert failed.returncode and b"skopeo copy" in failed.stderr, failed.stderr[-1000:]
        assert proxy.cut_sizes and set(proxy.cut_sizes) == {layer["size"] // 2}
        assert not list(outputs.glob(".tarit-oci-*")), "abandoned pull workspace"
        if output == fresh:
            assert not fresh.exists(), "failed pull published a new image"
        else:
            assert hashlib.sha256(existing.read_bytes()).digest() == previous, "failed pull changed existing image"
        print(f"{output.name} interrupted at {proxy.cut_sizes[0]}/{layer['size']} bytes, rollback verified", flush=True)

        if output == existing:
            registry_image = f"docker://127.0.0.1:{backend}/tarit/fixture"
            old_digest = old_ref.rsplit("@", 1)[1]
            run("skopeo", "copy", "--preserve-digests", "--src-tls-verify=false", "--dest-tls-verify=false",
                f"{registry_image}@{old_digest}", f"{registry_image}:test", stdout=subprocess.DEVNULL)
            tag_digest = run("skopeo", "inspect", "--tls-verify=false", "--format", "{{.Digest}}",
                             f"{registry_image}:test", capture_output=True, text=True).stdout.strip()
            assert tag_digest == old_digest, "tag did not move back to the previous image"
            print("registry tag moved to previous manifest before pinned retry", flush=True)
        proxy.interrupt.clear()
        proxy.full_sizes.clear()
        retried = pull(new_ref, output)
        assert retried.returncode == 0, retried.stderr[-1000:]
        assert layer["size"] in proxy.full_sizes, "retry did not fetch the complete layer"
        check_image(output, new_data)
        assert not list(outputs.glob(".tarit-oci-*")), "abandoned retry workspace"
        print(f"{output.name} retry fetched full layer, fsck and {len(new_data)} proof bytes passed", flush=True)


if __name__ == "__main__":
    if not __debug__:
        raise SystemExit("Run with Python assertions enabled")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("vmm", type=Path)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="tarit-registry-") as directory, ExitStack() as stack:
        qualify(args.vmm.resolve(), Path(directory), stack)
    print("OCI_REGISTRY_RETRY_PASS")
