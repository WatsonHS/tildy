"""Linux counter parsing must preserve units and missing-metric semantics."""
from pathlib import Path
import importlib.util
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('resource_bench', Path(__file__).with_name('resource.py'))
resource_bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(resource_bench)
LinuxMetrics = resource_bench.LinuxMetrics


class LinuxMetricsTests(unittest.TestCase):
    def test_comm_with_spaces_and_parentheses_does_not_shift_fields(self):
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root, '123')
            directory.mkdir()
            fields = ['0'] * 22
            fields[0] = 'S'
            fields[11], fields[12], fields[17] = '123', '45', '7'
            (directory / 'stat').write_text('123 (worker ) name) ' + ' '.join(fields))
            (directory / 'status').write_text('Name:\tworker\nVmRSS:\t4096 kB\n')
            metrics = LinuxMetrics(Path(root))
            metrics.ticks = 100
            row = metrics.read(123)
            self.assertEqual(row['rss_bytes'], 4 * 1024 * 1024)
            self.assertEqual(row['cpu_user_ns'], 1_230_000_000)
            self.assertEqual(row['cpu_system_ns'], 450_000_000)
            self.assertEqual(row['thread_count'], 7)
            self.assertIsNone(row['phys_footprint_bytes'])

    def test_missing_process_is_an_error_not_zero_resource_usage(self):
        with tempfile.TemporaryDirectory() as root:
            with self.assertRaises(FileNotFoundError):
                LinuxMetrics(Path(root)).read(123)


if __name__ == '__main__':
    unittest.main()
