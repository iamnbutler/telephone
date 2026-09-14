"""Exercise real temporary keychain registration; never change trust settings."""

import secrets
import shlex
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("add-signing-keychain.py")


def search_list():
    result = subprocess.run(
        ["/usr/bin/security", "list-keychains", "-d", "user"],
        capture_output=True,
        text=True,
        timeout=10,
        check=True,
    )
    return [str(Path(path).resolve()) for path in shlex.split(result.stdout)]


@unittest.skipUnless(sys.platform == "darwin", "requires macOS Keychain")
class SigningKeychainTests(unittest.TestCase):
    def test_registration_preserves_entries_and_is_idempotent(self):
        original = search_list()
        with tempfile.TemporaryDirectory(prefix="telephone keychain test ") as work:
            keychain = str(Path(work, "signing keychain.keychain-db").resolve())
            try:
                subprocess.run(
                    ["/usr/bin/security", "create-keychain", "-p", secrets.token_hex(32), keychain],
                    capture_output=True,
                    timeout=10,
                    check=True,
                )
                for _ in range(2):
                    result = subprocess.run(
                        [sys.executable, str(SCRIPT), keychain],
                        capture_output=True,
                        text=True,
                        timeout=25,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    registered = search_list()
                    self.assertEqual(registered.count(keychain), 1)
                    self.assertEqual([p for p in registered if p != keychain], original)
            finally:
                if Path(keychain).exists():
                    subprocess.run(
                        ["/usr/bin/security", "delete-keychain", keychain],
                        capture_output=True,
                        timeout=10,
                        check=True,
                    )
            self.assertEqual(search_list(), original)

    def test_missing_keychain_does_not_change_search_list(self):
        original = search_list()
        with tempfile.TemporaryDirectory(prefix="telephone-keychain-missing-") as work:
            result = subprocess.run(
                [sys.executable, str(SCRIPT), str(Path(work, "missing.keychain-db"))],
                capture_output=True,
                text=True,
                timeout=10,
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("expected an existing keychain file", result.stderr)
        self.assertEqual(search_list(), original)


if __name__ == "__main__":
    unittest.main()
