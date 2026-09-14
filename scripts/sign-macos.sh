#!/usr/bin/env bash
# Sign one CI-built archive. Do not build or execute its contents with keys loaded.
set +x
set -euo pipefail
umask 077

fail() { printf 'sign-macos: %s\n' "$*" >&2; exit 1; }
[[ $# == 3 ]] || fail 'Usage: bash scripts/sign-macos.sh <target> <input.tar.gz> <output.tar.gz>'
target=$1
input=$2
output=$3
case "$target" in
  aarch64-apple-darwin) architecture=arm64 ;;
  x86_64-apple-darwin) architecture=x86_64 ;;
  *) fail "Unsupported signing target: $target" ;;
esac
[[ $(uname -s) == Darwin ]] || fail 'Signing requires macOS.'
[[ -f "$input" && ! -L "$input" ]] || fail 'Input must be a regular archive.'
[[ ! -e "$output" && ! -L "$output" ]] || fail 'Refusing to overwrite an existing output.'
[[ -n ${MACOS_CERTIFICATE_P12_BASE64:-} ]] || fail 'MACOS_CERTIFICATE_P12_BASE64 is required.'
[[ -n ${MACOS_CERTIFICATE_PASSWORD:-} ]] || fail 'MACOS_CERTIFICATE_PASSWORD is required.'
[[ ${MACOS_SIGNING_IDENTITY:-} =~ ^[A-F0-9]{40}$ ]] || fail 'MACOS_SIGNING_IDENTITY must be a certificate SHA-1 fingerprint.'
[[ ${MACOS_TEAM_ID:-} =~ ^[A-Z0-9]{10}$ ]] || fail 'MACOS_TEAM_ID must be a ten-character Apple team ID.'

# Reject extra paths, duplicate entries and traversal before extracting anything.
listing=$(tar -tzf "$input")
[[ "$listing" == $'telephone\nREADME.md\nSECURITY.md\nLICENSE' ]] || fail 'Unexpected archive contents.'
work=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/telephone-sign.XXXXXX")
keychain="$work/signing.keychain-db"
cleanup() {
  status=$?
  trap - EXIT
  if [[ -e "$keychain" ]]; then
    if ! security delete-keychain "$keychain"; then
      printf 'sign-macos: failed to delete the temporary signing keychain.\n' >&2
      status=1
    fi
  fi
  if ! rm -r -- "$work"; then
    printf 'sign-macos: failed to remove the temporary signing directory.\n' >&2
    status=1
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir "$work/package"
tar -xzf "$input" -C "$work/package" --no-same-owner --no-same-permissions
for name in telephone README.md SECURITY.md LICENSE; do
  [[ -f "$work/package/$name" && ! -L "$work/package/$name" ]] || fail "Not a regular file: $name"
  [[ $(stat -f %l "$work/package/$name") == 1 ]] || fail "Hard links are not allowed: $name"
done
binary="$work/package/telephone"
[[ $(lipo -archs "$binary") == "$architecture" ]] || fail 'Binary architecture does not match the signing target.'
chmod 755 "$binary"
chmod 644 "$work/package/README.md" "$work/package/SECURITY.md" "$work/package/LICENSE"

keychain_password=$(openssl rand -hex 32)
if [[ ${GITHUB_ACTIONS:-} == true ]]; then
  printf '::add-mask::%s\n' "$keychain_password"
fi
printf '%s' "$MACOS_CERTIFICATE_P12_BASE64" | base64 --decode > "$work/certificate.p12"
security create-keychain -p "$keychain_password" "$keychain"
security set-keychain-settings -lut 600 "$keychain"
security unlock-keychain -p "$keychain_password" "$keychain"
# Restrict this imported key to codesign; never grant every application access (-A).
security import "$work/certificate.p12" -k "$keychain" -P "$MACOS_CERTIFICATE_PASSWORD" \
  -f pkcs12 -T /usr/bin/codesign
rm "$work/certificate.p12"
unset MACOS_CERTIFICATE_P12_BASE64 MACOS_CERTIFICATE_PASSWORD
security set-key-partition-list -S apple-tool:,apple: -s -k "$keychain_password" "$keychain" >/dev/null
unset keychain_password
identities=$(security find-identity -v -p codesigning "$keychain" | awk '$1 ~ /^[0-9]+\)$/ { print $2 }')
[[ "$identities" == "$MACOS_SIGNING_IDENTITY" ]] || fail 'Expected only the pinned, valid signing identity.'

codesign --force --sign "$MACOS_SIGNING_IDENTITY" --keychain "$keychain" \
  --identifier com.github.iamnbutler.telephone --options runtime --timestamp "$binary"
# Require Apple's Developer ID Application certificate extension and our team,
# not just a cryptographically valid signature (which could still be ad-hoc).
requirement="anchor apple generic and certificate leaf[subject.OU] = \"$MACOS_TEAM_ID\" and certificate leaf[field.1.2.840.113635.100.6.1.13] exists"
codesign --verify --strict --verbose=2 -R="$requirement" "$binary"
signature=$(codesign --display --verbose=4 "$binary" 2>&1)
[[ "$signature" == *'flags=0x10000(runtime)'* ]] || fail 'Hardened runtime is missing.'
[[ "$signature" == *$'\nTimestamp='* ]] || fail 'Secure timestamp is missing.'

# Remove the key before packaging; the EXIT trap also covers every failure path.
security delete-keychain "$keychain"
mkdir -p "$(dirname "$output")"
COPYFILE_DISABLE=1 tar -czf "$output" -C "$work/package" telephone README.md SECURITY.md LICENSE
printf 'Developer ID signed: %s\n' "$output"
