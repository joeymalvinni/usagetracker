# Adding a provider

A provider integration is a compile-time adapter. Adding one requires only a provider module and one entry in `runtime/provider_registry.rs`; shared polling, daemon routing, API capability reporting, config loading, and the menu app do not need provider-specific branches.

## Provider module

Create `crates/usage-daemon/src/providers/<provider>/` with:

- `adapter.rs`: manifest, execution policy, collector construction, optional local-usage watch roots, and any optional add/repair/launch/setup/delete handlers.
- `settings.rs`: typed provider and profile settings decoded from the shared config envelope.
- Collection code implementing `ProviderCollector`.
- Inline unit tests for parsing, identity, fallback, and error mapping.

The provider module must expose `pub const PROVIDER_ID: &str`; use that constant in its manifest, collector, handlers, profile paths, and tests so those identities cannot drift.

Every typed settings struct must use `#[serde(deny_unknown_fields)]`. This is a required correctness boundary: it makes misspelled supported settings and incomplete migrations fail visibly. Declare the corresponding flattened keys through the adapter's `provider_setting_keys` and `profile_setting_keys` methods. The loader warns, removes, and persists keys the adapter does not own so stale settings from an older release cannot brick startup; supported keys with invalid values still fail validation. Use the shared `settings_accessors!` helper for the standard typed provider/profile readers and mutation functions instead of copying them into each module.

The adapter's required `validate_config` method decodes all provider and profile settings before any runtime component is built. The collector must implement `configured_profile_ids` (profileless providers explicitly return an empty list) and return one `ProfileDiscovery` outcome for every configured profile, including when every profile fails. Collection returns a `CollectionOutcome`: authoritative data or an authoritative failure, plus any supplemental datasets. Every dataset declares typed source, scope, quality, completeness, and confidence. A supplemental success must never turn an authoritative rate limit or authentication failure into healthy state.

Capabilities are inferred from the handlers the adapter actually exposes. Do not add separate booleans. Setup handlers describe fields as generic select/text/secret controls and receive provider-owned key/value updates; shared Rust and Swift code must not learn those keys. Provider IDs returned by collectors are checked at construction, and every collected snapshot must retain that same ID or the coordinator records a parse failure without persisting it.

Multi-account login should resume an existing pending managed profile before creating another one. A pending profile is enabled, not deleted, managed by UsageTracker, and not represented by a connected account; providers may add a credential-file check when login completion has an authoritative on-disk marker. Keep this selection in the provider's profile service and cover the resume/create distinction with tests.

## Registry entry

Import the adapter and add its static instance to `PROVIDERS` in `runtime/provider_registry.rs`. Registry order is API and UI order. The registry supplies default config entries, server descriptors, execution budgets, collector construction, config migrations, and lifecycle dispatch.

The macOS app treats the ordered server descriptor list as authoritative. Optional entries in `ProviderCatalog.swift` add a custom short name, symbol, or palette, but they are not required for the provider to appear, onboard, configure, or work. Periodic polling honors the manifest's minimum refresh interval even when the global interval is shorter.

## Required checks

Run:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets
cargo test
swift test --package-path apps/UsageMenuBar
```

The registry conformance tests enforce unique manifests, matching collector IDs, real lifecycle handlers for every advertised capability, valid time/concurrency budgets, and config-envelope round trips. Coordinator tests enforce profile-complete discovery and degraded authoritative failures that retain backoff.
