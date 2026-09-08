import unittest

from resource_shape_matrix import meminfo, shapes, validate_guest


class ShapeTests(unittest.TestCase):
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
            validate_guest(8, 4096, 2, 4000 * 1024)
        with self.assertRaises(AssertionError):
            validate_guest(1, 256, 1, 512 * 1024)

    def test_host_memory_parser(self):
        self.assertEqual(meminfo('MemTotal: 8000 kB\nMemAvailable: 6000 kB\n'),
                         {'MemTotal': 8000, 'MemAvailable': 6000})


if __name__ == '__main__':
    unittest.main()
