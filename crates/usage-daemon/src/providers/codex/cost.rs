//! Local Codex session scanning and estimated model cost calculation.

use std::{
    collections::BTreeMap,
    fs::File,
    hash::{DefaultHasher, Hash, Hasher},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Instant, UNIX_EPOCH},
};

use chrono::{DateTime, Days, NaiveDate, TimeZone, Utc};
use serde_json::{json, Value};
use usage_core::{UsageAmount, UsageUnit, UsageWindow, UsageWindowKind};

use crate::providers::ProviderUsage;

use super::{pricing::CodexPricingCatalog, CODEX_COST_SCAN_MIN_INTERVAL, COST_LOOKBACK_DAYS};

pub(super) trait CodexUsageCostExt {
    fn merge_cost_report(&mut self, report: CodexCostReport, include_token_activity: bool);
}

impl CodexUsageCostExt for ProviderUsage {
    fn merge_cost_report(&mut self, report: CodexCostReport, include_token_activity: bool) {
        if report.total_tokens == 0 {
            self.metadata["codex_cost"] = json!({
                "source": "local_session_logs",
                "estimate": true,
                "partial": true,
                "complete_lookback": false,
                "session_roots": report.session_roots,
                "files_scanned": report.files_scanned,
                "token_count_events": report.token_count_events,
                "baseline_seeded_events": report.baseline_seeded_events,
                "undated_tokens": report.undated_tokens,
                "undated_cost_usd": report.undated_cost_usd,
                "priced_tokens": report.priced_tokens,
                "unpriced_tokens": report.unpriced_tokens,
                "unpriced_models": unpriced_model_rows(&report.unpriced_models),
                "pricing_source": report.pricing_source,
                "pricing_fetched_at": report.pricing_fetched_at,
            });
            return;
        }

        if report.today_cost_usd > 0.0 {
            self.windows.push(cost_window(
                "codex_estimated_spend_today",
                "Codex estimated cost today",
                report.today_cost_usd,
            ));
        }
        if include_token_activity && report.today_tokens > 0 {
            self.windows.push(token_window(
                "codex_tokens_today",
                "Codex tokens today",
                report.today_tokens,
                UsageWindowKind::Daily,
            ));
        }

        if report.lookback_cost_usd > 0.0 {
            self.windows.push(cost_window(
                "codex_estimated_spend_30d",
                "Codex estimated cost 30 days",
                report.lookback_cost_usd,
            ));
        }
        if include_token_activity && report.lookback_tokens > 0 {
            self.windows.push(token_window(
                "codex_tokens_30d",
                "Codex tokens 30 days",
                report.lookback_tokens,
                UsageWindowKind::Monthly,
            ));
        }

        self.metadata["codex_cost"] = json!({
            "source": "local_session_logs",
            "estimate": true,
            "partial": true,
            "complete_lookback": false,
            "hint": "Estimated from this device's local Codex logs; account-wide token activity is tracked separately.",
            "session_roots": report.session_roots,
            "files_scanned": report.files_scanned,
            "token_count_events": report.token_count_events,
            "baseline_seeded_events": report.baseline_seeded_events,
            "today_cost_usd": report.today_cost_usd,
            "today_tokens": report.today_tokens,
            "lookback_days": COST_LOOKBACK_DAYS,
            "lookback_cost_usd": report.lookback_cost_usd,
            "lookback_tokens": report.lookback_tokens,
            "total_cost_usd": report.total_cost_usd,
            "total_tokens": report.total_tokens,
            "undated_cost_usd": report.undated_cost_usd,
            "undated_tokens": report.undated_tokens,
            "priced_tokens": report.priced_tokens,
            "unpriced_tokens": report.unpriced_tokens,
            "unpriced_models": unpriced_model_rows(&report.unpriced_models),
            "pricing_source": report.pricing_source,
            "pricing_fetched_at": report.pricing_fetched_at,
            "by_day": daily_cost_rows(&report.by_day),
            "by_model": report.by_model,
        });
    }
}

fn cost_window(window_id: &str, label: &str, value: f64) -> UsageWindow {
    UsageWindow {
        window_id: window_id.to_string(),
        label: label.to_string(),
        kind: UsageWindowKind::Credits,
        used: Some(UsageAmount {
            value,
            unit: UsageUnit::Usd,
        }),
        limit: None,
        remaining: None,
        percent_used: None,
        percent_remaining: None,
        reset_at: None,
    }
}

pub(super) fn token_window(
    window_id: &str,
    label: &str,
    tokens: u64,
    kind: UsageWindowKind,
) -> UsageWindow {
    UsageWindow {
        window_id: window_id.to_string(),
        label: label.to_string(),
        kind,
        used: Some(UsageAmount {
            value: tokens as f64,
            unit: UsageUnit::Tokens,
        }),
        limit: None,
        remaining: None,
        percent_used: None,
        percent_remaining: None,
        reset_at: None,
    }
}

#[derive(Clone, Debug)]
pub(super) struct CodexCostCache {
    fingerprint: CodexSessionFingerprint,
    pricing_revision: u64,
    report: CodexCostReport,
    scanned_at: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CodexCostCacheStatus {
    Hit,
    Throttled,
    Refreshed,
}

impl CodexCostCacheStatus {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Throttled => "throttled",
            Self::Refreshed => "refreshed",
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct CodexCostScan {
    pub(super) report: CodexCostReport,
    pub(super) cache_status: CodexCostCacheStatus,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct CodexSessionFingerprint {
    files: usize,
    total_size: u64,
    latest_modified_ns: u128,
    digest: u64,
}

#[derive(Clone, Debug, Default)]
pub(super) struct CodexCostReport {
    pub(super) session_roots: Vec<String>,
    pub(super) files_scanned: usize,
    pub(super) token_count_events: usize,
    pub(super) baseline_seeded_events: usize,
    pub(super) today_cost_usd: f64,
    pub(super) today_tokens: u64,
    pub(super) lookback_cost_usd: f64,
    pub(super) lookback_tokens: u64,
    pub(super) total_cost_usd: f64,
    pub(super) total_tokens: u64,
    pub(super) undated_cost_usd: f64,
    pub(super) undated_tokens: u64,
    pub(super) priced_tokens: u64,
    pub(super) unpriced_tokens: u64,
    pub(super) unpriced_models: BTreeMap<String, u64>,
    pub(super) pricing_source: String,
    pub(super) pricing_fetched_at: Option<DateTime<Utc>>,
    pub(super) by_day: BTreeMap<NaiveDate, DailyCostSummary>,
    pub(super) by_model: BTreeMap<String, CodexModelCostSummary>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct DailyCostSummary {
    pub(super) cost_usd: f64,
    pub(super) tokens: u64,
    pub(super) priced_tokens: u64,
    pub(super) unpriced_tokens: u64,
    pub(super) unpriced_models: BTreeMap<String, u64>,
}

#[derive(Debug, serde::Serialize)]
pub(super) struct DailyCostRow {
    date: String,
    cost_usd: f64,
    tokens: u64,
    priced_tokens: u64,
    unpriced_tokens: u64,
    unpriced_models: Vec<UnpricedModelRow>,
}

#[derive(Debug, serde::Serialize)]
struct UnpricedModelRow {
    model: String,
    tokens: u64,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub(super) struct CodexModelCostSummary {
    input_tokens: u64,
    cached_input_tokens: u64,
    cache_write_input_tokens: u64,
    output_tokens: u64,
    cost_usd: f64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct CodexTokenTotals {
    pub(super) input: u64,
    pub(super) cached: u64,
    pub(super) cache_write: u64,
    pub(super) output: u64,
}

impl CodexTokenTotals {
    pub(super) fn total(self) -> u64 {
        self.input.saturating_add(self.output)
    }

    fn saturating_delta(self, previous: Self) -> Self {
        Self {
            input: self.input.saturating_sub(previous.input),
            cached: self.cached.saturating_sub(previous.cached),
            cache_write: self.cache_write.saturating_sub(previous.cache_write),
            output: self.output.saturating_sub(previous.output),
        }
    }
}

pub(super) fn scan_codex_local_costs_cached(
    cache: Arc<Mutex<Option<CodexCostCache>>>,
    roots: Vec<PathBuf>,
    pricing: CodexPricingCatalog,
) -> anyhow::Result<CodexCostScan> {
    let fingerprint = codex_session_fingerprint(&roots)?;
    let pricing_revision = pricing.revision();
    let session_roots = roots
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    if let Some(cached) = cache
        .lock()
        .map_err(|_| anyhow::anyhow!("Codex cost cache mutex poisoned"))?
        .as_ref()
    {
        let same_roots = cached.report.session_roots == session_roots;
        if same_roots
            && cached.fingerprint == fingerprint
            && cached.pricing_revision == pricing_revision
        {
            return Ok(CodexCostScan {
                report: cached.report.clone(),
                cache_status: CodexCostCacheStatus::Hit,
            });
        }
        if same_roots
            && cached.pricing_revision == pricing_revision
            && cached.scanned_at.elapsed() < CODEX_COST_SCAN_MIN_INTERVAL
        {
            return Ok(CodexCostScan {
                report: cached.report.clone(),
                cache_status: CodexCostCacheStatus::Throttled,
            });
        }
    }

    let report = scan_codex_local_costs_from_roots(roots, &pricing)?;
    *cache
        .lock()
        .map_err(|_| anyhow::anyhow!("Codex cost cache mutex poisoned"))? = Some(CodexCostCache {
        fingerprint,
        pricing_revision,
        report: report.clone(),
        scanned_at: Instant::now(),
    });
    Ok(CodexCostScan {
        report,
        cache_status: CodexCostCacheStatus::Refreshed,
    })
}

fn scan_codex_local_costs_from_roots(
    roots: Vec<PathBuf>,
    pricing: &CodexPricingCatalog,
) -> anyhow::Result<CodexCostReport> {
    let today = Utc::now().date_naive();
    let lookback_start = today
        .checked_sub_days(Days::new(COST_LOOKBACK_DAYS.saturating_sub(1)))
        .unwrap_or(today);
    let mut report = CodexCostReport {
        session_roots: roots
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>(),
        pricing_source: pricing.source().to_string(),
        pricing_fetched_at: pricing.fetched_at(),
        ..Default::default()
    };

    for root in roots {
        collect_codex_session_files(&root, &mut |path| {
            report.files_scanned += 1;
            scan_codex_session_file(path, today, lookback_start, pricing, &mut report)
        })?;
    }

    Ok(report)
}

fn codex_session_fingerprint(roots: &[PathBuf]) -> anyhow::Result<CodexSessionFingerprint> {
    let mut files = Vec::new();
    for root in roots {
        collect_codex_session_files(root, &mut |path| {
            let metadata = std::fs::metadata(path)?;
            let modified_ns = metadata
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos())
                .unwrap_or_default();
            files.push((path.to_path_buf(), metadata.len(), modified_ns));
            Ok(())
        })?;
    }
    files.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    let mut digest = DefaultHasher::new();
    let mut fingerprint = CodexSessionFingerprint {
        files: files.len(),
        ..Default::default()
    };
    for (path, size, modified_ns) in files {
        path.hash(&mut digest);
        size.hash(&mut digest);
        modified_ns.hash(&mut digest);
        fingerprint.total_size = fingerprint.total_size.saturating_add(size);
        fingerprint.latest_modified_ns = fingerprint.latest_modified_ns.max(modified_ns);
    }
    fingerprint.digest = digest.finish();
    Ok(fingerprint)
}

pub(super) fn codex_session_roots(
    profile_home: &Path,
    local_codex_home: &Path,
    owns_default_activity: bool,
) -> Vec<PathBuf> {
    let mut roots = vec![profile_home.join("sessions")];
    if profile_home != local_codex_home && owns_default_activity {
        roots.push(local_codex_home.join("sessions"));
    }
    roots.sort();
    roots.dedup();
    roots
}

fn collect_codex_session_files(
    path: &Path,
    visit: &mut impl FnMut(&Path) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let Ok(entries) = std::fs::read_dir(path) else {
        return Ok(());
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_codex_session_files(&path, visit)?;
        } else if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
            visit(&path)?;
        }
    }
    Ok(())
}

pub(super) fn scan_codex_session_file(
    path: &Path,
    today: NaiveDate,
    lookback_start: NaiveDate,
    pricing: &CodexPricingCatalog,
    report: &mut CodexCostReport,
) -> anyhow::Result<()> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut current_model: Option<String> = None;
    let mut previous_totals: Option<CodexTokenTotals> = None;

    for line in reader.lines() {
        let line = line?;
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };

        if let Some(model) = codex_turn_context_model(&event) {
            current_model = Some(normalize_codex_model(model));
        }

        let Some(info) = codex_token_count_info(&event) else {
            continue;
        };

        report.token_count_events += 1;
        let (delta, baseline_seeded) = codex_token_delta(info, &mut previous_totals);
        if baseline_seeded {
            report.baseline_seeded_events = report.baseline_seeded_events.saturating_add(1);
        }

        if delta.total() == 0 {
            continue;
        }

        let model = current_model.as_deref().unwrap_or("unknown");
        let cost = codex_cost_usd_for_normalized_model(pricing, model, delta);
        let tokens = delta.total();
        let date = codex_event_date_in_timezone(&event, &Utc);

        report.total_tokens = report.total_tokens.saturating_add(tokens);
        if let Some(cost) = cost {
            report.total_cost_usd += cost;
            report.priced_tokens = report.priced_tokens.saturating_add(tokens);
        } else {
            report.unpriced_tokens = report.unpriced_tokens.saturating_add(tokens);
            add_unpriced_model(&mut report.unpriced_models, model, tokens);
        }

        if let Some(date) = date {
            if date == today {
                report.today_tokens = report.today_tokens.saturating_add(tokens);
                if let Some(cost) = cost {
                    report.today_cost_usd += cost;
                }
            }
            if date >= lookback_start && date <= today {
                report.lookback_tokens = report.lookback_tokens.saturating_add(tokens);
                if let Some(cost) = cost {
                    report.lookback_cost_usd += cost;
                }
                let day = report.by_day.entry(date).or_default();
                day.tokens = day.tokens.saturating_add(tokens);
                if let Some(cost) = cost {
                    day.cost_usd += cost;
                    day.priced_tokens = day.priced_tokens.saturating_add(tokens);
                } else {
                    day.unpriced_tokens = day.unpriced_tokens.saturating_add(tokens);
                    add_unpriced_model(&mut day.unpriced_models, model, tokens);
                }
            }
        } else {
            report.undated_tokens = report.undated_tokens.saturating_add(tokens);
            if let Some(cost) = cost {
                report.undated_cost_usd += cost;
            }
        }

        if !report.by_model.contains_key(model) {
            report
                .by_model
                .insert(model.to_string(), CodexModelCostSummary::default());
        }
        let summary = report
            .by_model
            .get_mut(model)
            .expect("model summary exists");
        summary.input_tokens = summary.input_tokens.saturating_add(delta.input);
        summary.cached_input_tokens = summary.cached_input_tokens.saturating_add(delta.cached);
        summary.cache_write_input_tokens = summary
            .cache_write_input_tokens
            .saturating_add(delta.cache_write);
        summary.output_tokens = summary.output_tokens.saturating_add(delta.output);
        if let Some(cost) = cost {
            summary.cost_usd += cost;
        }
    }

    Ok(())
}

pub(super) fn codex_token_delta(
    info: &Value,
    previous_totals: &mut Option<CodexTokenTotals>,
) -> (CodexTokenTotals, bool) {
    let total = info
        .get("total_token_usage")
        .and_then(codex_totals_from_value);
    let last = info
        .get("last_token_usage")
        .and_then(codex_totals_from_value);
    let (delta, baseline_seeded) = match (last, total, *previous_totals) {
        (Some(last), _, _) => (last, false),
        (None, Some(current), Some(previous)) => (current.saturating_delta(previous), false),
        (None, Some(_), None) => {
            // A total-only first event is an ambiguous cumulative baseline. Treat it as
            // pre-existing usage so resumed or copied rollouts cannot charge it again.
            (CodexTokenTotals::default(), true)
        }
        (None, None, _) => (CodexTokenTotals::default(), false),
    };

    if let Some(total) = total {
        *previous_totals = Some(total);
    } else if last.is_some() {
        *previous_totals = Some(previous_totals.unwrap_or_default().add(delta));
    }
    (delta, baseline_seeded)
}

fn daily_cost_rows(by_day: &BTreeMap<NaiveDate, DailyCostSummary>) -> Vec<DailyCostRow> {
    by_day
        .iter()
        .map(|(date, summary)| DailyCostRow {
            date: date.to_string(),
            cost_usd: summary.cost_usd,
            tokens: summary.tokens,
            priced_tokens: summary.priced_tokens,
            unpriced_tokens: summary.unpriced_tokens,
            unpriced_models: unpriced_model_rows(&summary.unpriced_models),
        })
        .collect()
}

fn add_unpriced_model(models: &mut BTreeMap<String, u64>, model: &str, tokens: u64) {
    let total = models.entry(model.to_string()).or_default();
    *total = total.saturating_add(tokens);
}

fn unpriced_model_rows(models: &BTreeMap<String, u64>) -> Vec<UnpricedModelRow> {
    models
        .iter()
        .map(|(model, tokens)| UnpricedModelRow {
            model: model.clone(),
            tokens: *tokens,
        })
        .collect()
}

pub(super) trait CodexTotalsAdd {
    fn add(self, delta: CodexTokenTotals) -> Self;
}

impl CodexTotalsAdd for CodexTokenTotals {
    fn add(self, delta: CodexTokenTotals) -> Self {
        Self {
            input: self.input.saturating_add(delta.input),
            cached: self.cached.saturating_add(delta.cached),
            cache_write: self.cache_write.saturating_add(delta.cache_write),
            output: self.output.saturating_add(delta.output),
        }
    }
}

pub(super) fn codex_token_count_info(event: &Value) -> Option<&Value> {
    if event.get("type").and_then(Value::as_str) == Some("token_count") {
        return event
            .get("info")
            .or_else(|| event.get("payload")?.get("info"));
    }

    let payload = event.get("payload")?;
    if payload.get("type").and_then(Value::as_str) == Some("token_count") {
        return payload.get("info");
    }
    None
}

pub(super) fn codex_turn_context_model(event: &Value) -> Option<&str> {
    if event.get("type").and_then(Value::as_str) == Some("turn_context") {
        return event
            .get("payload")
            .and_then(|payload| payload.get("model"))
            .and_then(Value::as_str);
    }

    let payload = event.get("payload")?;
    if payload.get("type").and_then(Value::as_str) == Some("turn_context") {
        return payload.get("payload")?.get("model").and_then(Value::as_str);
    }
    None
}

pub(super) fn codex_event_timestamp(event: &Value) -> Option<DateTime<Utc>> {
    let timestamp = event
        .get("timestamp")
        .and_then(Value::as_str)
        .or_else(|| event.get("payload")?.get("timestamp")?.as_str())?;
    DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

pub(super) fn codex_event_date_in_timezone<Tz: TimeZone>(
    event: &Value,
    timezone: &Tz,
) -> Option<NaiveDate> {
    codex_event_timestamp(event).map(|timestamp| timestamp.with_timezone(timezone).date_naive())
}

pub(super) fn codex_totals_from_value(value: &Value) -> Option<CodexTokenTotals> {
    Some(CodexTokenTotals {
        input: u64_from_json_value(value.get("input_tokens")?)?,
        cached: value
            .get("cached_input_tokens")
            .and_then(u64_from_json_value)
            .unwrap_or(0),
        cache_write: value
            .get("cache_write_tokens")
            .or_else(|| value.get("cache_creation_input_tokens"))
            .and_then(u64_from_json_value)
            .unwrap_or(0),
        output: u64_from_json_value(value.get("output_tokens")?)?,
    })
}

pub(super) fn u64_from_json_value(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64(),
        Value::String(value) => value.parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
pub(super) fn codex_cost_usd(model: &str, totals: CodexTokenTotals) -> Option<f64> {
    let model = normalize_codex_model(model);
    codex_cost_usd_for_normalized_model(&CodexPricingCatalog::bundled(), &model, totals)
}

fn codex_cost_usd_for_normalized_model(
    catalog: &CodexPricingCatalog,
    normalized_model: &str,
    totals: CodexTokenTotals,
) -> Option<f64> {
    let pricing = catalog.pricing(normalized_model)?;
    let cached = totals.cached.min(totals.input);
    let non_cached = totals.input.saturating_sub(cached);
    let cache_write = totals.cache_write.min(non_cached);
    let ordinary_input = non_cached.saturating_sub(cache_write);
    let rates = pricing
        .long_context_threshold
        .filter(|threshold| totals.input > *threshold)
        .and(pricing.long_context)
        .unwrap_or(pricing.standard);
    let cached_rate = rates
        .cached_input_per_million
        .unwrap_or(rates.input_per_million);
    let cache_write_rate = rates
        .cache_write_per_million
        .unwrap_or(rates.input_per_million);

    Some(
        ordinary_input as f64 * per_token(rates.input_per_million)
            + cached as f64 * per_token(cached_rate)
            + cache_write as f64 * per_token(cache_write_rate)
            + totals.output as f64 * per_token(rates.output_per_million),
    )
}

fn per_token(per_million: f64) -> f64 {
    per_million / 1_000_000.0
}

pub(super) fn normalize_codex_model(model: &str) -> String {
    let model = model.strip_prefix("openai/").unwrap_or(model).trim();
    if model.len() > 11 && model.as_bytes()[model.len() - 11] == b'-' {
        let suffix = &model[model.len() - 10..];
        if suffix.len() == 10
            && suffix.as_bytes()[4] == b'-'
            && suffix.as_bytes()[7] == b'-'
            && suffix
                .as_bytes()
                .iter()
                .enumerate()
                .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
        {
            return model[..model.len() - 11].to_string();
        }
    }
    model.to_string()
}
