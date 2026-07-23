# Maintaining model pricing

UsageTracker estimates cost from local token records for providers that do not
report an account bill. Pricing is bundled with the daemon so estimates are
deterministic within a release, work offline, and do not depend on scraping a
pricing page at collection time.

This is a maintainer guide for adding models, changing rates, managing aliases
and dated model IDs, and testing cost behavior. For the user-visible meaning of
cost and coverage, see the [API model reference](api/models.md#dashboard-and-forecasts).

## Know which kind of cost you have

Choose the data source before writing a price table:

| Available data | Treatment |
| --- | --- |
| Provider-reported billed or metered cost | Preserve the reported cost; do not recalculate it from model tokens |
| Local input/output/cache token counts | Estimate cost with a bundled catalog and mark it local, estimated, and potentially partial |
| Quota percentage or request allowance only | Do not infer currency cost |
| Tokens for an unknown model | Count them as unpriced; never treat a missing rate as free |

The Codex and Claude catalogs are API-equivalent estimates for locally observed
tool activity. They are not ChatGPT, Codex, Claude subscription invoices and do
not model enterprise contracts, negotiated discounts, batch pricing, priority
processing, taxes, or credits.

Provider-reported cost is a different path. For example, a provider usage event
that already contains metered USD should retain that value and provenance rather
than passing through one of these catalogs.

## Current pricing implementations

| Provider | Rates and model lookup | Token extraction and cost aggregation |
| --- | --- | --- |
| Codex | [`codex/pricing.rs`](../crates/usage-daemon/src/providers/codex/pricing.rs) | [`codex/cost.rs`](../crates/usage-daemon/src/providers/codex/cost.rs) |
| Claude | [`claude/pricing.rs`](../crates/usage-daemon/src/providers/claude/pricing.rs) | [`claude/cost.rs`](../crates/usage-daemon/src/providers/claude/cost.rs) |

Both implementations:

- normalize provider model strings before lookup;
- return `None` when no price is known;
- retain unknown-model tokens as unpriced coverage;
- expose pricing source, version, and effective date in diagnostics;
- calculate local estimates without converting them into provider-reported
  quota or account-wide billing.

They are not the same catalog abstraction. Codex uses a `BTreeMap` with explicit
aliases and an internal cache revision. Claude uses a normalized model `match`
and currently relies on process restart to clear its in-memory file cache.

## Verify a rate before changing code

Use a first-party provider source. Current starting points are:

- the [OpenAI model catalog](https://developers.openai.com/api/docs/models) and
  [model comparison](https://developers.openai.com/api/docs/models/compare)
- [Anthropic pricing](https://docs.anthropic.com/en/docs/about-claude/pricing)

Pricing changes over time. For every update:

1. Record the source URL and the date checked in the change description.
2. Confirm the currency and unit. Catalog inputs are normally USD per one
   million tokens.
3. Confirm the exact model or model family. Do not copy a sibling model's price
   because its name looks related.
4. Check every token class the local logs expose: ordinary input, cached input
   or cache reads, cache writes or creation, and output.
5. Check whether different cache durations have different multipliers.
6. Check whether a long-context threshold changes the rate, whether the
   threshold includes cache tokens, and whether crossing it reprices the whole
   request or only excess tokens.
7. Confirm that the price applies to standard synchronous API-equivalent
   traffic. Do not silently apply batch, priority, regional, or negotiated
   pricing.
8. Capture representative raw model identifiers from sanitized local records so
   normalization and alias tests reflect strings the tool actually emits.

If a first-party source is ambiguous, leave the model unpriced until the rule is
clear. A visible coverage gap is safer than a precise-looking incorrect total.

## Catalog metadata

Every estimated-cost provider should report:

- `pricing_source`: a stable identifier describing provenance, such as
  `bundled_openai_api_equivalent`;
- `pricing_version`: the bundled table revision visible in diagnostics;
- `pricing_effective_from`: the provider rate schedule's effective date.

`pricing_source` normally remains stable. Change `pricing_version` whenever a
rate, supported model, alias, or normalization rule changes the models that can
be priced. Set `pricing_effective_from` to the provider's effective date, not
automatically to the day the code was edited.

These fields describe the estimate; they do not select a historical rate.

### Historical-rate limitation

The current Codex and Claude implementations each bundle one active table.
Event timestamps are used for daily grouping, but not to select a price version.
After a catalog update and rescan, old local events can therefore be estimated
using the new table.

Before replacing an existing rate, choose and document one of these policies:

- The provider states that the new rate applies to the activity being scanned,
  so repricing the retained local history is acceptable.
- The displayed value is intentionally a current-rate estimate; call that out
  in the provider behavior page and release notes.
- Historical accuracy matters. In that case, first change the catalog to retain
  dated rate schedules and select a schedule using each event timestamp. Do not
  imply that `pricing_effective_from` already provides this behavior.

Adding a genuinely new model does not normally alter old priced rows, but a new
alias or broader normalization rule can convert previously unpriced historical
rows into priced rows.

## Adding or changing a Codex model

Codex pricing lives in
[`providers/codex/pricing.rs`](../crates/usage-daemon/src/providers/codex/pricing.rs).
Simple models use:

```rust
insert(
    "model-id",
    input_per_million,
    Some(cached_input_per_million),
    output_per_million,
);
```

The four values are:

1. canonical normalized model ID;
2. ordinary input USD per million tokens;
3. cached input USD per million tokens, or `None`;
4. output USD per million tokens.

In this catalog, `None` for cached input does not mean free. The calculator
falls back to the ordinary input rate. Use `Some(0.0)` only when a first-party
source establishes a zero rate.

Models with cache-write or long-context rates use `model(...)` and `rates(...)`
directly:

```rust
models.insert(
    "model-id".to_string(),
    model(
        input_per_million,
        Some(cached_input_per_million),
        Some(cache_write_per_million),
        output_per_million,
        Some(rates(
            long_input_per_million,
            Some(long_cached_input_per_million),
            Some(long_cache_write_per_million),
            long_output_per_million,
        )),
    ),
);
```

`model(...)` currently applies the shared `LONG_CONTEXT_THRESHOLD` whenever
long-context rates are present. If a new model uses another threshold, change
the data model to store that model's real threshold rather than forcing it
through the shared constant.

### Codex token accounting

`CodexTokenTotals.input` includes cached and cache-write input. The calculator
partitions it in this order:

1. clamp cached input to total input;
2. subtract cached input;
3. clamp cache-write input to the remaining non-cached input;
4. price the rest as ordinary input;
5. price output separately.

Visible total tokens are `input + output`; cached input is not added again.
Preserve these invariants when adding a token class or changing log parsing.

Long-context selection currently compares total input with the model threshold.
It applies the long-context rate set to the entire request when the threshold is
exceeded.

### Codex normalization and aliases

Codex model lookup has two stages:

1. `normalize_codex_model` in `codex/cost.rs` trims whitespace, removes an
   `openai/` prefix, and removes a trailing `-YYYY-MM-DD` snapshot date.
2. `model_alias` in `codex/pricing.rs` maps a known semantic alias to a canonical
   catalog entry.

Put structural variants in normalization and explicit product aliases in
`model_alias`. Avoid broad prefix matching: a future model with a similar prefix
may have a different price.

### Codex version and cache revision

When any rate, model, alias, or pricing-relevant normalization rule changes:

1. Update `BUNDLED_CATALOG_VERSION`.
2. Update `BUNDLED_CATALOG_EFFECTIVE_FROM` when the provider schedule's
   effective date changed.
3. Increment `BUNDLED_CATALOG_REVISION`.

The version and effective date appear in diagnostics. The numeric revision is
an internal cache key: changing it forces unchanged local session files to be
reparsed and repriced. It must change even when only an alias or normalization
rule changes.

## Adding or changing a Claude model

Claude pricing lives in
[`providers/claude/pricing.rs`](../crates/usage-daemon/src/providers/claude/pricing.rs).
Add the normalized ID to `claude_pricing`:

```rust
"claude-model-id" => standard(input_per_million, output_per_million),
```

`standard(...)` derives:

- five-minute cache creation at `1.25 ×` ordinary input;
- one-hour cache creation at `2 ×` ordinary input;
- cache reads at `0.1 ×` ordinary input.

Those are pricing rules, not universal constants. If the provider changes a
multiplier or a model differs, represent the explicit rates instead of
continuing to derive them.

For a model with premium long-context pricing, use:

```rust
"claude-model-id" => long_context(
    input_per_million,
    output_per_million,
    threshold_tokens,
    long_input_per_million,
    long_output_per_million,
),
```

The current calculator selects long-context pricing when ordinary input plus
cache creation plus cache reads exceeds the threshold. Once exceeded, all input,
cache, and output tokens in the row use the long-context rate family.

### Claude normalization

`normalize_claude_model` currently handles:

- an `anthropic.` prefix;
- an `@version` suffix;
- a numeric `-v...` suffix;
- a trailing eight-digit model date.

Add structural formats there. Add several exact IDs to the same pricing match
arm only when first-party pricing establishes that they share a rate. Unknown
normalized IDs must continue to return `None`.

### Claude version and cache behavior

When any Claude rate, supported model, or normalization rule changes:

1. Update `CLAUDE_PRICING_VERSION`.
2. Update `CLAUDE_PRICING_EFFECTIVE_FROM` when the provider schedule's
   effective date changed.
3. Keep `CLAUDE_PRICING_SOURCE` stable unless the provenance itself changed.

Claude currently passes revision `0` to the shared local-file cache. A released
binary update restarts the daemon and drops that in-memory cache, so the new
table is applied on the next scan. If Claude pricing ever becomes reloadable
without process restart, add a numeric `CLAUDE_PRICING_REVISION`, pass it to
`scan_cached_files`, and increment it for every pricing-relevant change, as the
Codex implementation does.

## Adding pricing to another provider

For a new provider with local token estimates:

1. Put pricing and model normalization in a focused `pricing.rs`, separate from
   transport and credential code.
2. Represent each token class actually emitted by the provider. Do not squeeze
   cache writes, image tokens, request units, or another billable dimension into
   ordinary input without evidence.
3. Return `Option<f64>` or an equivalent explicit result so unknown models
   remain unpriced.
4. Track priced tokens, unpriced tokens, and bounded normalized model names.
5. Attach `pricing_source`, `pricing_version`, and
   `pricing_effective_from` diagnostics.
6. Mark the dataset local, estimated, and partial when it only covers this
   machine or selected roots.
7. Give a cached local scan a pricing revision so catalog changes invalidate
   per-file summaries.
8. Describe the estimate and its limitations in `docs/<provider>.md`.

Follow the local-data and provenance rules in
[Adding a provider](adding-a-provider.md#add-local-usage-only-when-needed).

## Required tests

Every catalog change should test the behavior it introduces, not merely that a
lookup returns `Some`.

Add or update tests for:

- the exact normalized model identifiers observed in provider records;
- ordinary input and output cost using small, hand-checkable token totals;
- cached input or cache reads;
- each cache-write duration or class;
- values just below, at, and above a long-context threshold;
- aliases and dated/snapshot model names;
- an unknown model remaining unpriced;
- priced and unpriced token coverage in daily and per-model summaries;
- no double-counting of cached tokens;
- cache invalidation when a pricing revision changes;
- diagnostics carrying the new source, version, and effective date.

Use a tolerance for floating-point cost assertions:

```rust
assert!((actual - expected).abs() < 1e-12);
```

Compute `expected` independently from the production helper. A test that calls
the same helper twice will not detect a wrong rate or multiplier.

Run the focused provider tests:

```sh
cargo test -p usage-daemon codex
cargo test -p usage-daemon claude
```

Then run the repository checks:

```sh
just check-rust
```

If cost metadata or coverage presentation changed, also run:

```sh
just check-swift
just fixture
```

Inspect the CLI or fixture output for:

- the expected cost;
- a correct priced/unpriced coverage percentage;
- normalized, bounded model names;
- the new catalog version and effective date;
- no suggestion that an estimate is a provider invoice.

## Review checklist

Before merging a pricing change, confirm:

- [ ] A first-party source and verification date are recorded.
- [ ] Rates use the correct currency, per-token unit, and traffic class.
- [ ] All locally emitted token classes are handled without double-counting.
- [ ] Model normalization is narrow and covered by observed examples.
- [ ] Unknown models remain unpriced rather than zero-cost.
- [ ] Long-context boundaries and cache multipliers have boundary tests.
- [ ] Catalog version and effective date are accurate.
- [ ] The Codex cache revision was bumped, or equivalent invalidation exists.
- [ ] Historical repricing behavior was considered and disclosed.
- [ ] Provider documentation and release notes describe material estimate changes.
- [ ] Focused tests and repository checks pass.
