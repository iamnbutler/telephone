# Releases

Merge a version bump and `releases/<version>.md`, then run
`bash scripts/release.sh` on `main`. The tag builds all four targets before
publishing their archives and checksums. Published releases are not overwritten.

macOS release archives are Developer ID signed in separate tag-only jobs.
PR and branch builds are ad-hoc signed and never receive signing secrets.

Repository Actions settings:

| Kind | Name | Value |
| --- | --- | --- |
| Secret | `MACOS_CERTIFICATE_P12_BASE64` | Base64 of a password-protected PKCS#12 containing one Developer ID Application identity |
| Secret | `MACOS_CERTIFICATE_PASSWORD` | PKCS#12 password |
| Variable | `MACOS_SIGNING_IDENTITY` | Certificate's uppercase SHA-1 fingerprint, used only to select the identity |
| Variable | `MACOS_TEAM_ID` | Ten-character Apple team ID |

Rotate the certificate secret, password and fingerprint together. Missing,
expired or mismatched credentials fail the release; there is no ad-hoc fallback.
Signing uses a temporary keychain, hardened runtime and a secure timestamp.
The keychain is deleted before smoke tests execute the signed binary.

Notarization is separate and is not configured yet. Existing v0.1.1 downloads
remain ad-hoc signed and unnotarized; do not change those release assets.

`bash scripts/test-sign-macos.sh` tests signing preflight and failed-import
cleanup using real macOS tools. A successful signing job also checks the Apple
Developer ID certificate type, pinned identity, team, runtime flag and timestamp.
