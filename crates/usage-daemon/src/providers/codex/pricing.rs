//! Versioned bundled OpenAI model pricing.
//!
//! Pricing is deliberately shipped with the daemon. Scraping the public pricing
//! page made collection depend on an unrelated HTML layout, required cache and
//! retry machinery, and made historical estimates change without a release.

use std::collections::BTreeMap;

const LONG_CONTEXT_THRESHOLD: u64 = 272_000;
const BUNDLED_CATALOG_EFFECTIVE_FROM: &str = "2026-07-11";
const BUNDLED_CATALOG_VERSION: &str = "bundled-2026-07-11";
// Increment whenever bundled rates or aliases change so cached file reports
// are repriced without restoring runtime catalog hashing.
const BUNDLED_CATALOG_REVISION: u64 = 20_260_711;

#[derive(Clone, Copy, Debug)]
pub(super) struct CodexTokenRates {
    pub(super) input_per_million: f64,
    pub(super) cached_input_per_million: Option<f64>,
    pub(super) cache_write_per_million: Option<f64>,
    pub(super) output_per_million: f64,
}

#[derive(Clone, Debug)]
pub(super) struct CodexModelPricing {
    pub(super) standard: CodexTokenRates,
    pub(super) long_context: Option<CodexTokenRates>,
    pub(super) long_context_threshold: Option<u64>,
}

#[derive(Clone, Debug)]
pub(super) struct CodexPricingCatalog {
    models: BTreeMap<String, CodexModelPricing>,
}

impl CodexPricingCatalog {
    pub(super) fn bundled() -> Self {
        let mut models = BTreeMap::new();
        let mut insert = |name: &str, input: f64, cached: Option<f64>, output: f64| {
            models.insert(name.to_string(), model(input, cached, None, output, None));
        };

        insert("gpt-5", 1.25, Some(0.125), 10.0);
        insert("gpt-5-mini", 0.25, Some(0.025), 2.0);
        insert("gpt-5-nano", 0.05, Some(0.005), 0.4);
        insert("gpt-5-pro", 15.0, None, 120.0);
        insert("gpt-5.1", 1.25, Some(0.125), 10.0);
        insert("gpt-5.2", 1.75, Some(0.175), 14.0);
        insert("gpt-5.2-pro", 21.0, None, 168.0);
        insert("gpt-5.3-codex", 1.75, Some(0.175), 14.0);
        insert("gpt-5.3-codex-spark", 0.0, Some(0.0), 0.0);
        insert("gpt-5.4-mini", 0.75, Some(0.075), 4.5);
        insert("gpt-5.4-nano", 0.2, Some(0.02), 1.25);

        models.insert(
            "gpt-5.4".to_string(),
            model(
                2.5,
                Some(0.25),
                None,
                15.0,
                Some(rates(5.0, Some(0.5), None, 22.5)),
            ),
        );
        models.insert(
            "gpt-5.4-pro".to_string(),
            model(
                30.0,
                None,
                None,
                180.0,
                Some(rates(60.0, None, None, 270.0)),
            ),
        );
        models.insert(
            "gpt-5.5".to_string(),
            model(
                5.0,
                Some(0.5),
                None,
                30.0,
                Some(rates(10.0, Some(1.0), None, 45.0)),
            ),
        );
        models.insert(
            "gpt-5.5-pro".to_string(),
            model(
                30.0,
                None,
                None,
                180.0,
                Some(rates(60.0, None, None, 270.0)),
            ),
        );
        models.insert(
            "gpt-5.6-sol".to_string(),
            model(
                5.0,
                Some(0.5),
                Some(6.25),
                30.0,
                Some(rates(10.0, Some(1.0), Some(12.5), 45.0)),
            ),
        );
        models.insert(
            "gpt-5.6-terra".to_string(),
            model(
                2.5,
                Some(0.25),
                Some(3.125),
                15.0,
                Some(rates(5.0, Some(0.5), Some(6.25), 22.5)),
            ),
        );
        models.insert(
            "gpt-5.6-luna".to_string(),
            model(
                1.0,
                Some(0.1),
                Some(1.25),
                6.0,
                Some(rates(2.0, Some(0.2), Some(2.5), 9.0)),
            ),
        );

        Self { models }
    }

    pub(super) fn pricing(&self, model: &str) -> Option<&CodexModelPricing> {
        self.models
            .get(model)
            .or_else(|| model_alias(model).and_then(|alias| self.models.get(alias)))
    }

    pub(super) fn source(&self) -> &'static str {
        "bundled_openai_api_equivalent"
    }

    pub(super) fn revision(&self) -> u64 {
        BUNDLED_CATALOG_REVISION
    }

    pub(super) fn version(&self) -> &'static str {
        BUNDLED_CATALOG_VERSION
    }

    pub(super) fn effective_from(&self) -> &'static str {
        BUNDLED_CATALOG_EFFECTIVE_FROM
    }
}

fn model(
    input: f64,
    cached: Option<f64>,
    cache_write: Option<f64>,
    output: f64,
    long_context: Option<CodexTokenRates>,
) -> CodexModelPricing {
    CodexModelPricing {
        standard: rates(input, cached, cache_write, output),
        long_context,
        long_context_threshold: long_context.map(|_| LONG_CONTEXT_THRESHOLD),
    }
}

fn rates(
    input: f64,
    cached: Option<f64>,
    cache_write: Option<f64>,
    output: f64,
) -> CodexTokenRates {
    CodexTokenRates {
        input_per_million: input,
        cached_input_per_million: cached,
        cache_write_per_million: cache_write,
        output_per_million: output,
    }
}

fn model_alias(model: &str) -> Option<&'static str> {
    Some(match model {
        "gpt-5-codex" => "gpt-5",
        "gpt-5.1-codex" | "gpt-5.1-codex-max" => "gpt-5.1",
        "gpt-5.2-codex" => "gpt-5.2",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_catalog_covers_current_models_and_aliases() {
        let catalog = CodexPricingCatalog::bundled();
        assert!(catalog.pricing("gpt-5.6-sol").is_some());
        assert!(catalog.pricing("gpt-5.1-codex-max").is_some());
        assert!(catalog.pricing("not-a-model").is_none());
    }
}
