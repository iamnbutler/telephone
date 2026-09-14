#!/usr/bin/env python3
"""Add a temporary signing keychain without dropping existing search entries."""

import shlex
import subprocess
import sys
from pathlib import Path


def main() -> int:
    if len(sys.argv) != 2 or not Path(sys.argv[1]).is_file():
        print("add-signing-keychain: expected an existing keychain file.", file=sys.stderr)
        return 1
    keychain = str(Path(sys.argv[1]).resolve())
    command = ["/usr/bin/security", "list-keychains", "-d", "user"]
    try:
        result = subprocess.run(command, capture_output=True, text=True, timeout=10)
        if result.returncode != 0:
            raise RuntimeError("Cannot read the keychain search list.")
        paths = shlex.split(result.stdout)
        if any(not Path(path).is_absolute() for path in paths):
            raise ValueError("Unexpected keychain search-list output.")
        if keychain not in (str(Path(path).resolve()) for path in paths):
            result = subprocess.run(
                [*command, "-s", keychain, *paths],
                capture_output=True,
                timeout=10,
            )
            if result.returncode != 0:
                raise RuntimeError("Cannot update the keychain search list.")
    except (OSError, ValueError, RuntimeError, subprocess.TimeoutExpired):
        print("add-signing-keychain: search-list update failed.", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
