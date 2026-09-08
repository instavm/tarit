#!/usr/bin/env python3
"""Serial API/CLI shape qualification against an isolated local taritd."""

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import urllib.error
import urllib.request
import urllib.parse
import uuid


VCPUS = (1, 2, 4, 8)
MEMORY_MIB = (256, 512, 1024, 2048, 3072, 4096)


def shapes():
    return [(cpu, memory) for memory in MEMORY_MIB for cpu in VCPUS]


def meminfo(text):
    return {line.split(':', 1)[0]: int(line.split()[1])
            for line in text.splitlines() if ':' in line}


def validate_guest(cpu, memory, actual_cpu, actual_kib):
    if actual_cpu != cpu:
        raise AssertionError(f"guest CPUs: {actual_cpu}, requested {cpu}")
    # MemTotal excludes kernel reservations. Reject lost high RAM and
    # accidentally oversized guests without assuming identical kernel overhead.
    if not memory * 1024 * 0.80 <= actual_kib <= memory * 1024:
        raise AssertionError(f"guest MemTotal: {actual_kib} KiB, requested {memory} MiB")


class Matrix:
    def __init__(self, args):
        self.args = args
        self.key = os.environ['TARIT_API_KEY']
        self.owned = set()

    def request(self, method, path, body=None, expected=200):
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(
            self.args.base_url.rstrip('/') + path, data=data, method=method,
            headers={'X-API-Key': self.key, 'Content-Type': 'application/json'})
        try:
            with urllib.request.urlopen(request, timeout=360) as response:
                status, payload = response.status, response.read()
        except urllib.error.HTTPError as error:
            status, payload = error.code, error.read()
        if status != expected:
            raise AssertionError(f"{method} {path}: {status}, expected {expected}: {payload!r}")
        return json.loads(payload) if payload else None

    def execute(self, vm_id, command):
        result = subprocess.run(
            [self.args.cli, '--base-url', self.args.base_url, '--json',
             'exec', vm_id, command], capture_output=True, text=True,
            timeout=180, check=True)
        row = json.loads(result.stdout)
        if row.get('exit_code') != 0 or row.get('error') or row.get('stderr'):
            raise AssertionError(row)
        return row['stdout'].strip()

    def verify(self, vm_id, cpu, memory, proof):
        output = self.execute(vm_id,
            "set -eu; . /etc/os-release; printf '%s\\n' \"$ID\"; uname -r; "
            "grep -c '^processor[[:space:]]*:' /proc/cpuinfo; "
            "awk '/^MemTotal:/ {print $2}' /proc/meminfo; "
            "cat /root/tarit-shape-proof; test ! -e /dev/kvm; "
            "! grep -Eq '(^|[[:space:]])(vmx|svm)([[:space:]]|$)' /proc/cpuinfo")
        os_id, kernel, actual_cpu, actual_kib, actual_proof = output.splitlines()
        assert os_id == self.args.os_id, output
        assert kernel.startswith(self.args.kernel_prefix), output
        assert actual_proof == proof, output
        validate_guest(cpu, memory, int(actual_cpu), int(actual_kib))

    def reject_shape(self, cpu, memory):
        before = self.request('GET', '/v1/vms')
        vm_id = str(uuid.uuid4())
        self.owned.add(vm_id)
        self.request('POST', '/v1/vms',
                     {'id': vm_id, 'vcpus': cpu, 'memory_mib': memory}, 429)
        after = self.request('GET', '/v1/vms')
        # Compare identities and lifecycle state, not timestamps or telemetry.
        identity = lambda rows: {(row['id'], row['status']) for row in rows}
        assert identity(after) == identity(before), (before, after)
        assert all(row['id'] != vm_id for row in after), after
        self.owned.remove(vm_id)
        print(json.dumps({'event': 'admission_rejection_pass',
                          'vcpus': cpu, 'memory_mib': memory}), flush=True)

    def check_empty_server_limits(self):
        assert self.request('GET', '/v1/vms') == [], 'admission lane requires an empty server'
        # With no VM present, the VM-count ceiling cannot mask these limits.
        self.reject_shape(9, 256)
        self.reject_shape(1, 4097)

    def run_shape(self, cpu, memory):
        available = meminfo(Path('/proc/meminfo').read_text())['MemAvailable']
        if available < (memory + self.args.host_reserve_mib) * 1024:
            raise RuntimeError(f"insufficient host headroom for {cpu}/{memory}: {available} KiB")
        free = shutil.disk_usage(self.args.storage_path).free
        if free < (memory + 2048) * 1024 * 1024:
            raise RuntimeError(f"insufficient snapshot storage for {cpu}/{memory}: {free} bytes")
        if self.request('GET', '/v1/vms') != []:
            raise RuntimeError('shape lane requires an empty isolated server')
        vm_id = str(uuid.uuid4())
        proof = uuid.uuid4().hex
        # Track the requested identity before sending: a lost create response
        # must not leave an unaccounted VM behind.
        self.owned.add(vm_id)
        row = self.request('POST', '/v1/vms',
                           {'id': vm_id, 'vcpus': cpu, 'memory_mib': memory}, 201)
        assert row['id'] == vm_id and row['status'] == 'running', row
        self.execute(vm_id, f"printf '%s' {proof} > /root/tarit-shape-proof; sync")
        self.verify(vm_id, cpu, memory, proof)
        self.reject_shape(1, 256)
        self.verify(vm_id, cpu, memory, proof)
        row = self.request('POST', f'/v1/vms/{vm_id}/hibernate', {})
        assert row['status'] == 'hibernated', row
        # The user's CLI exec must activate the VM without an explicit resume.
        self.verify(vm_id, cpu, memory, proof)
        self.request('DELETE', f'/v1/vms/{vm_id}', expected=204)
        self.owned.remove(vm_id)
        assert self.request('GET', '/v1/vms') == [], 'VM remained after deletion'
        print(json.dumps({'event': 'shape_pass', 'vcpus': cpu,
                          'memory_mib': memory, 'os': self.args.os_id,
                          'kernel': self.args.kernel_prefix,
                          'oversubscribed': cpu > os.cpu_count()}), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--base-url', required=True)
    parser.add_argument('--cli', required=True)
    parser.add_argument('--os-id', required=True, choices=['ubuntu', 'alpine'])
    parser.add_argument('--kernel-prefix', required=True, choices=['5.10.', '6.6.'])
    parser.add_argument('--host-reserve-mib', type=int, default=1536)
    parser.add_argument('--storage-path', required=True,
                        help='local filesystem containing the server snapshot artifacts')
    args = parser.parse_args()
    if args.host_reserve_mib < 1024:
        parser.error('host reserve must be at least 1024 MiB')
    if urllib.parse.urlsplit(args.base_url).hostname not in {'127.0.0.1', 'localhost', '::1'}:
        parser.error('host headroom checks require a local server')
    matrix = Matrix(args)
    try:
        matrix.check_empty_server_limits()
        for cpu, memory in shapes():
            matrix.run_shape(cpu, memory)
    except BaseException:
        # Retain identifiers and server evidence for inspection. Do not delete
        # a potentially live failure specimen automatically.
        print(json.dumps({'event': 'shape_failure', 'owned_vm_ids': sorted(matrix.owned)}), flush=True)
        raise
    print('RESOURCE_SHAPE_LANE_PASS cases=24', flush=True)


if __name__ == '__main__':
    main()
