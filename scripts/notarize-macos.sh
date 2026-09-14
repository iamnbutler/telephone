#!/usr/bin/env bash
# Notarize the exact signed payload without modifying the distributed archive.
set +x
set -euo pipefail
umask 077

fail() { printf 'notarize-macos: %s\n' "$*" >&2; exit 1; }
[[ $# == 3 ]] || fail 'Usage: bash scripts/notarize-macos.sh <target> <signed.tar.gz> <new-report-directory>'
target=$1
archive=$2
report=$3
case "$target" in
  aarch64-apple-darwin) architecture=arm64 ;;
  x86_64-apple-darwin) architecture=x86_64 ;;
  *) fail "Unsupported notarization target: $target" ;;
esac
[[ $(uname -s) == Darwin ]] || fail 'Notarization requires macOS.'
[[ -f "$archive" && ! -L "$archive" ]] || fail 'Input must be a regular archive.'
[[ ! -e "$report" && ! -L "$report" ]] || fail 'Refusing to overwrite an existing report.'
[[ -n ${APPLE_ID:-} ]] || fail 'APPLE_ID is required.'
[[ -n ${APPLE_APP_SPECIFIC_PASSWORD:-} ]] || fail 'APPLE_APP_SPECIFIC_PASSWORD is required.'
[[ ${MACOS_TEAM_ID:-} =~ ^[A-Z0-9]{10}$ ]] || fail 'MACOS_TEAM_ID must be a ten-character Apple team ID.'
script_dir=$(cd "$(dirname "$0")" && pwd)
notary=(python3 "$script_dir/notarytool.py")
archive_hash=$(shasum -a 256 "$archive" | cut -d ' ' -f 1)
listing=$(tar -tzf "$archive")
[[ "$listing" == $'telephone\nREADME.md\nSECURITY.md\nLICENSE' ]] || fail 'Unexpected archive contents.'

work=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/telephone-notary.XXXXXX")
keychain="$work/notary.keychain-db"
cleanup() {
  status=$?
  trap - EXIT
  if [[ -e "$keychain" ]]; then
    if ! security delete-keychain "$keychain"; then
      printf 'notarize-macos: failed to delete the temporary credential keychain.\n' >&2
      status=1
    fi
  fi
  if ! rm -r -- "$work"; then
    printf 'notarize-macos: failed to remove the temporary directory.\n' >&2
    status=1
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir "$work/package"
tar -xzf "$archive" -C "$work/package" --no-same-owner --no-same-permissions
for name in telephone README.md SECURITY.md LICENSE; do
  [[ -f "$work/package/$name" && ! -L "$work/package/$name" ]] || fail "Not a regular file: $name"
  [[ $(stat -f %l "$work/package/$name") == 1 ]] || fail "Hard links are not allowed: $name"
done
binary="$work/package/telephone"
[[ $(lipo -archs "$binary") == "$architecture" ]] || fail 'Binary architecture does not match the notarization target.'
requirement="anchor apple generic and certificate leaf[subject.OU] = \"$MACOS_TEAM_ID\" and certificate leaf[field.1.2.840.113635.100.6.1.13] exists"
codesign --verify --strict -R="$requirement" "$binary" || fail 'Expected a valid Developer ID signature for our team.'
signature=$(codesign --display --verbose=4 "$binary" 2>&1)
[[ "$signature" == *'flags=0x10000(runtime)'* ]] || fail 'Hardened runtime is missing.'
[[ "$signature" == *$'\nTimestamp='* ]] || fail 'Secure timestamp is missing.'

# The service accepts ZIPs, not tar.gz. Only the signed executable goes to Apple.
# Standalone Mach-O binaries cannot be stapled; Gatekeeper fetches their tickets online.
zip="$work/telephone-$target.zip"
ditto -c -k --norsrc "$binary" "$zip"
zip_hash=$(shasum -a 256 "$zip" | cut -d ' ' -f 1)
mkdir -p "$report"
keychain_password=$(openssl rand -hex 32)
if [[ ${GITHUB_ACTIONS:-} == true ]]; then
  printf '::add-mask::%s\n' "$keychain_password"
fi
security create-keychain -p "$keychain_password" "$keychain"
security set-keychain-settings -lut 1800 "$keychain"
security unlock-keychain -p "$keychain_password" "$keychain"
unset keychain_password
# Keep authentication output private, including on errors. It is not an artifact.
if ! "${notary[@]}" 120 store-credentials telephone-ci --keychain "$keychain" \
    --apple-id "$APPLE_ID" --password "$APPLE_APP_SPECIFIC_PASSWORD" --team-id "$MACOS_TEAM_ID" \
    > "$work/auth.log" 2>&1; then
  fail 'Apple credential validation failed or timed out. Check the account, app-specific password, team access and developer agreements.'
fi
unset APPLE_ID APPLE_APP_SPECIFIC_PASSWORD
auth=(--keychain-profile telephone-ci --keychain "$keychain")
printf 'Apple notarization credentials validated.\n'

submit_status=0
"${notary[@]}" 300 submit "$zip" "${auth[@]}" --no-wait --output-format json \
  > "$work/submission.json" 2> "$work/submit.err" || submit_status=$?
submission_id=$(jq -er '.id | select(type == "string" and test("^[0-9a-fA-F]{8}-([0-9a-fA-F]{4}-){3}[0-9a-fA-F]{12}$")) | ascii_downcase' \
  "$work/submission.json" 2>/dev/null) || fail 'Submission returned no valid ID. Apple may have received it; check submission history before retrying.'
# Persist the ID before waiting so a timed-out job can be investigated without resubmitting.
jq -n --arg id "$submission_id" --arg target "$target" --arg archive "$archive_hash" --arg zip "$zip_hash" \
  '{submissionId: $id, target: $target, archiveSha256: $archive, submittedZipSha256: $zip}' > "$report/submission.json"
printf 'Apple submission ID: %s\n' "$submission_id"
[[ "$submit_status" == 0 ]] || fail "Submission command failed; check $submission_id before retrying."

wait_status=0
"${notary[@]}" 960 wait "$submission_id" "${auth[@]}" --timeout 15m --output-format json \
  > "$work/status.json" 2> "$work/wait.err" || wait_status=$?
# Download the log even for a rejected submission, not just for successful ones.
if ! "${notary[@]}" 120 log "$submission_id" "${auth[@]}" "$report/log.json" \
    > "$work/log.out" 2> "$work/log.err"; then
  fail "No completed notarization log for $submission_id. Publication is blocked; check this ID before retrying."
fi
[[ "$wait_status" == 0 ]] || fail "Notarization failed or timed out for $submission_id; see the saved log."
jq -e --arg id "$submission_id" '(.id | ascii_downcase) == $id and .status == "Accepted"' "$work/status.json" >/dev/null \
  || fail "Notarization was not Accepted for $submission_id."
jq -e --arg id "$submission_id" --arg sha "$zip_hash" \
  '(.jobId | ascii_downcase) == $id and .status == "Accepted" and (.sha256 | ascii_downcase) == $sha
   and (.issues == null or (.issues | type) == "array") and all(.issues[]?; .severity != "error")' \
  "$report/log.json" >/dev/null || fail 'Apple log does not confirm acceptance of the exact submitted ZIP.'
if jq -e '.issues | length > 0' "$report/log.json" >/dev/null; then
  printf 'Apple reported notarization warnings; inspect the saved log.\n' >&2
fi
[[ $(shasum -a 256 "$archive" | cut -d ' ' -f 1) == "$archive_hash" ]] || fail 'The release archive changed during notarization.'
codesign --verify --strict -R="$requirement" "$binary"
printf 'Notarization Accepted: %s (%s)\n' "$target" "$submission_id"
