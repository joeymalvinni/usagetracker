# Releasing UsageTracker

Git tags drive releases. A release is accepted only when the tag, Cargo workspace version, and app marketing version all agree. The workflow builds Apple Silicon and Intel artifacts, applies free ad-hoc code signatures, generates SHA-256 checksums, and publishes the artifacts with the installer scripts.

## Signing and Gatekeeper

Releases are not signed with an Apple Developer ID and are not submitted to Apple's notary service. No Apple Developer account, certificate, notarization credentials, or GitHub Actions secrets are required.

An ad-hoc signature lets `codesign` detect changes after packaging, but it does not prove the publisher's identity and Gatekeeper does not treat it as an identified-developer signature. Every GitHub Release and the README say this plainly. If macOS blocks the first launch, follow [Opening the unnotarized app](troubleshooting.md#opening-the-unnotarized-app); do not disable Gatekeeper globally.

The installer verifies the published checksums, expected archive contents, UsageTracker bundle and signing identifiers, and ad-hoc signature integrity. Checksums and artifacts are hosted by the same GitHub Release, so users still rely on GitHub and the repository account as the distribution trust boundary.

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

Build an ad-hoc-signed artifact without release credentials:

```sh
./scripts/package-release.sh aarch64-apple-darwin dist
```

Use `x86_64-apple-darwin` to test the Intel artifact. The packaging script intentionally forces ad-hoc signing even when other signing identities are installed on the build Mac.
