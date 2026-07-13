# Releasing UsageTracker

Git tags drive releases. A release is accepted only when the tag, Cargo workspace version, and app marketing version all agree. The workflow builds Apple Silicon and Intel artifacts, signs them with Developer ID, submits each app to Apple's notary service, staples the tickets, generates checksums, and publishes the artifacts with the installer scripts.

## One-time GitHub setup

Add these Actions secrets to the repository:

| Secret | Value |
| --- | --- |
| `MACOS_CERTIFICATE_P12` | Base64-encoded export of the Developer ID Application certificate and private key |
| `MACOS_CERTIFICATE_PASSWORD` | Password used when exporting that `.p12` file |
| `APPLE_ID` | Apple ID used for notarization |
| `APPLE_TEAM_ID` | Apple Developer team identifier |
| `APPLE_APP_SPECIFIC_PASSWORD` | App-specific password for the Apple ID |

For example, copy a certificate export to the clipboard with:

```sh
base64 < DeveloperIDApplication.p12 | pbcopy
```

The release workflow deliberately fails if any credential is absent. It never publishes an ad-hoc-signed or unnotarized app.

## Publish a release

1. Update `version` under `[workspace.package]` in `Cargo.toml`.
2. Update `CFBundleShortVersionString` in `apps/UsageMenuBar/Info.plist` to the same version.
3. Update `CHANGELOG.md`, commit the release, and run the normal local checks.
4. Create and push a matching tag:

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

## Test packaging locally

An ad-hoc-signed build does not need release credentials:

```sh
./scripts/package-release.sh aarch64-apple-darwin dist
```

For a distributable local build, set `CODESIGN_IDENTITY` and either a `NOTARY_KEYCHAIN_PROFILE` or the three Apple notarization variables used by the workflow. Set `REQUIRE_SIGNING=1` and `REQUIRE_NOTARIZATION=1` to make missing credentials fatal.
