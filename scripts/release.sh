#!/usr/bin/env bash
# Tag the current, merged version. GitHub Actions builds and publishes it.
set -euo pipefail

fail() { printf 'release: %s\n' "$*" >&2; exit 1; }
dry_run=false
case "${1:-}" in
  --dry-run) dry_run=true ;;
  '') ;;
  *) fail 'Usage: bash scripts/release.sh [--dry-run]' ;;
esac
[[ $# -le 1 ]] || fail 'Too many arguments.'
for tool in git cargo jq; do
  command -v "$tool" >/dev/null || fail "Required tool not found: $tool"
done

repo_root=$(git -C "$(dirname "$0")/.." rev-parse --show-toplevel)
cd "$repo_root"
[[ $(git branch --show-current) == main ]] || fail 'Run from the main branch.'
git diff --quiet HEAD -- || fail 'Commit or set aside tracked changes first.'
git fetch origin main --tags
[[ $(git rev-parse HEAD) == "$(git rev-parse origin/main)" ]] || fail 'Push or fast-forward main first; HEAD must equal origin/main.'

version=$(cargo metadata --locked --no-deps --format-version 1 | jq -er '.packages[] | select(.name == "telephone") | .version')
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail 'Expected a stable x.y.z package version.'
tag="v$version"
[[ -f "releases/$version.md" ]] || fail "Add release notes at releases/$version.md."
git ls-files --error-unmatch "releases/$version.md" >/dev/null 2>&1 || fail 'Release notes must be committed.'
if git rev-parse --verify --quiet "refs/tags/$tag" >/dev/null; then
  fail "$tag already exists. Bump Cargo.toml and Cargo.lock for a new release."
fi

printf 'Release %s from %s\n' "$tag" "$(git rev-parse --short HEAD)"
if "$dry_run"; then
  printf 'Dry run: would test, create an annotated tag, and push it to origin.\n'
  exit 0
fi

cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
git tag -a "$tag" -m "Telephone $tag"
if ! git push origin "refs/tags/$tag"; then
  fail "Push failed; the local $tag tag was retained. Retry with: git push origin refs/tags/$tag"
fi
printf 'Tag pushed. Follow the Build and release workflow in GitHub Actions.\n'
