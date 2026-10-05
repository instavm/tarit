#!/usr/bin/env python3
"""Starts an isolated local taritd; never targets an existing service.
Run through e2e_memory_growth.sh with freshly built kernel/agent fixtures.
"""
import json
import os
from pathlib import Path
import signal
import shutil
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[2]

def main():
    assert os.geteuid() == 0 and Path('/dev/kvm').exists(), 'requires Linux root + KVM'
    serial = os.environ.get('TARIT_MEMORY_GROWTH_SERIAL') == '1'
    available = next(int(line.split()[1]) * 1024 for line in Path('/proc/meminfo').read_text().splitlines() if line.startswith('MemAvailable:'))
    assert available >= (6 if serial else 12) * 1024 ** 3, 'insufficient available host RAM; do not disturb existing guests'
    assert shutil.disk_usage(os.environ.get('TARIT_TEST_SOCKET_ROOT', '/tmp')).free >= 20 * 1024 ** 3, 'need 20GiB free scratch disk'
    assert Path(os.environ['TARIT_ROOTFS']).is_file()
    assert Path(os.environ['TARIT_KERNEL']).is_file()
    with tempfile.TemporaryDirectory(prefix='tarit-memory-e2e-', dir=os.environ.get('TARIT_TEST_SOCKET_ROOT', '/tmp')) as tmp:
        path = Path(tmp)
        for name in ('sockets', 'runtime'):
            (path / name).mkdir(mode=0o700)
        with socket.socket() as listener:
            listener.bind(('127.0.0.1', 0))
            port = listener.getsockname()[1]
        base = f'http://127.0.0.1:{port}'
        key = uuid.uuid4().hex
        env = {key: value for key, value in os.environ.items() if not key.startswith('TARIT_')}
        env['TARIT_KERNEL'] = os.environ['TARIT_KERNEL']
        env['TARIT_ROOTFS'] = os.environ['TARIT_ROOTFS']
        env.update(TARIT_API_KEY=key, TARIT_LISTEN=f'127.0.0.1:{port}', TARIT_RPC_ADDR=base,
                   TARIT_ALLOW_INSECURE_PEER_HTTP='1', TARIT_ENABLE_NET='0', TARIT_ROOTFS_READONLY='0',
                   TARIT_SOCKET_DIR=str(path / 'sockets'), TARIT_DB=str(path / 'fleet.db'),
                   TARIT_CONFIG=str(path / 'none.toml'), TARIT_WARM_POOL='0', TARIT_MAX_VMS='1' if serial else '2',
                   TARIT_MAX_MEMORY_MIB='4096' if serial else '8192', TARIT_MAX_VCPUS='4', TARIT_ADMISSION_TIMEOUT_MS='1000',
                   TARIT_REAP_ON_SHUTDOWN='true', TARIT_PRODUCTION='0', TARIT_SSH_GATEWAY='0',
                   TMPDIR=str(path / 'runtime'), RUST_LOG='info')
        env['TARIT_VMM_BIN'] = os.environ.get('TARIT_VMM_BIN', str(ROOT / 'vmm/target/release/vmm'))
        # Prevent caller configuration from joining a fleet or operating external providers.
        for name in list(env):
            if name.startswith(('TARIT_PG_', 'TARIT_POSTGRES', 'TARIT_FLEET_', 'TARIT_PEER_', 'TARIT_AUTOSCALE')):
                env.pop(name)
        log = (path / 'taritd.log').open('w+')
        proc = subprocess.Popen([os.environ.get('TARITD_BIN', str(ROOT / 'orch/target/release/taritd')), 'serve'],
                                env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        ids = set()
        def api(method, route, body=None, allow_error=False):
            data = None if body is None else json.dumps(body).encode()
            request = urllib.request.Request(base + route, data=data, method=method,
                                             headers={'X-API-Key': key, 'Content-Type': 'application/json'})
            try:
                with urllib.request.urlopen(request, timeout=180) as response:
                    raw = response.read()
                    return json.loads(raw) if raw else None
            except urllib.error.HTTPError as error:
                detail = error.read().decode()
                if allow_error:
                    return error.code, detail
                raise AssertionError((method, route, error.code, detail)) from error
        def execute(vm, command):
            result = api('POST', '/v1/execute', {'vm_id': vm, 'command': command, 'timeout_ms': 120000})
            assert result['exit_code'] == 0, result
            return result['stdout'].strip()
        def status(vm):
            result = json.loads(execute(vm, '/tarit-memory-workload status'))
            assert result['bad_pages'] == 0, result
            return result
        def snapshot(vm):
            return api('POST', f'/v1/vms/{vm}/snapshot', {'diff': False})['snapshot_id']
        def restore(snap, target=None, fail=None):
            vm = str(uuid.uuid4())
            ids.add(vm)
            body = {'snapshot_id': snap, 'id': vm}
            if target is not None:
                body['target_memory_mib'] = target
            result = api('POST', '/v1/restore', body, allow_error=bool(fail))
            if fail:
                assert isinstance(result, tuple) and result[0] >= 400, result
                assert fail in result[1], result
                visible = api('GET', f'/v1/vms/{vm}', allow_error=True)
                assert isinstance(visible, tuple) or visible['status'] != 'running', visible
                api('DELETE', f'/v1/vms/{vm}', allow_error=True)
                ids.discard(vm)
                return
            assert result['status'] == 'running' and result['memory_mib'] == 4096, result
            return vm
        def delete(vm):
            api('DELETE', f'/v1/vms/{vm}')
            ids.discard(vm)
        try:
            for _ in range(160):
                if proc.poll() is not None:
                    raise RuntimeError('isolated taritd exited')
                try:
                    api('GET', '/health')
                    break
                except (OSError, AssertionError):
                    time.sleep(0.25)
            else:
                raise RuntimeError('isolated taritd did not start')
            source = api('POST', '/v1/vms', {'memory_mib': 4096, 'boot_memory_mib': 2048, 'vcpus': 1})['id']
            ids.add(source)
            # /proc/MemTotal is below nominal capacity because the kernel reserves pages.
            initial_total = int(execute(source, "awk '/MemTotal:/ {print $2}' /proc/meminfo"))
            assert 1800000 < initial_total < 2100000, initial_total
            execute(source, 'nohup /tarit-memory-workload serve >/tmp/memory-workload.log 2>&1 </dev/null &')
            for _ in range(100):
                try:
                    before = status(source)
                    break
                except AssertionError:
                    time.sleep(0.1)
            else:
                raise RuntimeError('memory witness did not start')
            snap = snapshot(source)
            if serial:
                delete(source)
            restore(snap, 8192, fail='memory target must be aligned')
            restore(snap, 1024, fail='memory target must be aligned')
            if not serial:
                assert status(source) == before
            child = restore(snap, 4096)
            assert status(child) == before, 'running PID/nonce/RAM markers changed'
            grown_total = int(execute(child, "awk '/MemTotal:/ {print $2}' /proc/meminfo"))
            assert grown_total > initial_total + 1900000, (initial_total, grown_total)
            grown = json.loads(execute(child, '/tarit-memory-workload grow'))
            assert grown['pid'] == before['pid'] and grown['nonce'] == before['nonce']
            assert grown['resident'] == 3 * 1024 ** 3 and grown['bad_pages'] == 0, grown
            assert status(child) == grown
            if not serial:
                assert status(source) == before, 'clone modified source process memory'
                delete(source)
            snap2 = snapshot(child)
            if serial:
                delete(child)
            restore(snap2, 3072, fail='memory shrink is unsupported')
            if not serial:
                assert status(child) == grown
            second = restore(snap2)  # Saved target/device bitmap must survive without a new override.
            assert status(second) == grown
            assert int(execute(second, "awk '/MemTotal:/ {print $2}' /proc/meminfo")) == grown_total
            print(json.dumps({'result': 'PASS', 'serial': serial, 'source_isolation_checked': not serial, 'boot_memtotal_kib': initial_total,
                              'grown_memtotal_kib': grown_total, 'witness': grown,
                              'checks': ['2GiB boot', '4GiB restore', '3GiB touched',
                                         'second snapshot/restore', 'oversize/shrink rollback', 'reservation reuse']}))
        finally:
            for vm in ids:
                try:
                    api('DELETE', f'/v1/vms/{vm}', allow_error=True)
                except (OSError, AssertionError):
                    pass
            os.killpg(proc.pid, signal.SIGTERM) if proc.poll() is None else None
            try:
                proc.wait(timeout=15)
            except subprocess.TimeoutExpired:
                os.killpg(proc.pid, signal.SIGKILL)
                proc.wait()
            log.seek(0)
            print(log.read()[-24000:])
            log.close()

if __name__ == '__main__':
    main()
