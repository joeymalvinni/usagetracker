# Changelog

UsageTracker is pre-1.0. This file records user-visible changes from protocol v3 forward; older history is available in Git.

## Unreleased

## 0.1.10 — 2026-09-10

### App

- Added per-account Claude launch preferences with a confirmation sheet for working directory, model, effort, and permission flags. Permission bypass is use-once unless explicitly remembered.
- Added import of local Claude preferences, project trust, and prompt history into managed accounts while preserving account identity and MCP configuration.
- Added Claude authentication-code sign-in and improved sign-in cancellation.
- Showed account plan labels and clarified remaining allowance, model metrics, and notification wording.

### Usage tracking

- Included archived Codex sessions in local usage history and corrected account attribution, token accounting, and model pricing.
- Improved Claude usage collection with a profile-isolated terminal fallback and normalized quota meters.
- Distinguished collection failures and stale data from quota status, and improved automatic refresh and recovery after wake.

### Reliability

- Kept background Keychain access noninteractive and added explicit account-scoped permission recovery with bounded helpers and cache revalidation.
- Serialized Claude launch credential migration with token refresh and prevented stale credentials from replacing newer Keychain items.
- Unified configuration mutations, pooled SQLite reads, and typed usage snapshot details across the daemon and app.

### Protocol

- Added v3 account launch settings, authentication-code submission and cancellation, and Claude import methods with wire fixtures and schemas.

### Distribution

- Strengthened installer signature verification and added optional Developer ID signing and notarization support for release builds. This release remains ad-hoc signed.

## 0.1.9 — 2026-07-25

### App

- Reworked onboarding around explicit provider consent, with provider-by-provider connection flows, live account-discovery progress, and clearer tracking controls.
- Added tabs to Settings so general preferences and provider accounts are easier to navigate.
- Showed complete activity history and all-time totals in the activity grid, and added a hover indicator for the selected chart bar.

### Usage tracking

- Corrected Codex token totals to use the account's recorded activity instead of scaling local activity to an unrelated aggregate.

### Reliability

- Made first-run onboarding anchor to the menu bar item reliably after installation and added a local install test for the packaged app.
- Prevented temporary Claude backoff health from being presented as low usage.

## 0.1.8 — 2026-07-23

### App

- Made first-launch onboarding wait until the menu bar item is visible before opening, avoiding a startup race where the icon appeared without its popover.
- Standardized the app, updater, and background service under the `app.usagetracker` identity and restored the full UsageTracker app name.

### Installation

- Preserved installer-managed app and CLI upgrades across the identity migration by validating their install receipts and quitting the previously installed app by its actual bundle identifier.

## 0.1.7 — 2026-07-23

### App

- Added an optional activity grid alongside the existing bar chart, with the preferred chart style saved in Settings.
- Made Finder launches and app reopens reliably surface the menu bar popover, and refreshed onboarding with the UsageTracker app icon.

### Reliability

- Kept last-known usage visible while offline, skipped unavailable remote collection without overwriting provider health, and clearly marked cached values in the app and CLI.
- Prevented copy-link provider sign-in from opening a browser, including when a provider invokes the absolute macOS opener.

## 0.1.6 — 2026-07-23

### App

- Added Cursor to onboarding, Settings, dashboard summaries, provider details, and activity views.
- Made provider sign-in links available to copy when browser-based authentication needs manual follow-up.

### Usage tracking

- Added Cursor usage collection for included plan limits, Auto/API lanes, Enterprise personal caps and team pools, legacy request quotas, and personal or team on-demand budgets.
- Added complete Cursor billing-cycle usage-event collection with bounded pagination, individual event history, per-day and per-model costs, and vendor-versus-metered comparisons.

### Reliability

- Bound cached Cursor web sessions to validated account identities, re-read Cursor.app authentication during collection, and prevented account fallback after transient or rate-limit failures.
- Restored Codex rate-limit reset timestamps from app-server responses.
- Prevented stale menu bar app processes from surviving an update.

## 0.1.5 — 2026-07-19

### App

- Moved the daemon to a per-user macOS LaunchAgent so usage collection and the CLI continue working after the menu bar app closes.
- Reworked first-run setup so Codex starts enabled, other providers are inspected only after explicit opt-in, and captured provider sign-in links can be copied from the app.

### Usage tracking

- Corrected Codex activity and cost estimates to use local session logs, count cached input once, apply its discounted price, and avoid scaling local costs to opaque account totals.
- Added Claude scoped limits such as Fable and preserved five-hour and seven-day windows when canonical API responses need legacy utilization fallbacks.

### Reliability

- Cached successful Keychain reads for the daemon lifetime to avoid repeated authorization prompts, while invalidating Claude credential caches after managed sign-in completes.

### Installation

- Coordinated install, update, and uninstall operations with the LaunchAgent while preserving an explicitly disabled background service across updates.

### Development

- Added a Nix flake for building and running the daemon and CLI on macOS and Linux, plus a reproducible Rust development shell.

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
