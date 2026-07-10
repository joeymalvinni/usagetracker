//! OpenAI model pricing with a daily refreshed, validated on-disk cache.

use std::{
    collections::BTreeMap,
    hash::{DefaultHasher, Hash, Hasher},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use chrono::{DateTime, TimeDelta, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::providers::read_response_body;

const OPENAI_PRICING_URL: &str = "https://developers.openai.com/api/docs/pricing";
const PRICING_REFRESH_INTERVAL: TimeDelta = TimeDelta::hours(24);
const PRICING_RETRY_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const LONG_CONTEXT_THRESHOLD: u64 = 272_000;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub(super) struct CodexTokenRates {
    pub(super) input_per_million: f64,
    pub(super) cached_input_per_million: Option<f64>,
    pub(super) cache_write_per_million: Option<f64>,
    pub(super) output_per_million: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct CodexModelPricing {
    pub(super) standard: CodexTokenRates,
    pub(super) long_context: Option<CodexTokenRates>,
    pub(super) long_context_threshold: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct CodexPricingCatalog {
    source: String,
    fetched_at: Option<DateTime<Utc>>,
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

        Self {
            source: "bundled_fallback".to_string(),
            fetched_at: None,
            models,
        }
    }

    pub(super) fn pricing(&self, model: &str) -> Option<&CodexModelPricing> {
        self.models
            .get(model)
            .or_else(|| model_alias(model).and_then(|alias| self.models.get(alias)))
    }

    pub(super) fn source(&self) -> &str {
        &self.source
    }

    pub(super) fn fetched_at(&self) -> Option<DateTime<Utc>> {
        self.fetched_at
    }

    pub(super) fn revision(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.source.hash(&mut hasher);
        serde_json::to_string(&self.models)
            .unwrap_or_default()
            .hash(&mut hasher);
        hasher.finish()
    }

    fn is_fresh(&self, now: DateTime<Utc>) -> bool {
        self.fetched_at
            .is_some_and(|fetched_at| now - fetched_at < PRICING_REFRESH_INTERVAL)
    }

    fn with_remote_models(
        remote_models: BTreeMap<String, CodexModelPricing>,
        fetched_at: DateTime<Utc>,
    ) -> Self {
        let mut catalog = Self::bundled();
        catalog.models.extend(remote_models);
        catalog.source = "openai_pricing_page".to_string();
        catalog.fetched_at = Some(fetched_at);
        catalog
    }

    fn is_valid_cache(&self) -> bool {
        self.models.len() >= 3
            && self.models.values().all(valid_model_pricing)
            && self.pricing("gpt-5").is_some()
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

fn valid_rate(value: f64) -> bool {
    value.is_finite() && (0.0..=1_000_000.0).contains(&value)
}

fn valid_model_pricing(pricing: &CodexModelPricing) -> bool {
    let valid = |rates: CodexTokenRates| {
        valid_rate(rates.input_per_million)
            && rates.cached_input_per_million.is_none_or(valid_rate)
            && rates.cache_write_per_million.is_none_or(valid_rate)
            && valid_rate(rates.output_per_million)
    };
    valid(pricing.standard) && pricing.long_context.is_none_or(valid)
}

#[derive(Debug)]
struct PricingState {
    catalog: CodexPricingCatalog,
    last_attempt: Option<Instant>,
}

#[derive(Clone, Debug)]
pub(super) struct CodexPricingManager {
    client: reqwest::Client,
    cache_path: Option<PathBuf>,
    state: Arc<Mutex<PricingState>>,
}

impl CodexPricingManager {
    pub(super) fn new(client: reqwest::Client) -> Self {
        let cache_path = usage_core::paths::default_app_dir()
            .map(|directory| directory.join("openai-pricing.json"));
        let catalog = cache_path
            .as_deref()
            .and_then(load_cached_catalog)
            .unwrap_or_else(CodexPricingCatalog::bundled);
        Self {
            client,
            cache_path,
            state: Arc::new(Mutex::new(PricingState {
                catalog,
                last_attempt: None,
            })),
        }
    }

    pub(super) async fn catalog(&self) -> (CodexPricingCatalog, Option<String>) {
        let mut state = self.state.lock().await;
        let now = Utc::now();
        if state.catalog.is_fresh(now)
            || state
                .last_attempt
                .is_some_and(|attempt| attempt.elapsed() < PRICING_RETRY_INTERVAL)
        {
            return (state.catalog.clone(), None);
        }
        state.last_attempt = Some(Instant::now());

        match self.fetch_catalog(now).await {
            Ok(catalog) => {
                if let Some(path) = self.cache_path.as_deref() {
                    if let Err(err) = save_catalog(path, &catalog).await {
                        let warning = format!("OpenAI pricing cache could not be saved: {err}");
                        state.catalog = catalog.clone();
                        return (catalog, Some(warning));
                    }
                }
                state.catalog = catalog.clone();
                (catalog, None)
            }
            Err(err) => (
                state.catalog.clone(),
                Some(format!(
                    "OpenAI pricing refresh failed; using cached rates: {err}"
                )),
            ),
        }
    }

    async fn fetch_catalog(
        &self,
        fetched_at: DateTime<Utc>,
    ) -> anyhow::Result<CodexPricingCatalog> {
        let response = self.client.get(OPENAI_PRICING_URL).send().await?;
        anyhow::ensure!(
            response.status().is_success(),
            "pricing page returned HTTP {}",
            response.status().as_u16()
        );
        let body = read_response_body(response, "OpenAI pricing page")
            .await
            .map_err(|err| anyhow::anyhow!(err.to_string()))?;
        let html = std::str::from_utf8(&body)?;
        let models = parse_standard_pricing_html(html)?;
        Ok(CodexPricingCatalog::with_remote_models(models, fetched_at))
    }
}

fn load_cached_catalog(path: &std::path::Path) -> Option<CodexPricingCatalog> {
    let bytes = std::fs::read(path).ok()?;
    let catalog = serde_json::from_slice::<CodexPricingCatalog>(&bytes).ok()?;
    catalog.is_valid_cache().then_some(catalog)
}

async fn save_catalog(path: &std::path::Path, catalog: &CodexPricingCatalog) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    tokio::fs::write(&temporary, serde_json::to_vec_pretty(catalog)?).await?;
    tokio::fs::rename(temporary, path).await?;
    Ok(())
}

pub(super) fn parse_standard_pricing_html(
    html: &str,
) -> anyhow::Result<BTreeMap<String, CodexModelPricing>> {
    let latest_start = html
        .find("id=\"latest-models\"")
        .ok_or_else(|| anyhow::anyhow!("latest pricing section was missing"))?;
    let standard_marker = "data-content-switcher-pane=\"true\" data-value=\"standard\"";
    let batch_marker = "data-content-switcher-pane=\"true\" data-value=\"batch\"";
    let standard_start = html[latest_start..]
        .find(standard_marker)
        .map(|offset| latest_start + offset)
        .ok_or_else(|| anyhow::anyhow!("standard pricing table was missing"))?;
    let standard_end = html[standard_start..]
        .find(batch_marker)
        .map(|offset| standard_start + offset)
        .ok_or_else(|| anyhow::anyhow!("standard pricing table was incomplete"))?;
    let section = &html[standard_start..standard_end];

    let row_pattern = Regex::new(r"(?s)<tr[^>]*>(.*?)</tr>")?;
    let cell_pattern = Regex::new(r"(?s)<td[^>]*>(.*?)</td>")?;
    let tag_pattern = Regex::new(r"(?s)<[^>]*>")?;
    let mut models = parse_serialized_pricing_rows(section)?;
    for row in row_pattern.captures_iter(section) {
        let cells = cell_pattern
            .captures_iter(&row[1])
            .map(|cell| clean_html_cell(&cell[1], &tag_pattern))
            .collect::<Vec<_>>();
        if cells.len() != 9 {
            continue;
        }
        let name = cells[0].trim().to_ascii_lowercase();
        if !name.starts_with("gpt-") {
            continue;
        }
        let Ok(standard) = parse_rates(&cells[1..5]) else {
            continue;
        };
        let Ok(long_context) = parse_optional_rates(&cells[5..9]) else {
            continue;
        };
        models.insert(
            name,
            CodexModelPricing {
                standard,
                long_context,
                long_context_threshold: long_context.map(|_| LONG_CONTEXT_THRESHOLD),
            },
        );
    }
    anyhow::ensure!(
        models.len() >= 3 && models.values().all(valid_model_pricing),
        "standard pricing table did not contain enough valid model rows"
    );
    Ok(models)
}

fn parse_serialized_pricing_rows(
    section: &str,
) -> anyhow::Result<BTreeMap<String, CodexModelPricing>> {
    let props_pattern = Regex::new(r#"(?s)<astro-island[^>]*\sprops="([^"]+)""#)?;
    let mut models = BTreeMap::new();
    for capture in props_pattern.captures_iter(section) {
        let decoded = decode_html_entities(&capture[1]);
        let Ok(serialized) = serde_json::from_str::<Value>(&decoded) else {
            continue;
        };
        let decoded = decode_astro_value(&serialized);
        let Some(object) = decoded.as_object() else {
            continue;
        };
        if object.get("tier").and_then(Value::as_str) != Some("standard") {
            continue;
        }
        let Some(rows) = object.get("rows").and_then(Value::as_array) else {
            continue;
        };
        for row in rows {
            let Some(cells) = row.as_array() else {
                continue;
            };
            if cells.len() != 4 && cells.len() != 5 {
                continue;
            }
            let Some(raw_name) = cells[0].as_str() else {
                continue;
            };
            let name = raw_name
                .split_once(" (")
                .map_or(raw_name, |(name, _)| name)
                .trim()
                .to_ascii_lowercase();
            if !name.starts_with("gpt-") {
                continue;
            }
            let standard = if cells.len() == 5 {
                rates_from_values(&cells[1], &cells[2], Some(&cells[3]), &cells[4])
            } else {
                rates_from_values(&cells[1], &cells[2], None, &cells[3])
            };
            let Ok(standard) = standard else { continue };
            models.insert(
                name,
                CodexModelPricing {
                    standard,
                    long_context: None,
                    long_context_threshold: None,
                },
            );
        }
    }
    Ok(models)
}

fn decode_astro_value(value: &Value) -> Value {
    match value {
        Value::Array(values) if values.len() == 2 && values[0].as_u64() == Some(0) => {
            decode_astro_value(&values[1])
        }
        Value::Array(values) if values.len() == 2 && values[0].as_u64() == Some(1) => values[1]
            .as_array()
            .map(|items| Value::Array(items.iter().map(decode_astro_value).collect()))
            .unwrap_or(Value::Null),
        Value::Array(values) => Value::Array(values.iter().map(decode_astro_value).collect()),
        Value::Object(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), decode_astro_value(value)))
                .collect(),
        ),
        value => value.clone(),
    }
}

fn clean_html_cell(value: &str, tag_pattern: &Regex) -> String {
    decode_html_entities(&tag_pattern.replace_all(value, " "))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn decode_html_entities(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

fn rates_from_values(
    input: &Value,
    cached: &Value,
    cache_write: Option<&Value>,
    output: &Value,
) -> anyhow::Result<CodexTokenRates> {
    Ok(rates(
        required_json_price(input)?,
        optional_json_price(cached)?,
        cache_write.map(optional_json_price).transpose()?.flatten(),
        required_json_price(output)?,
    ))
}

fn required_json_price(value: &Value) -> anyhow::Result<f64> {
    optional_json_price(value)?.ok_or_else(|| anyhow::anyhow!("required price was missing"))
}

fn optional_json_price(value: &Value) -> anyhow::Result<Option<f64>> {
    match value {
        Value::Number(number) => number
            .as_f64()
            .filter(|price| valid_rate(*price))
            .map(Some)
            .ok_or_else(|| anyhow::anyhow!("price was invalid")),
        Value::String(value) => optional_price(value),
        Value::Null => Ok(None),
        _ => anyhow::bail!("price had an unsupported shape"),
    }
}

fn parse_rates(cells: &[String]) -> anyhow::Result<CodexTokenRates> {
    anyhow::ensure!(cells.len() == 4, "pricing row had the wrong width");
    Ok(rates(
        required_price(&cells[0])?,
        optional_price(&cells[1])?,
        optional_price(&cells[2])?,
        required_price(&cells[3])?,
    ))
}

fn parse_optional_rates(cells: &[String]) -> anyhow::Result<Option<CodexTokenRates>> {
    if cells.iter().all(|cell| cell.trim() == "-") {
        return Ok(None);
    }
    parse_rates(cells).map(Some)
}

fn required_price(value: &str) -> anyhow::Result<f64> {
    optional_price(value)?.ok_or_else(|| anyhow::anyhow!("required price was missing"))
}

fn optional_price(value: &str) -> anyhow::Result<Option<f64>> {
    let value = value.trim();
    if value == "-" {
        return Ok(None);
    }
    let price = value
        .trim_start_matches('$')
        .replace(',', "")
        .parse::<f64>()?;
    anyhow::ensure!(valid_rate(price), "price was invalid");
    Ok(Some(price))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_and_long_context_prices() {
        let html = r#"
          <div id="latest-models">
            <div data-content-switcher-pane="true" data-value="standard">
              <astro-island props="{&quot;tier&quot;:[0,&quot;standard&quot;],&quot;rows&quot;:[1,[[1,[[0,&quot;gpt-5.2&quot;],[0,1.75],[0,0.175],[0,14]]]]]}"></astro-island>
              <table><tbody>
                <tr><td>gpt-5.6-sol</td><td>$5.00</td><td>$0.50</td><td>$6.25</td><td>$30.00</td><td>$10.00</td><td>$1.00</td><td>$12.50</td><td>$45.00</td></tr>
                <tr><td>gpt-5.6-terra</td><td>$2.50</td><td>$0.25</td><td>$3.125</td><td>$15.00</td><td>$5.00</td><td>$0.50</td><td>$6.25</td><td>$22.50</td></tr>
                <tr><td>gpt-5.6-luna</td><td>$1.00</td><td>$0.10</td><td>$1.25</td><td>$6.00</td><td>$2.00</td><td>$0.20</td><td>$2.50</td><td>$9.00</td></tr>
                <tr><td>gpt-5.4-mini</td><td>$0.75</td><td>$0.075</td><td>-</td><td>$4.50</td><td>-</td><td>-</td><td>-</td><td>-</td></tr>
              </tbody></table>
            </div>
            <div data-content-switcher-pane="true" data-value="batch"></div>
          </div>
        "#;

        let models = parse_standard_pricing_html(html).unwrap();
        let sol = &models["gpt-5.6-sol"];
        assert_eq!(sol.standard.input_per_million, 5.0);
        assert_eq!(sol.standard.cache_write_per_million, Some(6.25));
        assert_eq!(sol.long_context.unwrap().output_per_million, 45.0);
        assert_eq!(models["gpt-5.2"].standard.output_per_million, 14.0);
        assert!(models["gpt-5.4-mini"].long_context.is_none());
    }

    #[test]
    fn bundled_catalog_covers_current_models_and_aliases() {
        let catalog = CodexPricingCatalog::bundled();
        assert!(catalog.pricing("gpt-5.6-sol").is_some());
        assert!(catalog.pricing("gpt-5.1-codex-max").is_some());
        assert!(catalog.pricing("not-a-model").is_none());
    }
}
