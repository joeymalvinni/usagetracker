# Changelog

UsageTracker is pre-1.0. This file records user-visible changes from protocol v3 forward; older history is available in Git.

## Unreleased

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
