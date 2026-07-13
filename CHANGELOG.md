# Changelog

UsageTracker is pre-1.0. This file records user-visible changes from protocol v3 forward; older history is available in Git.

## Unreleased

### Documentation

- Replaced the README with a concise product and source-build guide.
- Added CLI, configuration, troubleshooting, security, privacy, provider, and protocol v3 references.
- Added generated request and response JSON Schemas with a Rust drift test.

### Distribution

- Added checksum-verifying app and CLI installer and uninstaller scripts.
- Added signed, notarized Apple Silicon and Intel artifacts for tagged GitHub releases.

## Protocol v3 — 2026-07-12

- Requires exact `api_version: 3` request/response envelopes.
- Added typed errors, combined `get_state`, usage provenance, provider capabilities, refresh jobs, and refresh coalescing.
- Added bounded request/response frames and persistent pipelined connections.
- CLI JSON remains a separate, envelope-free interface.

## Storage schema 1 — 2026-07-12

- Consolidated the disposable local schema into `0001_initial.sql` with an application identifier.
- Positively identified legacy UsageTracker schemas are reset; unrelated non-empty databases are refused.
