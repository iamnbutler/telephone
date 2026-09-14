#!/usr/bin/env bash
# Real archive/signature checks. Preflight failures must never reach Apple.
set -euo pipefail
umask 077
[[ $(uname -s) == Darwin ]] || { printf 'These tests require macOS.\n' >&2; exit 1; }
repo_root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/telephone-notary-test.XXXXXX")
cleanup() {
  status=$?
  trap - EXIT
  if ! rm -r -- "$work"; then status=1; fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
mkdir "$work/input" "$work/runner"
export RUNNER_TEMP="$work/runner"
export APPLE_ID=notary-test@example.invalid
export APPLE_APP_SPECIFIC_PASSWORD=not-a-real-password
export MACOS_TEAM_ID=0000000000
case $(uname -m) in
  arm64) target=aarch64-apple-darwin; other=x86_64-apple-darwin ;;
  x86_64) target=x86_64-apple-darwin; other=aarch64-apple-darwin ;;
  *) printf 'Unsupported host.\n' >&2; exit 1 ;;
esac
printf 'int main(void) { return 0; }\n' | cc -x c - -o "$work/input/telephone"
codesign --force --sign - "$work/input/telephone"
for name in README.md SECURITY.md LICENSE; do
  printf 'Notarization-test fixture.\n' > "$work/input/$name"
done
archive="$work/input.tar.gz"
COPYFILE_DISABLE=1 tar -czf "$archive" -C "$work/input" telephone README.md SECURITY.md LICENSE
original_keychains=$(security list-keychains -d user)
original_hash=$(shasum -a 256 "$archive")

expect_failure() {
  local name=$1 expected=$2
  shift 2
  if "$@" > "$work/output.log" 2>&1; then
    printf 'FAIL: %s unexpectedly succeeded.\n' "$name" >&2
    exit 1
  fi
  if ! grep -F -- "$expected" "$work/output.log" >/dev/null; then
    printf 'FAIL: %s did not report the expected error.\n' "$name" >&2
    cat "$work/output.log" >&2
    exit 1
  fi
  [[ ! -e "$work/report" ]]
  [[ $(security list-keychains -d user) == "$original_keychains" ]]
  [[ $(shasum -a 256 "$archive") == "$original_hash" ]]
  shopt -s nullglob
  local leftovers=("$RUNNER_TEMP"/telephone-notary.*)
  [[ ${#leftovers[@]} == 0 ]]
  printf 'PASS: %s\n' "$name"
}
notarize=(bash "$repo_root/scripts/notarize-macos.sh")
expect_failure 'missing Apple ID' 'APPLE_ID is required' \
  env APPLE_ID= "${notarize[@]}" "$target" "$archive" "$work/report"
expect_failure 'missing app-specific password' 'APPLE_APP_SPECIFIC_PASSWORD is required' \
  env APPLE_APP_SPECIFIC_PASSWORD= "${notarize[@]}" "$target" "$archive" "$work/report"
expect_failure 'invalid team' 'must be a ten-character Apple team ID' \
  env MACOS_TEAM_ID=invalid "${notarize[@]}" "$target" "$archive" "$work/report"
expect_failure 'existing report' 'Refusing to overwrite an existing report' \
  "${notarize[@]}" "$target" "$archive" "$work/input"
expect_failure 'wrong architecture' 'Binary architecture does not match' \
  "${notarize[@]}" "$other" "$archive" "$work/report"
expect_failure 'ad-hoc signature' 'Expected a valid Developer ID signature' \
  "${notarize[@]}" "$target" "$archive" "$work/report"
COPYFILE_DISABLE=1 tar -czf "$work/duplicate.tar.gz" -C "$work/input" telephone README.md SECURITY.md LICENSE telephone
expect_failure 'duplicate member' 'Unexpected archive contents' \
  "${notarize[@]}" "$target" "$work/duplicate.tar.gz" "$work/report"
mv "$work/input/telephone" "$work/untouched"
ln -s "$work/untouched" "$work/input/telephone"
COPYFILE_DISABLE=1 tar -czf "$work/symlink.tar.gz" -C "$work/input" telephone README.md SECURITY.md LICENSE
expect_failure 'symlink member' 'Not a regular file: telephone' \
  "${notarize[@]}" "$target" "$work/symlink.tar.gz" "$work/report"
python3 -B "$repo_root/scripts/test_notarytool.py"
