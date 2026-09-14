# Releases

Merge a version bump and `releases/<version>.md`, then run
`bash scripts/release.sh` on `main`. The tag builds all four targets before
publishing their archives and checksums. Published releases are not overwritten.

macOS release archives are Developer ID signed and notarized in separate jobs.
PR and ordinary branch builds are ad-hoc signed and never receive credentials.
To test the full release pipeline without publishing, run
`gh workflow run build.yml --ref main`. Only a version-tag push can publish;
manual runs use credentials only when run against merged `main`.

Repository Actions settings:

| Kind | Name | Value |
| --- | --- | --- |
| Secret | `MACOS_CERTIFICATE_P12_BASE64` | Base64 of a password-protected PKCS#12 containing one Developer ID Application identity |
| Secret | `MACOS_CERTIFICATE_PASSWORD` | PKCS#12 password |
| Secret | `APPLE_ID` | Developer Apple Account email |
| Secret | `APPLE_APP_SPECIFIC_PASSWORD` | App-specific password for notarization, not the account's normal password |
| Variable | `MACOS_SIGNING_IDENTITY` | Certificate's uppercase SHA-1 fingerprint, used only to select the identity |
| Variable | `MACOS_TEAM_ID` | Ten-character Apple team ID |

Rotate the certificate secret, password and fingerprint together. Missing,
expired or mismatched credentials fail the release; there is no ad-hoc fallback.
Signing uses a temporary keychain, hardened runtime and a secure timestamp.
The keychain is deleted before smoke tests execute the signed binary.

Notarization validates the credentials, submits a ZIP of the exact signed binary,
and requires Apple's `Accepted` status and a matching ZIP hash in the log.
Authentication, upload, wait and log retrieval have wall-clock limits. Submission
IDs and logs are saved as workflow artifacts, including on failures. After a
timeout, check the saved ID before retrying: Apple may still be processing it.

Standalone binaries cannot have notarization tickets stapled to them; Gatekeeper
retrieves the ticket online. The distributed tar.gz is unchanged by notarization.
Existing v0.1.1 downloads remain ad-hoc signed and unnotarized; do not change them.

`bash scripts/test-sign-macos.sh` tests signing preflight and failed-import
cleanup using real macOS tools. A successful signing job also checks the Apple
Developer ID certificate type, pinned identity, team, runtime flag and timestamp.
`bash scripts/test-notarize-macos.sh` checks notarization preflight and the real
subprocess timeout/error paths without network requests or mocked tools.
