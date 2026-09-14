"""Real subprocess tests for credential-safe errors and timeout cleanup."""
import contextlib
import io
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest

from notarytool import run_bounded


class NotarytoolTests(unittest.TestCase):
    def test_real_child_timeout_is_reaped_without_echoing_arguments(self):
        with tempfile.TemporaryDirectory(prefix="telephone-notary-timeout-") as directory:
            pid_file = Path(directory) / "pid"
            output = io.StringIO()
            start = time.monotonic()
            with contextlib.redirect_stderr(output):
                status = run_bounded(
                    [sys.executable, "-c",
                     "import os, pathlib, sys, time; pathlib.Path(sys.argv[1]).write_text(str(os.getpid())); time.sleep(60)",
                     str(pid_file), "credential-marker-not-for-logs"],
                    2,
                )
            self.assertEqual(status, 124)
            self.assertLess(time.monotonic() - start, 10)
            self.assertNotIn("credential-marker-not-for-logs", output.getvalue())
            pid = int(pid_file.read_text())
            with self.assertRaises(ProcessLookupError):
                os.kill(pid, 0)

    def test_spawn_error_does_not_echo_arguments(self):
        with tempfile.TemporaryDirectory(prefix="telephone-notary-missing-") as directory:
            output = io.StringIO()
            with contextlib.redirect_stderr(output):
                status = run_bounded([str(Path(directory) / "missing"), "credential-marker-not-for-logs"], 1)
            self.assertEqual(status, 127)
            self.assertNotIn("credential-marker-not-for-logs", output.getvalue())

    def test_exit_status_and_signal_are_preserved(self):
        self.assertEqual(run_bounded([sys.executable, "-c", "raise SystemExit(7)"], 5), 7)
        self.assertEqual(run_bounded([sys.executable, "-c", "import os, signal; os.kill(os.getpid(), signal.SIGTERM)"], 5), 143)

    def test_real_notarytool_commands(self):
        # Some runner images report "unknown (0)" as their tool version. Check
        # the actual CLI capabilities rather than treating that label as semver.
        result = subprocess.run(
            [sys.executable, str(Path(__file__).with_name("notarytool.py")), "10", "--help"],
            stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=20, check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("store-credentials", result.stdout)
        self.assertIn("submit", result.stdout)


if __name__ == "__main__":
    unittest.main()
