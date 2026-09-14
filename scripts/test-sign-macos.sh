#!/usr/bin/env bash
# Exercise real tar, Mach-O inspection and Keychain failure paths; no tool mocks.
set -euo pipefail
umask 077

[[ $(uname -s) == Darwin ]] || { printf 'These tests require macOS.\n' >&2; exit 1; }
repo_root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/telephone-sign-test.XXXXXX")
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
export MACOS_CERTIFICATE_P12_BASE64=bm90IGEgcGtjczEy
export MACOS_CERTIFICATE_PASSWORD=test-only
export MACOS_SIGNING_IDENTITY=0000000000000000000000000000000000000000
export MACOS_TEAM_ID=0000000000
case $(uname -m) in
  arm64) target=aarch64-apple-darwin; other=x86_64-apple-darwin ;;
  x86_64) target=x86_64-apple-darwin; other=aarch64-apple-darwin ;;
  *) printf 'Unsupported host.\n' >&2; exit 1 ;;
esac
printf 'int main(void) { return 0; }\n' | cc -x c - -o "$work/input/telephone"
for name in README.md SECURITY.md LICENSE; do
  printf 'Signing-test fixture.\n' > "$work/input/$name"
done
archive="$work/input.tar.gz"
COPYFILE_DISABLE=1 tar -czf "$archive" -C "$work/input" telephone README.md SECURITY.md LICENSE
original_keychains=$(security list-keychains -d user)

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
  [[ ! -e "$work/output.tar.gz" ]]
  [[ $(security list-keychains -d user) == "$original_keychains" ]]
  shopt -s nullglob
  local leftovers=("$RUNNER_TEMP"/telephone-sign.*)
  [[ ${#leftovers[@]} == 0 ]]
  printf 'PASS: %s\n' "$name"
}
sign=(bash "$repo_root/scripts/sign-macos.sh")
expect_failure 'missing certificate' 'MACOS_CERTIFICATE_P12_BASE64 is required' \
  env MACOS_CERTIFICATE_P12_BASE64= "${sign[@]}" "$target" "$archive" "$work/output.tar.gz"
expect_failure 'missing password' 'MACOS_CERTIFICATE_PASSWORD is required' \
  env MACOS_CERTIFICATE_PASSWORD= "${sign[@]}" "$target" "$archive" "$work/output.tar.gz"
expect_failure 'invalid identity' 'must be a certificate SHA-1 fingerprint' \
  env MACOS_SIGNING_IDENTITY=- "${sign[@]}" "$target" "$archive" "$work/output.tar.gz"
expect_failure 'invalid team' 'must be a ten-character Apple team ID' \
  env MACOS_TEAM_ID=invalid "${sign[@]}" "$target" "$archive" "$work/output.tar.gz"
expect_failure 'Linux target' 'Unsupported signing target' \
  "${sign[@]}" aarch64-unknown-linux-musl "$archive" "$work/output.tar.gz"
expect_failure 'existing output' 'Refusing to overwrite' \
  "${sign[@]}" "$target" "$archive" "$archive"
expect_failure 'wrong architecture' 'Binary architecture does not match' \
  "${sign[@]}" "$other" "$archive" "$work/output.tar.gz"
expect_failure 'malformed base64' 'base64:' \
  env MACOS_CERTIFICATE_P12_BASE64='%%%' "${sign[@]}" "$target" "$archive" "$work/output.tar.gz"
# This creates a real temporary keychain, then fails to import an invalid PKCS#12.
# Assert that the EXIT trap deletes it and restores the original search list.
expect_failure 'invalid PKCS#12 and keychain cleanup' 'SecKeychainItemImport' \
  "${sign[@]}" "$target" "$archive" "$work/output.tar.gz"
COPYFILE_DISABLE=1 tar -czf "$work/duplicate.tar.gz" -C "$work/input" telephone README.md SECURITY.md LICENSE telephone
expect_failure 'duplicate archive member' 'Unexpected archive contents' \
  "${sign[@]}" "$target" "$work/duplicate.tar.gz" "$work/output.tar.gz"
mv "$work/input/telephone" "$work/untouched"
ln -s "$work/untouched" "$work/input/telephone"
original_hash=$(shasum -a 256 "$work/untouched")
COPYFILE_DISABLE=1 tar -czf "$work/symlink.tar.gz" -C "$work/input" telephone README.md SECURITY.md LICENSE
expect_failure 'symlink archive member' 'Not a regular file: telephone' \
  "${sign[@]}" "$target" "$work/symlink.tar.gz" "$work/output.tar.gz"
[[ $(shasum -a 256 "$work/untouched") == "$original_hash" ]]
rm "$work/input/telephone" "$work/input/README.md"
mv "$work/untouched" "$work/input/telephone"
ln "$work/input/telephone" "$work/input/README.md"
COPYFILE_DISABLE=1 tar -czf "$work/hardlink.tar.gz" -C "$work/input" telephone README.md SECURITY.md LICENSE
expect_failure 'hard-linked archive members' 'Hard links are not allowed' \
  "${sign[@]}" "$target" "$work/hardlink.tar.gz" "$work/output.tar.gz"
COPYFILE_DISABLE=1 tar -czf "$work/traversal.tar.gz" -s ',LICENSE,../outside,' \
  -C "$work/input" telephone README.md SECURITY.md LICENSE
expect_failure 'archive path traversal' 'Unexpected archive contents' \
  "${sign[@]}" "$target" "$work/traversal.tar.gz" "$work/output.tar.gz"
[[ ! -e "$work/outside" && ! -e "$RUNNER_TEMP/outside" ]]
