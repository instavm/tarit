#!/usr/bin/env python3
"""Send one session-fenced, HMAC-authenticated Tarit peer request over mTLS."""

import base64
import hashlib
import hmac
import ssl
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid


def b64url(value: bytes) -> str:
    return base64.urlsafe_b64encode(value).rstrip(b"=").decode()


def main() -> int:
    (
        url,
        cert,
        key,
        ca,
        secret,
        source,
        source_session,
        target,
        target_session,
        expected_status,
    ) = sys.argv[1:]
    issued_at = str(int(time.time()))
    nonce = str(uuid.uuid4())
    identity_nonce = str(uuid.uuid4())
    tenant = "peer-e2e"
    role = "admin"
    api_key_id = "peer-e2e-key"
    identity_mac = hmac.new(secret.encode(), digestmod=hashlib.sha256)
    identity_mac.update(
        "\n".join(
            (
                "tarit-peer-identity-v1",
                source,
                issued_at,
                identity_nonce,
                tenant,
                role,
                api_key_id,
            )
        ).encode()
    )
    identity_signature = b64url(identity_mac.digest())
    payload_hash = b64url(hashlib.sha256(b"").digest())
    path = urllib.parse.urlsplit(url).path
    components = (
        "tarit-peer-request-v2",
        "GET",
        path,
        payload_hash,
        issued_at,
        nonce,
        source,
        target,
        source_session,
        target_session,
        identity_signature,
    )
    mac = hmac.new(secret.encode(), digestmod=hashlib.sha256)
    for component in components:
        mac.update(component.encode())
        mac.update(b"\n")
    headers = {
        "X-Tarit-Peer-Version": "tarit-peer-request-v2",
        "X-Tarit-Peer-Source": source,
        "X-Tarit-Peer-Target": target,
        "X-Tarit-Peer-Source-Session": source_session,
        "X-Tarit-Peer-Target-Session": target_session,
        "X-Tarit-Peer-Timestamp": issued_at,
        "X-Tarit-Peer-Nonce": nonce,
        "X-Tarit-Peer-Body-SHA256": payload_hash,
        "X-Tarit-Peer-Signature": b64url(mac.digest()),
        "X-Tarit-Tenant": tenant,
        "X-Tarit-Role": role,
        "X-Tarit-Api-Key-Id": api_key_id,
        "X-Tarit-Identity-Timestamp": issued_at,
        "X-Tarit-Identity-Nonce": identity_nonce,
        "X-Tarit-Identity-Signature": identity_signature,
    }
    context = ssl.create_default_context(cafile=ca)
    context.load_cert_chain(certfile=cert, keyfile=key)
    request = urllib.request.Request(url, headers=headers, method="GET")
    try:
        with urllib.request.urlopen(request, context=context, timeout=10) as response:
            status = response.status
    except urllib.error.HTTPError as error:
        status = error.code
    expected = int(expected_status)
    if status != expected:
        raise AssertionError(f"expected HTTP {expected}, got {status}")
    print(f"peer_probe_status={status}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
