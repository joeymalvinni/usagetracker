# Changelog

UsageTracker is pre-1.0. This file records user-visible changes from protocol v3 forward; older history is available in Git.

## Unreleased

## 0.1.4 — 2026-07-15

### CLI

- Redesigned the CLI around provider-focused views, including provider shortcuts such as `usage codex` and dedicated `summary`, `activity`, and scoped `status` commands.
- Improved response processing and preserved unavailable cost data instead of presenting unknown totals as zero.

### App

- Displayed Codex rate-limit reset credits consistently across account, provider, summary, and detail views.
- Unified the menu bar popover under one native glass shell.

### Usage tracking

- Added event-driven local usage overlays that refresh supported providers promptly as their local activity changes.

### Reliability

- Distinguished and retried Keychain authentication failures so temporary credential-access errors can recover automatically.
- Resolved the Codex executable through the login shell when UsageTracker starts outside a terminal.
- Prevented duplicate threshold alerts when providers revise future reset timestamps.

### Installation

- Added clearer installer progress, verification, completion, and troubleshooting feedback while preserving existing checksum and code-signature checks.

## 0.1.3 — 2026-07-13

### App

- Made onboarding, Settings, and dashboard provider details follow the capabilities and setup fields advertised by the daemon.
- Improved app and installer restarts so an old daemon is fully stopped before its replacement starts.

### Reliability

- Serialized Keychain access in isolated, time-bounded helper processes so a stalled credential request cannot wedge the daemon.
- Cached successful Keychain reads briefly to prevent overlapping discovery and refresh work from showing duplicate authorization prompts.
- Reworked provider collection behind a shared adapter model with stricter provider-owned configuration validation and more consistent account lifecycle behavior.

## 0.1.2 — 2026-07-13

### App

- Added a compact dashboard update button that appears when a newer stable GitHub release is available, verifies the published installer checksum, and updates the current app bundle in place.
- Added a post-update card that summarizes the release and its highlights after the app relaunches.
- Refined the update button styling and progress state to make available updates easier to spot.

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
