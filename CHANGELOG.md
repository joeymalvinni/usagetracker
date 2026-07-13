# Changelog

UsageTracker is pre-1.0. This file records user-visible changes from protocol v3 forward; older history is available in Git.

## Unreleased

### App

- Added a compact dashboard update button that appears when a newer stable GitHub release is available, verifies the published installer checksum, and updates the current app bundle in place.

## 0.1.1 — 2026-07-12

### Onboarding

- Added a Keychain explanation before UsageTracker starts its daemon or triggers macOS permission prompts, including guidance to choose **Always Allow**.
- Added all-provider account discovery that automatically enables discovered accounts and providers.
- Added rescanning and clear discovery status and result messages.
- Preserved the existing startup flow for users who have already completed onboarding.

### Fixed

- Fixed provider switches in Settings so enabling or disabling a provider rebuilds app state, refreshes enabled providers, and reloads configuration correctly.

## 0.1.0 — 2026-07-12

### Documentation

- Replaced the README with a concise product and source-build guide.
- Added CLI, configuration, troubleshooting, security, privacy, provider, and protocol v3 references.
- Added generated request and response JSON Schemas with a Rust drift test.

### Distribution

- Added checksum-verifying app and CLI installer and uninstaller scripts.
- Added checksum-verified, ad-hoc-signed Apple Silicon and Intel artifacts for tagged GitHub releases.
- Documented that releases are not Apple-notarized and how to approve the app safely in Gatekeeper.

### Protocol v3

- Requires exact `api_version: 3` request/response envelopes.
- Added typed errors, combined `get_state`, usage provenance, provider capabilities, refresh jobs, and refresh coalescing.
- Added bounded request/response frames and persistent pipelined connections.
- CLI JSON remains a separate, envelope-free interface.

### Storage schema 1

- Consolidated the disposable local schema into `0001_initial.sql` with an application identifier.
- Positively identified legacy UsageTracker schemas are reset; unrelated non-empty databases are refused.
