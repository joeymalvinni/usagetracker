//! Claude model normalization and API-equivalent cost estimation.

pub(super) const CLAUDE_PRICING_SOURCE: &str = "bundled_anthropic_api_equivalent";
pub(super) const CLAUDE_PRICING_VERSION: &str = "anthropic-bundled-2026-07-11";
pub(super) const CLAUDE_PRICING_EFFECTIVE_FROM: &str = "2026-07-11";

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct ClaudeTokenTotals {
    pub(super) input: u64,
    pub(super) cache_creation: u64,
    pub(super) cache_creation_1h: u64,
    pub(super) cache_read: u64,
    pub(super) output: u64,
}

impl ClaudeTokenTotals {
    pub(super) fn total(self) -> u64 {
        self.input
            .saturating_add(self.cache_creation)
            .saturating_add(self.cache_read)
            .saturating_add(self.output)
    }
}

#[derive(Clone, Copy)]
struct ClaudePricing {
    input: f64,
    cache_creation: f64,
    cache_read: f64,
    output: f64,
    long_context_threshold: Option<u64>,
    long_context_input: Option<f64>,
    long_context_cache_creation: Option<f64>,
    long_context_cache_read: Option<f64>,
    long_context_output: Option<f64>,
}

pub(super) fn claude_cost_usd(model: &str, totals: ClaudeTokenTotals) -> Option<f64> {
    let pricing = claude_pricing(model)?;
    let threshold_tokens = totals
        .input
        .saturating_add(totals.cache_creation)
        .saturating_add(totals.cache_read);
    let long_context = pricing
        .long_context_threshold
        .is_some_and(|threshold| threshold_tokens > threshold);

    let input_rate = if long_context {
        pricing.long_context_input.unwrap_or(pricing.input)
    } else {
        pricing.input
    };
    let cache_creation_rate = if long_context {
        pricing
            .long_context_cache_creation
            .unwrap_or(pricing.cache_creation)
    } else {
        pricing.cache_creation
    };
    let cache_read_rate = if long_context {
        pricing
            .long_context_cache_read
            .unwrap_or(pricing.cache_read)
    } else {
        pricing.cache_read
    };
    let output_rate = if long_context {
        pricing.long_context_output.unwrap_or(pricing.output)
    } else {
        pricing.output
    };

    let cache_creation_1h = totals.cache_creation_1h.min(totals.cache_creation);
    let cache_creation_5m = totals.cache_creation.saturating_sub(cache_creation_1h);

    Some(
        totals.input as f64 * input_rate
            + totals.cache_read as f64 * cache_read_rate
            + cache_creation_5m as f64 * cache_creation_rate
            + cache_creation_1h as f64 * input_rate * 2.0
            + totals.output as f64 * output_rate,
    )
}

fn claude_pricing(model: &str) -> Option<ClaudePricing> {
    let model = normalize_claude_model(model);
    let standard = |input_per_million: f64, output_per_million: f64| ClaudePricing {
        input: input_per_million / 1_000_000.0,
        cache_creation: input_per_million * 1.25 / 1_000_000.0,
        cache_read: input_per_million * 0.1 / 1_000_000.0,
        output: output_per_million / 1_000_000.0,
        long_context_threshold: None,
        long_context_input: None,
        long_context_cache_creation: None,
        long_context_cache_read: None,
        long_context_output: None,
    };
    let long_context = |input_per_million: f64,
                        output_per_million: f64,
                        threshold: u64,
                        long_input_per_million: f64,
                        long_output_per_million: f64| ClaudePricing {
        input: input_per_million / 1_000_000.0,
        cache_creation: input_per_million * 1.25 / 1_000_000.0,
        cache_read: input_per_million * 0.1 / 1_000_000.0,
        output: output_per_million / 1_000_000.0,
        long_context_threshold: Some(threshold),
        long_context_input: Some(long_input_per_million / 1_000_000.0),
        long_context_cache_creation: Some(long_input_per_million * 1.25 / 1_000_000.0),
        long_context_cache_read: Some(long_input_per_million * 0.1 / 1_000_000.0),
        long_context_output: Some(long_output_per_million / 1_000_000.0),
    };

    Some(match model.as_str() {
        "claude-fable-5" => standard(10.00, 50.00),
        "claude-haiku-4-5" => standard(1.00, 5.00),
        "claude-opus-4-5" | "claude-opus-4-6" | "claude-opus-4-7" | "claude-opus-4-8" => {
            standard(5.00, 25.00)
        }
        "claude-sonnet-4-5" => long_context(3.00, 15.00, 200_000, 6.00, 22.50),
        "claude-sonnet-4-6" => standard(3.00, 15.00),
        "claude-opus-4-1" => standard(15.00, 75.00),
        _ => return None,
    })
}

pub(super) fn normalize_claude_model(model: &str) -> String {
    let model = model
        .strip_prefix("anthropic.")
        .unwrap_or(model)
        .trim()
        .split('@')
        .next()
        .unwrap_or(model);
    let model = model
        .split_once("-v")
        .filter(|(_, suffix)| {
            suffix
                .chars()
                .all(|character| character.is_ascii_digit() || character == ':')
        })
        .map(|(base, _)| base)
        .unwrap_or(model);

    // Check ASCII bytes before slicing so non-ASCII model names remain safe.
    let bytes = model.as_bytes();
    if bytes.len() > 9
        && bytes[bytes.len() - 9] == b'-'
        && bytes[bytes.len() - 8..].iter().all(u8::is_ascii_digit)
    {
        return model[..model.len() - 9].to_string();
    }

    model.to_string()
}
