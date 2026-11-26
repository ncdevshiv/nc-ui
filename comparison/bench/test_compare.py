import os
import sys
import unittest
from unittest import mock

if sys.platform == "win32":
    import compare
else:
    compare = None


@unittest.skipUnless(sys.platform == "win32", "the comparison harness is Windows-only")
class ComparisonEvidenceTests(unittest.TestCase):
    def setUp(self):
        self.result = {
            "framework": "ncui",
            "rounds": 200,
            "completed": 200,
            "elapsed_ms": 100.0,
            "per_round_ms": 0.5,
        }

    def test_complete_ipc_result_is_accepted(self):
        self.assertEqual(compare.validate_ipc_result(self.result, 200, "ncui", "port"), 0.5)

    def test_wrong_framework_is_refused(self):
        self.result["framework"] = "electron"
        with self.assertRaisesRegex(RuntimeError, "expected 'ncui'"):
            compare.validate_ipc_result(self.result, 200, "ncui", "port")

    def test_incomplete_ipc_calls_are_refused(self):
        self.result["completed"] = 199
        with self.assertRaisesRegex(RuntimeError, "199 of 200"):
            compare.validate_ipc_result(self.result, 200, "ncui", "port")

    def test_wrong_round_count_is_refused(self):
        self.result["rounds"] = 100
        with self.assertRaisesRegex(RuntimeError, "requested 200 rounds"):
            compare.validate_ipc_result(self.result, 200, "ncui", "port")

    def test_inconsistent_elapsed_time_is_refused(self):
        self.result["elapsed_ms"] = 101.0
        with self.assertRaisesRegex(RuntimeError, "disagree"):
            compare.validate_ipc_result(self.result, 200, "ncui", "port")

    def test_non_finite_or_boolean_times_are_refused(self):
        for value in (float("nan"), float("inf"), True):
            with self.subTest(value=value):
                self.result["per_round_ms"] = value
                with self.assertRaisesRegex(RuntimeError, "invalid per-round time"):
                    compare.validate_ipc_result(self.result, 200, "ncui", "port")

    def test_ipc_summary_retains_samples_and_consistent_median_timing(self):
        results = [
            {**self.result, "repeat": index, "per_round_ms": value, "elapsed_ms": value * 200}
            for index, value in enumerate((0.5, 0.4, 0.6), start=1)
        ]

        summary = compare.summarize_ipc(results, 200)

        self.assertEqual(summary["repeat"], 3)
        self.assertEqual(summary["per_round_ms_samples"], [0.5, 0.4, 0.6])
        self.assertEqual(summary["per_round_ms_min"], 0.4)
        self.assertEqual(summary["per_round_ms"], 0.5)
        self.assertEqual(summary["per_round_ms_max"], 0.6)
        self.assertEqual(summary["elapsed_ms_samples"], [100.0, 80.0, 120.0])
        self.assertEqual(summary["elapsed_ms_min"], 80.0)
        self.assertEqual(summary["elapsed_ms"], 100.0)
        self.assertEqual(summary["elapsed_ms_max"], 120.0)
        self.assertNotIn("per_round_ms_samples", results[-1])

    def test_process_tree_contains_the_root_process(self):
        pid = os.getpid()
        self.assertIn(pid, compare.process_tree_pids(pid))
        self.assertGreater(compare.process_tree_memory(pid), 0)

    def test_idle_sampling_waits_for_a_live_descendant(self):
        with mock.patch.object(compare, "process_memory", return_value=100), mock.patch.object(
            compare, "process_tree_memory", side_effect=(100, 250)
        ):
            self.assertTrue(compare.wait_for_process_tree(123, timeout=0.5))

    def test_idle_sampling_refuses_a_tree_that_never_gains_a_descendant(self):
        with mock.patch.object(compare, "process_memory", return_value=100), mock.patch.object(
            compare, "process_tree_memory", return_value=100
        ):
            self.assertFalse(compare.wait_for_process_tree(123, timeout=0.05))

    def test_cleanup_waits_for_the_captured_descendant_to_exit(self):
        process = mock.Mock(pid=123)
        with mock.patch.object(compare, "process_tree_pids", return_value={123, 456}), mock.patch.object(
            compare, "kill_tree"
        ) as kill, mock.patch.object(
            compare, "process_memory", side_effect=(None, 100, None, None)
        ):
            compare.stop_process_tree(process)
        process.wait.assert_called_once_with(timeout=10)
        kill.assert_called_once_with(123)


if __name__ == "__main__":
    unittest.main()
