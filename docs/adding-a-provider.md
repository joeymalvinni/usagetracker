# Adding a provider

A provider integration is a compile-time daemon adapter. The shared runtime
handles scheduling, storage, socket routing, configuration envelopes, capability
reporting, and generic CLI and menu-app presentation. Provider code owns
credentials, account identity, collection, normalization, and provider-specific
lifecycle actions.

This guide starts with the smallest safe integration and then covers optional
multi-account, setup, and local-usage features.

## Choose a reference provider

Read one existing integration before starting. Choose the closest shape rather
than the provider with the most similar API:

| Integration shape | Start with | Why |
| --- | --- | --- |
| One discovered account or workspace, web collection, optional setup | [`opencode`](../crates/usage-daemon/src/providers/opencode/) | Smallest profileless collector and a generic setup handler |
| Multiple managed CLI profiles | [`grok`](../crates/usage-daemon/src/providers/grok/) | Profile isolation, add/repair/delete handlers, and CLI-to-web fallback |
| Multiple file-backed profiles | [`codex`](../crates/usage-daemon/src/providers/codex/) | Profile paths, managed login, child process collection, and local logs |
| Multiple Keychain-backed OAuth profiles | [`claude`](../crates/usage-daemon/src/providers/claude/) | Keychain access, token refresh, CLI fallback, and local logs |

The provider behavior pages—[Codex](codex.md), [Claude](claude.md),
[OpenCode Go](opencode.md), and [Grok](grok.md)—describe the observable
credential, fallback, normalization, and failure rules that their code
implements.

## Files to change

A functional provider requires all of these changes:

1. Create `crates/usage-daemon/src/providers/<provider>/`.
2. Export the module from
   [`providers/mod.rs`](../crates/usage-daemon/src/providers/mod.rs).
3. Import and register its adapter in
   [`runtime/provider_registry.rs`](../crates/usage-daemon/src/runtime/provider_registry.rs).
4. Update the expected registry order in
   `production_registry_is_valid_and_ordered`. Registry order is API and UI
   order, so changing it is an intentional user-facing decision.
5. Add inline provider tests and a provider behavior page under `docs/`.
6. Add dependencies to `crates/usage-daemon/Cargo.toml` only when the provider
   cannot use existing workspace dependencies.

The provider directory normally contains:

```text
providers/<provider>/
├── mod.rs       # collector, orchestration, and normalized output
├── adapter.rs   # manifest, policy, construction, and optional handlers
├── settings.rs  # typed provider/profile settings
└── ...          # focused client, credential, parser, and profile modules
```

Exact provider IDs work automatically in the CLI, and unknown providers receive
generic menu-app presentation. Optional first-class presentation is covered in
[Polish the client presentation](#polish-the-client-presentation).

## Pick a stable identity

Define one snake-case ID and use it everywhere:

```rust
pub const PROVIDER_ID: &str = "acme_ai";
```

Use the constant in the manifest, collector, normalized snapshots, handlers,
profile paths, tests, and diagnostic metadata. Do not use an email, display
name, local username, or profile label as the account identity.

`DiscoveredAccount` has two distinct identities:

- `external_account_id` is the provider's stable account, organization, or
  workspace ID. It determines whether two discoveries are the same account.
- `profile_id` identifies the configured credential slot that found the
  account. Profileless providers use `None`.

If the provider does not expose a stable ID, document the fallback identity and
its limitations in the provider behavior page. Test duplicate credentials and
identity changes explicitly.

## Add typed settings

Provider configuration is a shared envelope with provider-owned flattened
settings. Keep those settings typed inside the provider:

```rust
// providers/acme/settings.rs
use serde::{Deserialize, Serialize};

use crate::{config::ProviderConfig, providers::settings_accessors};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AcmeSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) organization_id: Option<String>,
}

pub(crate) fn validate(config: &ProviderConfig) -> anyhow::Result<()> {
    provider(config)?;
    for (index, profile) in config.profiles.iter().enumerate() {
        profile.ensure_settings_empty(&format!("Acme profile at index {index}"))?;
    }
    Ok(())
}

settings_accessors!(provider: AcmeSettings);
```

Every provider and profile settings struct must use
`#[serde(deny_unknown_fields)]`. Declare every flattened field through
`ProviderAdapter::provider_setting_keys` or `profile_setting_keys`. The config
loader removes and persists fields the registered adapter does not own; fields
the adapter owns but cannot decode fail validation visibly.

For a provider with no provider-level settings, call
`config.ensure_settings_empty(...)`. Do the equivalent for profiles when the
provider is profileless. For a multi-profile example, see
[`claude/settings.rs`](../crates/usage-daemon/src/providers/claude/settings.rs).

Do not put secrets in config merely because the envelope can hold strings.
Prefer the existing Keychain, credential-file, or filtered browser-cookie
helpers and document the credential precedence.

## Implement the collector

The collector contract is
[`ProviderCollector`](../crates/usage-daemon/src/providers/mod.rs). A safe
starting module can compile and register before network collection exists:

```rust
// providers/acme/mod.rs
use async_trait::async_trait;
use usage_core::ProviderId;

use crate::{
    config::ProviderConfig,
    providers::{
        AccountDiscovery, CollectionOutcome, DiscoveredAccount, ProviderCollector,
        ProviderError, ProviderErrorKind,
    },
};

pub const PROVIDER_ID: &str = "acme_ai";

pub(crate) mod adapter;
pub(crate) mod settings;

pub(crate) struct AcmeCollector;

impl AcmeCollector {
    pub(crate) fn new(config: ProviderConfig) -> anyhow::Result<Self> {
        settings::validate(&config)?;
        Ok(Self)
    }
}

#[async_trait]
impl ProviderCollector for AcmeCollector {
    fn provider_id(&self) -> ProviderId {
        ProviderId::new(PROVIDER_ID)
    }

    fn configured_profile_ids(&self) -> Vec<String> {
        // Return every enabled configured profile ID for a profile-based
        // provider. Profileless providers explicitly return an empty list.
        Vec::new()
    }

    async fn discover_accounts(&self) -> Result<AccountDiscovery, ProviderError> {
        Err(ProviderError::new(
            ProviderErrorKind::ProviderUnavailable,
            "Acme account discovery is not implemented",
        ))
    }

    async fn collect_usage(
        &self,
        _account: &DiscoveredAccount,
    ) -> Result<CollectionOutcome, ProviderError> {
        Err(ProviderError::new(
            ProviderErrorKind::ProviderUnavailable,
            "Acme usage collection is not implemented",
        ))
    }
}
```

Keep the manifest disabled by default until discovery and collection are
implemented. The explicit errors above are safe during incremental development;
do not leave `todo!()`, `unimplemented!()`, or panic paths in a registered
collector.

### Discover every configured profile

`discover_accounts` identifies accounts; it must not persist them or collect
usage. A profileless success usually returns:

```rust
Ok(vec![DiscoveredAccount {
    external_account_id: identity.account_id,
    display_name: identity.display_name,
    email: identity.email,
    profile_id: None,
}]
.into())
```

A profile-based collector must:

1. Return all enabled, non-deleted profile IDs from `configured_profile_ids`.
2. Return exactly one `ProfileDiscovery` result for every configured ID,
   including failures.
3. Put the matching ID in both the discovery result and the discovered
   account's `profile_id`.
4. Deduplicate accounts that resolve to the same `external_account_id` according
   to a documented deterministic rule.

Use `AccountDiscovery::from_parts` with `AccountDiscoveryFailure` for mixed
success and failure. Do not return a top-level `Err` merely because one profile
failed; doing so loses the successful profiles and the per-profile diagnosis.
Coordinator tests enforce the complete-profile contract before storage changes.

### Normalize collection results

`collect_usage` returns a `CollectionOutcome`. The normal provider-reported path
is:

```rust
use chrono::Utc;
use serde_json::json;
use usage_core::{ProviderId, UsageWindow, UsageWindowKind};

use crate::providers::{
    CollectionOutcome, ProviderCollectionResult, ProviderUsage,
};

let collection = ProviderCollectionResult {
    usage: ProviderUsage {
        provider_id: ProviderId::new(PROVIDER_ID),
        collected_at: Utc::now(),
        windows: vec![UsageWindow {
            window_id: "acme_weekly".to_string(),
            label: "Weekly".to_string(),
            kind: UsageWindowKind::Weekly,
            used: None,
            limit: None,
            remaining: None,
            percent_used: Some(percent_used.clamp(0.0, 100.0)),
            percent_remaining: Some((100.0 - percent_used).clamp(0.0, 100.0)),
            reset_at,
        }],
        metadata: json!({
            "plan": plan_name,
        }),
    },
    daily_usage: Vec::new(),
    collection_mode: "acme_usage_api".to_string(),
    account_email: account_email,
    warnings: Vec::new(),
};

Ok(CollectionOutcome::collected(collection))
```

Use `UsageAmount` and `UsageUnit` for windows that report requests, tokens,
credits, or currency instead of percentages.

Normalization rules:

- Keep `window_id` stable across releases. Labels may change; IDs are storage
  and forecast identities.
- Determine whether the upstream percentage means used or remaining, populate
  both normalized fields when possible, and clamp provider rounding noise to
  `0..100`.
- Convert resets to `DateTime<Utc>` at the parser boundary.
- Use checked or saturating arithmetic for provider counters.
- Keep `collection_mode` stable and specific enough to distinguish fallback
  paths.
- Store only bounded, structured, non-secret diagnostics in `metadata`. Do not
  retain raw provider payloads.
- Preserve account-wide provider data as authoritative. Device-local estimates
  belong in supplemental datasets, not the authoritative result.

If the authoritative path fails but a local source succeeds, return
`CollectionOutcome::degraded(error, supplemental)`. Supplemental data must
never turn an authentication or rate-limit failure into healthy provider state.

If the provider estimates cost from model token counts, follow
[Maintaining model pricing](model-pricing.md). Unknown models must remain
unpriced, and catalog changes need explicit versioning, cache invalidation, and
historical-repricing decisions.

## Map failures deliberately

Map failures to the smallest accurate `ProviderErrorKind`:

| Kind | Use it for |
| --- | --- |
| `CredentialsMissing` | No usable credential exists |
| `CredentialsInvalid` | A configured credential is malformed or incomplete |
| `KeychainAccessFailed` | macOS denied or failed a Keychain operation |
| `Unauthorized` | The provider rejected otherwise well-formed credentials |
| `RateLimited` | The provider throttled the request |
| `Network` | DNS, connect, TLS, timeout, or response transport failure |
| `Parse` | A successful response has an unsupported or invalid shape |
| `ProviderUnavailable` | A required binary, endpoint, or provider feature is unavailable |

Attach the provider's retry deadline with `with_retry_at` when it is known. Use
the shared `retry_after_deadline` helper for standard HTTP `Retry-After`
headers. Rate limits do not fall through to another collection path: returning
`RateLimited` allows the coordinator to retain and honor shared backoff.

Error messages may reach logs and clients. Include the failed stage and safe
context, but never tokens, cookies, complete response bodies, Keychain secret
values, or unfiltered child-process output.

## Add the adapter

The adapter connects provider-owned behavior to the shared runtime:

```rust
// providers/acme/adapter.rs
use std::{sync::Arc, time::Duration};

use crate::{
    config::ProviderConfig,
    providers::ProviderCollector,
    runtime::provider_adapter::{
        ExecutionPolicy, ProviderAdapter, ProviderManifest,
    },
};

use super::{settings, AcmeCollector, PROVIDER_ID};

pub(crate) static ADAPTER: AcmeAdapter = AcmeAdapter;

pub(crate) struct AcmeAdapter;

impl ProviderAdapter for AcmeAdapter {
    fn manifest(&self) -> ProviderManifest {
        ProviderManifest {
            id: PROVIDER_ID,
            display_name: "Acme AI",
            minimum_refresh_interval_seconds: 60,
            default_visible: false,
        }
    }

    fn execution_policy(&self) -> ExecutionPolicy {
        ExecutionPolicy::new(
            Duration::from_secs(30), // account discovery
            Duration::from_secs(60), // one account collection
            1,                       // parallel accounts
        )
    }

    fn provider_setting_keys(&self) -> &'static [&'static str] {
        &["organization_id"]
    }

    fn validate_config(&self, config: &ProviderConfig) -> anyhow::Result<()> {
        settings::validate(config)
    }

    fn build_collector(
        &self,
        config: &ProviderConfig,
    ) -> anyhow::Result<Arc<dyn ProviderCollector>> {
        Ok(Arc::new(AcmeCollector::new(config.clone())?))
    }
}
```

Choose timeouts from measured worst cases, not by copying another provider.
`minimum_refresh_interval_seconds` is enforced even when the global interval is
shorter. `max_parallel_accounts` bounds collection within this provider.

Expose optional handlers only when the implementation exists:

- `AddAccountHandler` starts or resumes sign-in for another profile.
- `RepairHandler` reconnects an existing or provider-level account.
- `LaunchHandler` opens the provider tool in the selected account's isolated
  profile.
- `SetupHandler` describes and accepts generic select, text, or secret fields.
- `DeleteHandler` plans provider config changes and cleans provider-owned
  credentials or managed paths.

Capabilities are inferred from the handlers returned by the adapter; do not add
parallel capability booleans. The shared Rust and Swift code must not learn
provider-owned setup field keys.

For managed multi-account login, resume an existing pending profile before
creating another. A pending profile is enabled, not deleted, owned by
UsageTracker, and not represented by a connected account. Keep that selection
in a provider profile service and test resume versus create. Use
`plan_profile_deletion` and the shared managed-profile checks rather than
deleting an arbitrary configured path.

## Export and register the provider

Export the module:

```rust
// providers/mod.rs
pub mod acme;
```

Then add the adapter to the registry:

```rust
// runtime/provider_registry.rs
use crate::providers::acme::adapter::ADAPTER as ACME;

const PROVIDERS: &[&dyn ProviderAdapter] =
    &[&CODEX, &CLAUDE, &OPENCODE_GO, &GROK, &ACME];
```

Update the expected list in `production_registry_is_valid_and_ordered` to the
same order. Registry conformance tests check unique manifests, positive timing
and concurrency budgets, default config validation, matching collector IDs,
declared settings, local-watch validity, capability derivation, and generic
config-envelope round trips.

At construction, the registry checks that `ProviderCollector::provider_id`
matches the manifest. During collection, the coordinator also rejects snapshots
whose provider ID changes instead of persisting them.

## Secure network and process collection

Before enabling the provider:

- Restrict requests to fixed HTTPS hosts. Disable redirects or allow only
  same-host HTTPS redirects.
- Apply connect, request, and per-operation timeouts. The adapter execution
  policy is an outer bound, not a replacement for client timeouts.
- Read response bodies through `read_response_body` or enforce an equivalent
  bound before parsing.
- Bound pagination, retries, history lookback, concurrent requests, and
  in-memory records.
- Do not retry authentication failures. Retry transient requests only a small,
  explicit number of times.
- Never log request authorization headers, cookies, tokens, raw credential
  files, raw provider responses, or raw CLI output.
- For child processes, use a resolved executable, a minimal provider-specific
  environment, bounded stdout/stderr, a timeout, and guaranteed termination.
- Keep managed account homes isolated. Do not share global browser credentials
  with additional managed profiles unless account ownership is authoritative.
- Document credential precedence, remote hosts, stored diagnostics, and known
  limitations in `docs/<provider>.md`.

See [Security](security.md) and [Data and privacy](data-and-privacy.md) for the
project-wide trust and storage boundaries.

## Add local usage only when needed

Providers with reproducible local activity should implement both halves of the
local update contract:

1. Return a `LocalUsageWatch` from the adapter. Construct it with resolved roots,
   declarative `LocalUsagePathMatcher`s (`extension`, `file_name`, or `suffix`),
   and the source's real minimum scan interval. Use `with_timing` only when the
   default 30-second debounce and 60-second maximum latency are inappropriate.
   Shared watcher code must not contain provider file names or extensions.
2. Override `ProviderCollector::collect_local_usage`. It receives a stored
   account and the current composed snapshot, performs no remote requests, and
   returns only supplemental `UsageDataset`s with typed provenance.

Use `UsageDataset::supplemental_named` with a stable provider-owned source ID so
multiple local sources remain independently replaceable. A successful local
result is the complete current source set for that account: omitting a
previously returned source removes its overlay, while an error preserves the
last successful observation.

Filesystem events are dirty hints, not usage records. Reconcile the source from
disk so coalesced, missing, rename, delete, and overflow events remain correct.
The shared watcher batches independently per provider, bounds continuous-write
latency, and refreshes every matching target when roots overlap. Local
refreshes never mutate remote health or rate-limit backoff.

Full collection should return the same local observation through
`CollectionOutcome::collected_with_supplemental`. Do not merge local windows
into the authoritative `ProviderCollectionResult`; that would label
device-local estimates as account-wide provider reports.

Local overlays currently support snapshot windows and diagnostics. A provider
that needs local daily-usage buckets must first extend the source-scoped daily
storage model.

## Polish the client presentation

These changes are optional for functionality but expected for a polished
built-in provider:

| Surface | File | Purpose |
| --- | --- | --- |
| CLI aliases | [`usage-cli/src/selection.rs`](../crates/usage-cli/src/selection.rs) | Friendly aliases in addition to the exact provider ID |
| CLI labels | [`usage-cli/src/render/style.rs`](../crates/usage-cli/src/render/style.rs) | Display fallback and compact collection-mode labels |
| Menu name and SF Symbol | [`ProviderCatalog.swift`](../apps/UsageMenuBar/Sources/UsageMenuBar/State/ProviderCatalog.swift) | Short name and generic symbol |
| Menu palette and logo | [`ProviderBrand.swift`](../apps/UsageMenuBar/Sources/UsageMenuBar/DesignSystem/ProviderBrand.swift) | Provider color and optional bundled SVG |
| Synthetic UI data | [`fixtures.rs`](../crates/usage-daemon/src/fixtures.rs) | Provider-specific end-to-end UI inspection |

The daemon's ordered descriptor list remains authoritative. Without these
entries, the provider still appears, onboards, configures, and works using its
manifest display name or generic fallbacks.

Do not add provider branches to shared clients for data that belongs in the
typed API or generic setup descriptors. Provider-specific presentation is
appropriate only for branding, aliases, or a genuinely unique visualization.

## Test the integration

Keep tests inline with the code using `#[cfg(test)] mod tests`. At minimum, cover:

- credential precedence, missing credentials, and malformed credentials;
- stable account identity, duplicate identities, and all-profile discovery;
- successful response normalization, stable window IDs, used-versus-remaining
  semantics, reset parsing, and bounded values;
- 401/403, 429 with retry timing, transport failure, and unsupported response
  shapes;
- fallback eligibility, especially that rate limits never fall back;
- secret redaction and response-size, pagination, or child-output bounds;
- settings validation and migration;
- local-source reconciliation and provenance, when applicable;
- add/repair/setup/launch/delete behavior for every exposed handler.

Name provider tests with a consistent provider substring so they can be run
quickly:

```sh
cargo test -p usage-daemon acme
```

For manual verification, run the daemon against a disposable
`USAGE_TRACKER_HOME`, then query it from another terminal. Do not test a new
provider against your normal database or configuration first.

The `just fixture` recipe exercises the real socket and Swift app with synthetic
storage. Add the provider to `fixtures.rs` when its windows or metadata need
provider-specific UI inspection; the stock fixture does not invent data for a
new registration automatically.

## Required checks

Run the repository checks:

```sh
just check-rust
just check-swift
```

Their expanded commands are:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
swift build --package-path apps/UsageMenuBar -Xswiftc -strict-concurrency=complete
swift test --package-path apps/UsageMenuBar -Xswiftc -strict-concurrency=complete
```

Finally, inspect the provider in both clients:

```sh
cargo run -p usage-daemon
cargo run -p usage-cli -- providers
cargo run -p usage-cli -- refresh acme_ai
```

Before considering the integration complete, make sure the behavior page
answers: where credentials come from, how accounts are identified, how usage is
collected and normalized, which fallbacks are eligible, what rate limits do,
what diagnostics are stored, what each failure means, and what limitations
remain.
