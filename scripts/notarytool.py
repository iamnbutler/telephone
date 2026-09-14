#!/usr/bin/env python3
"""Run Apple's tool with a wall-clock limit, without echoing credential arguments."""
import subprocess
import sys


def run_bounded(command, timeout):
    try:
        # Run the resolved executable directly so the bounded child is the tool,
        # not a launcher. Never stringify exceptions: they contain credential argv.
        # subprocess.run kills and reaps the child on timeout.
        result = subprocess.run(
            command,
            stdin=subprocess.DEVNULL,
            timeout=timeout,
            check=False,
        )
    except subprocess.TimeoutExpired:
        print("notarytool exceeded its wall-clock limit", file=sys.stderr)
        return 124
    except OSError:
        print("Could not execute notarytool", file=sys.stderr)
        return 127
    return result.returncode if result.returncode >= 0 else 128 - result.returncode


def main():
    if len(sys.argv) < 3 or not sys.argv[1].isdigit():
        print("Usage: notarytool.py <timeout-seconds> <notarytool arguments...>", file=sys.stderr)
        return 2
    timeout = int(sys.argv[1])
    if not 1 <= timeout <= 1800:
        print("notarytool timeout must be between 1 and 1800 seconds", file=sys.stderr)
        return 2
    try:
        resolved = subprocess.run(
            ["/usr/bin/xcrun", "--find", "notarytool"],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )
    except (subprocess.TimeoutExpired, OSError):
        print("Could not locate notarytool", file=sys.stderr)
        return 127
    executable = resolved.stdout.strip()
    if resolved.returncode != 0 or not executable.startswith("/") or "\n" in executable:
        print("Could not locate notarytool", file=sys.stderr)
        return 127
    return run_bounded([executable, *sys.argv[2:]], timeout)


if __name__ == "__main__":
    sys.exit(main())
