# Releasing UsageTracker

Git tags drive releases. A release is accepted only when the tag, Cargo workspace version, and app marketing version all agree. The workflow builds Apple Silicon and Intel artifacts, signs and verifies the binaries, generates SHA-256 checksums, and publishes the artifacts with the installer scripts.

## Signing and Gatekeeper

Local builds and the default release workflow use ad-hoc signing. To publish Developer ID releases, set the repository variable `APPLE_DEVELOPER_ID_RELEASES` to `true` and configure these repository secrets:

| Secret | Value |
| --- | --- |
| `APPLE_CERTIFICATE_BASE64` | Base64-encoded Developer ID Application certificate and private key exported as `.p12` |
| `APPLE_CERTIFICATE_PASSWORD` | Password protecting that export |
| `APPLE_SIGNING_IDENTITY` | Full Developer ID Application identity name, or its SHA-1 fingerprint |
| `APPLE_ID` | Apple ID used for notarization |
| `APPLE_TEAM_ID` | Developer team ID |
| `APPLE_APP_PASSWORD` | App-specific password for notarization |

The enabled workflow imports the identity into a temporary runner Keychain, signs the app, bundled daemon, and CLI with hardened runtime and timestamps, and submits the app and CLI together to Apple. Packaging requires an accepted notarization result, staples and validates the app ticket, and checks Gatekeeper before creating the final archives. Missing credentials or failed notarization stop the build; a signed release never falls back to ad-hoc signing. The runner credentials are removed even when a build fails.

The workflow follows [GitHub's certificate setup guidance](https://docs.github.com/en/actions/how-tos/deploy/deploy-to-third-party-platforms/sign-xcode-applications) and [Apple's notarization workflow](https://developer.apple.com/documentation/security/customizing-the-notarization-workflow). Keep the same Developer ID team and bundle identifiers across updates. Standalone CLI executables cannot carry stapled tickets; their notarization is available to macOS online.

The installer accepts legacy ad-hoc releases and verified Developer ID Application signatures. It checks published checksums, archive contents, bundle/signing identifiers, app/daemon/CLI team consistency, and signature integrity before replacing files. For signed apps, it also requires Gatekeeper acceptance. It does not remove quarantine attributes or alter macOS security settings. The installer and checksums share the GitHub Release trust boundary with the artifacts.

Until Developer ID releases are configured and published, downloaded ad-hoc releases may still need [first-launch approval](troubleshooting.md#opening-the-unnotarized-app). Packaging support alone does not notarize an existing download.

The dashboard's in-app updater checks GitHub's latest stable release, verifies the release's `install.sh` against `SHA256SUMS`, and runs that installer in app-only mode against the current app directory. Keep both files in every release; they are part of the update path as well as the command-line installation path.

## Publish a release

1. Update `version` under `[workspace.package]` in `Cargo.toml`.
2. Update `CFBundleShortVersionString` in `apps/UsageMenuBar/Info.plist` to the same version.
3. Move the user-visible changes into a versioned section in `CHANGELOG.md`.
4. Add the reviewed GitHub release body at `docs/releases/vMAJOR.MINOR.PATCH.md`. The release workflow publishes this file verbatim.
5. Commit the release preparation and run the normal local checks.
6. Create and push a matching tag:

```sh
git tag -a v0.1.0 -m "UsageTracker 0.1.0"
git push origin v0.1.0
```

Only stable `vMAJOR.MINOR.PATCH` tags are accepted. The GitHub Release receives these files:

- `UsageTracker-macos-arm64.zip`
- `UsageTracker-macos-x86_64.zip`
- `usage-macos-arm64.tar.gz`
- `usage-macos-x86_64.tar.gz`
- `install.sh` and `uninstall.sh`
- `SHA256SUMS`

### In-app release-note format

The menu bar app shows release highlights once, after an update relaunches the new version. It reads the same Markdown body that GitHub publishes, so do not maintain a separate in-app changelog.

For each new release, start the body with one short summary paragraph, followed by an exact `## Highlights` heading and one to six single-line bullets:

```markdown
UsageTracker 0.2.0 makes provider refreshes faster and easier to understand.

## Highlights

- Refresh individual providers without waiting on unrelated accounts.
- See clearer recovery guidance when credentials expire.
- Keep menu bar status current after your Mac wakes.
```

The app ignores later sections such as installation instructions and downloads. It caps display text and shows at most the first six highlight bullets, so put the most useful user-visible changes first. A missing summary, heading, or bullet makes the in-app card unavailable but never blocks installation.

## Test packaging locally

Build an ad-hoc-signed artifact without release credentials:

```sh
./scripts/package-release.sh aarch64-apple-darwin dist
```

Use `x86_64-apple-darwin` to test the Intel artifact. The script defaults to ad-hoc signing without selecting an installed identity automatically.

For a signed local release, first store your notary credentials with `xcrun notarytool store-credentials`, then run:

```sh
CODESIGN_IDENTITY='Developer ID Application: Your Name (TEAMID)' \
NOTARYTOOL_PROFILE='usagetracker-release' \
./scripts/package-release.sh aarch64-apple-darwin dist
```

Set `NOTARYTOOL_KEYCHAIN` if the profile is in a non-default Keychain. Run `python3 scripts/test-distribution.py` to test installer acceptance, rejection, and preservation behavior using synthetic archives. These tests do not contact Apple or replace a real app; a release still needs validation with actual signing credentials.
