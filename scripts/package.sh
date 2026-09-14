#!/usr/bin/env bash
# Build and smoke-test a native release archive. The target must run on this host.
set -euo pipefail

[[ $# == 1 ]] || { printf 'Usage: bash scripts/package.sh <target>\n' >&2; exit 1; }
target=$1
case "$target" in
  aarch64-apple-darwin|x86_64-apple-darwin|aarch64-unknown-linux-musl|x86_64-unknown-linux-musl) ;;
  *) printf 'Unsupported release target: %s\n' "$target" >&2; exit 1 ;;
esac

repo_root=$(cd "$(dirname "$0")/.." && pwd)
cd "$repo_root"
version=$(cargo metadata --locked --no-deps --format-version 1 | jq -er '.packages[] | select(.name == "telephone") | .version')
cargo build --locked --release --target "$target"
binary="target/$target/release/telephone"
if [[ "$target" == *-apple-darwin ]]; then
  codesign --force --sign - "$binary"
  codesign --verify "$binary"
else
  # The Linux download must not depend on the build runner's glibc.
  file "$binary" | grep -Eq 'statically linked|static-pie linked'
fi
bash scripts/smoke-test.sh "$binary" "$version"

mkdir -p target/release-packages
staging=$(mktemp -d "$repo_root/target/release-packages/staging.XXXXXX")
install -m 755 "$binary" "$staging/telephone"
cp README.md SECURITY.md LICENSE "$staging/"
archive="$repo_root/target/release-packages/telephone-$target.tar.gz"
COPYFILE_DISABLE=1 tar -czf "$archive" -C "$staging" telephone README.md SECURITY.md LICENSE

# Test the packaged copy, not just the cargo build output.
extracted=$(mktemp -d "$repo_root/target/release-packages/check.XXXXXX")
tar -xzf "$archive" -C "$extracted"
[[ $("$extracted/telephone" --version) == "telephone $version" ]]
printf 'Packaged %s\n' "$archive"
