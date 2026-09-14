# Releasing Telephone

Builds run on pull requests and pushes to `main`. A version tag builds the
same four archives and publishes a GitHub release only after every target
passes tests, Clippy, and packaged-binary smoke tests.

## Make a release

1. Update the package version in `Cargo.toml` and run `cargo check` to update
   `Cargo.lock`.
2. Add `releases/<version>.md` with release notes and update the displayed
   version in `docs/index.html`.
3. Merge and pull `main`, then run:

   ```sh
   bash scripts/release.sh --dry-run
   bash scripts/release.sh
   ```

The script requires Git, Rust, and `jq`. It checks that tracked files are
clean, `HEAD` matches `origin/main`, the release notes are committed, and the
version tag does not exist. Untracked files are not included in a release.
It runs tests and Clippy, creates an annotated `v<version>` tag, and pushes
that tag. It does not bump versions, merge code, or commit files for you.

Watch the **Build and release** workflow in GitHub Actions. Release archives
have stable filenames so `/releases/latest/download/<filename>` links keep
working. Every release includes `SHA256SUMS`, and every archive includes the
binary, README, and MIT license.

## Targets

| Runner | Rust target |
| --- | --- |
| macOS ARM64 | `aarch64-apple-darwin` |
| macOS Intel | `x86_64-apple-darwin` |
| Linux x86-64 | `x86_64-unknown-linux-musl` |
| Linux ARM64 | `aarch64-unknown-linux-musl` |

Builds use the compiler pinned in `rust-toolchain.toml` and `Cargo.lock`.
Linux uses a native musl compiler; macOS targets version 11 or later. macOS
downloads are ad-hoc signed but not notarized. There are no signing secrets
configured. Windows is unsupported because the adapters use Unix APIs.

To package the native macOS build locally:

```sh
rustup target add aarch64-apple-darwin
MACOSX_DEPLOYMENT_TARGET=11.0 bash scripts/package.sh aarch64-apple-darwin
```

## Failed builds and retries

Re-run failed GitHub Actions jobs for an infrastructure failure. If source
changes are needed, commit them and choose a new version; do not move release
tags. Publishing resumes an existing draft, but never replaces assets on a
published release. A failed tag push leaves the local tag in place and prints
the exact retry command.
