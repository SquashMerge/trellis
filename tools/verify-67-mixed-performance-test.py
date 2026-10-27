#!/usr/bin/env python3
"""Check private-HOME cleanup without launching Trellis or signaling processes."""
import importlib.util
import signal
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    "mixed_runner", Path(__file__).with_name("verify-67-mixed-performance.py"))
RUNNER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUNNER)


class HomeProcessCleanup(unittest.TestCase):
    def setUp(self):
        self.home = Path("/tmp/mixed-runner-test-home")
        self.binary = Path("/tmp/mixed-runner-test-binary")

    def result(self, code=0, output="", error=""):
        return subprocess.CompletedProcess([], code, output, error)

    def test_portable_snapshot_finds_only_live_private_home_binary(self):
        matching = f"123 S {self.binary} daemon HOME={self.home} PATH=/bin\n"
        rows = matching + (
            f"124 Z {self.binary} daemon HOME={self.home} PATH=/bin\n"
            f"125 S {self.binary} daemon HOME=/tmp/other-home PATH=/bin\n"
            f"126 S {self.binary} daemon HOME={self.home}-other PATH=/bin\n"
            f"127 S /tmp/other-binary daemon HOME={self.home} PATH=/bin\n"
        )
        rechecks = []

        def ps(args, **kwargs):
            if "-p" in args:
                pid = int(args[args.index("-p") + 1])
                self.assertEqual(pid, 123)
                rechecks.append(pid)
                return self.result(output=matching) if len(rechecks) == 1 else self.result(code=1)
            # procps rejects the original mixed BSD/System V selector.
            if "-a" in args or "-x" in args:
                return self.result(code=1, error="error: must set personality to get -x option")
            self.assertIn("-A", args)
            self.assertIn("eww", args)  # environment-bearing, untruncated HOME evidence
            return self.result(output=rows)

        with patch.object(RUNNER.subprocess, "run", side_effect=ps), \
                patch.object(RUNNER.os, "kill") as kill, \
                patch.object(RUNNER, "record"), \
                patch.object(RUNNER, "election_free", return_value=True) as election:
            RUNNER.stop_home_processes(self.home, self.binary)
        kill.assert_called_once_with(123, signal.SIGTERM)
        self.assertEqual(rechecks, [123, 123])
        election.assert_called_once_with(self.home)

    def test_empty_global_snapshot_requires_success(self):
        for code in (0, 1):
            with self.subTest(returncode=code), \
                    patch.object(RUNNER.subprocess, "run", return_value=self.result(code=code)), \
                    patch.object(RUNNER.os, "kill") as kill:
                if code == 0:
                    RUNNER.stop_home_processes(self.home, self.binary)
                else:
                    with self.assertRaisesRegex(RuntimeError, "cannot enumerate"):
                        RUNNER.stop_home_processes(self.home, self.binary)
                kill.assert_not_called()

    def test_ps_failure_preserves_diagnostic(self):
        error = "error: must set personality to get -x option"
        with patch.object(RUNNER.subprocess, "run", return_value=self.result(code=1, error=error)), \
                patch.object(RUNNER.os, "kill") as kill:
            with self.assertRaisesRegex(RuntimeError, error):
                RUNNER.stop_home_processes(self.home, self.binary)
        kill.assert_not_called()

    def test_real_host_ps_can_take_global_snapshot(self):
        with tempfile.TemporaryDirectory(prefix="mixed-runner-ps-") as scratch:
            home = Path(scratch) / "home"
            binary = Path(scratch) / "nonexistent-trellis"
            with patch.object(RUNNER.os, "kill") as kill:
                RUNNER.stop_home_processes(home, binary)
            kill.assert_not_called()


if __name__ == "__main__":
    unittest.main()
