import unittest
from unittest.mock import Mock, patch
from types import SimpleNamespace
import tempfile
from pathlib import Path

from resource_shape_matrix import Matrix, meminfo, process_identity, shapes, validate_guest


class ShapeTests(unittest.TestCase):
    def test_process_identity_handles_parentheses_in_command(self):
        with tempfile.TemporaryDirectory() as directory:
            proc = Path(directory)
            (proc / '42').mkdir()
            (proc / '42/stat').write_text('42 (a weird ) name) S 1 ' + '0 ' * 17 + '123 0')
            self.assertEqual(process_identity(42, proc), (1, 123))
            self.assertIsNone(process_identity(43, proc))

    def test_hibernation_rejects_surviving_process_and_stale_pid(self):
        matrix = object.__new__(Matrix)
        row = {'status': 'hibernated', 'pid': None, 'socket_path': None,
               'runtime_jail_path': None, 'runtime_overlay_path': None}
        matrix.runtime_record = Mock(return_value=row)
        with patch('resource_shape_matrix.process_identity', return_value=(1, 123)):
            with self.assertRaises(AssertionError):
                matrix.verify_hibernated('vm', {42: (1, 123)})
        with patch('resource_shape_matrix.process_identity', return_value=(1, 456)):
            matrix.verify_hibernated('vm', {42: (1, 123)})
        row['pid'] = 42
        with patch('resource_shape_matrix.process_identity', return_value=None):
            with self.assertRaises(AssertionError):
                matrix.verify_hibernated('vm', {42: (1, 123)})

    def test_exact_shape_coverage(self):
        cases = shapes()
        self.assertEqual(len(cases), 24)
        self.assertEqual(len(set(cases)), 24)
        self.assertEqual({cpu for cpu, _ in cases}, {1, 2, 4, 8})
        self.assertEqual({mem for _, mem in cases}, {256, 512, 1024, 2048, 3072, 4096})

    def test_guest_shape_rejects_missing_high_memory_and_cpu(self):
        validate_guest(8, 4096, 8, 4000 * 1024)
        with self.assertRaises(AssertionError):
            validate_guest(8, 4096, 8, 3072 * 1024)
        with self.assertRaises(AssertionError):
            validate_guest(8, 4096, 8, 3328 * 1024)
        with self.assertRaises(AssertionError):
            validate_guest(8, 4096, 2, 4000 * 1024)
        with self.assertRaises(AssertionError):
            validate_guest(1, 256, 1, 512 * 1024)

    def test_host_memory_parser(self):
        self.assertEqual(meminfo('MemTotal: 8000 kB\nMemAvailable: 6000 kB\n'),
                         {'MemTotal': 8000, 'MemAvailable': 6000})

    def test_rejection_preserves_existing_vm(self):
        matrix = object.__new__(Matrix)
        matrix.owned = set()
        rows = [{'id': 'existing', 'status': 'running'}]
        matrix.request = Mock(side_effect=[rows, {'error': 'capacity'}, rows])
        matrix.reject_shape(1, 256)
        self.assertEqual(matrix.owned, set())
        self.assertEqual(matrix.request.call_args_list[1].args[-1], 429)

    def test_rejection_detects_disrupted_guest(self):
        matrix = object.__new__(Matrix)
        matrix.owned = set()
        matrix.request = Mock(side_effect=[
            [{'id': 'existing', 'status': 'running'}], {'error': 'capacity'},
            [{'id': 'existing', 'status': 'error'}]])
        with self.assertRaises(AssertionError):
            matrix.reject_shape(1, 256)
        self.assertEqual(len(matrix.owned), 1, 'retain rejected request ID on failure')

    def test_mixed_lane_tracks_and_deletes_all_requested_identities(self):
        matrix = object.__new__(Matrix)
        matrix.args = SimpleNamespace(host_reserve_mib=1536, storage_path='/fixture')
        matrix.owned = set()
        rows = {}

        def request(method, path, body=None, expected=200):
            if method == 'GET':
                return list(rows.values())
            if method == 'DELETE':
                del rows[path.rsplit('/', 1)[1]]
                return None
            self.assertIn(body['id'], matrix.owned)
            row = {'id': body['id'], 'status': 'running'}
            rows[body['id']] = row
            return {'vm': row} if path.endswith('/fork') else row

        matrix.request = request
        matrix.execute = Mock(side_effect=lambda _, command: '20' if 'while' in command else '')
        matrix.verify = Mock()
        with patch('resource_shape_matrix.Path.read_text', return_value='MemAvailable: 8000000 kB'), \
             patch('resource_shape_matrix.shutil.disk_usage', return_value=SimpleNamespace(free=8 * 1024**3)):
            matrix.run_mixed()
        self.assertFalse(rows)
        self.assertFalse(matrix.owned)
        self.assertEqual(matrix.verify.call_count, 9)


if __name__ == '__main__':
    unittest.main()
