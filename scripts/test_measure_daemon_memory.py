"""Regression checks for the unprivileged measurement driver's failure paths."""

import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("measure", Path(__file__).with_name("measure-daemon-memory.py"))
measure = importlib.util.module_from_spec(spec)
spec.loader.exec_module(measure)


@unittest.skipUnless(sys.platform == "linux", "Linux measurement driver")
class DriverTests(unittest.TestCase):
    def test_timeout_kills_and_reaps_workload(self):
        child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"])
        with self.assertRaises(TimeoutError):
            measure.wait_for_workload(child, 0.05)
        self.assertLess(child.returncode, 0)
        with self.assertRaises(ChildProcessError):
            os.waitpid(child.pid, os.WNOHANG)

    def test_normal_exit_retains_child_usage(self):
        child = subprocess.Popen([sys.executable, "-c", "pass"])
        usage = measure.wait_for_workload(child, 5)
        self.assertEqual(child.returncode, 0)
        self.assertGreater(usage.ru_maxrss, 0)

    def test_output_aliases_are_rejected_before_execution(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "binary"
            binary.write_bytes(b"preserve this input")
            symbolic = Path(directory) / "symbolic"
            symbolic.symlink_to(binary)
            hard = Path(directory) / "hard"
            os.link(binary, hard)
            for output in [binary, symbolic, hard]:
                with self.subTest(output=output), patch.object(measure, "run") as run:
                    self.invoke(binary, output, expected_code=2)
                    run.assert_not_called()
                    self.assertEqual(binary.read_bytes(), b"preserve this input")

    def test_failure_preserves_partial_samples_and_marks_report_incomplete(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "binary"
            binary.write_bytes(b"synthetic")
            output = Path(directory) / "result.json"
            sample = {"wall_s": 1, "cpu_s": 0.5, "maxrss_kib": 10, "stages": []}
            # Complete one base/head pair, then only the next scenario's base.
            with patch.object(measure, "run", side_effect=[sample, sample, sample, TimeoutError("stalled")]):
                self.invoke(binary, output, head=binary, expected_code=1)
            report = json.loads(output.read_text())
            self.assertFalse(report["complete"])
            self.assertIn("TimeoutError", report["error"])
            self.assertEqual(len(report["runs"]), 3)
            self.assertEqual(set(report["summary"]["attached"]), {"base"})

    def invoke(self, binary, output, expected_code, head=None):
        arguments = ["measure", "--base", str(binary), "--mode", "timing", "--repeats", "1",
                     "--warmups", "0", "--output", str(output)]
        if head:
            arguments += ["--head", str(head)]
        with patch.object(sys, "argv", arguments), contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit) as exited:
                measure.main()
        self.assertEqual(exited.exception.code, expected_code)


if __name__ == "__main__":
    unittest.main()
