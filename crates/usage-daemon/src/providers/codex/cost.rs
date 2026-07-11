//! Local Codex session scanning and estimated model cost calculation.

use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use chrono::{DateTime, Local, NaiveDate, TimeZone, Utc};
use serde_json::{json, Value};
use usage_core::UsageWindowKind;

use crate::providers::{
    local_usage::{
        cost_window, daily_cost_rows, lookback_start, merge_daily_summary, scan_cached_files,
        token_window, CachedFile, DailyRollup, LocalFileCache, LocalFileScan,
    },
    ProviderUsage,
};

pub(super) use crate::providers::local_usage::DailyCostSummary;

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
                "pricing_version": report.pricing_version,
                "pricing_effective_from": report.pricing_effective_from,
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
            "pricing_version": report.pricing_version,
            "pricing_effective_from": report.pricing_effective_from,
            "pricing_fetched_at": report.pricing_fetched_at,
            "by_day": daily_cost_rows(&report.by_day),
            "by_model": report.by_model,
        });
    }
}

pub(super) type CodexCostCache = LocalFileCache<CodexFileCostReport, CodexCostReport>;
pub(super) type CodexCostScan = LocalFileScan<CodexCostReport>;
#[cfg(test)]
pub(super) type CodexCostCacheStatus = crate::providers::local_usage::CacheStatus;

#[derive(Clone, Debug)]
struct PricingIdentity {
    source: String,
    version: String,
    effective_from: String,
    fetched_at: Option<DateTime<Utc>>,
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
    pub(super) pricing_version: String,
    pub(super) pricing_effective_from: String,
    pub(super) pricing_fetched_at: Option<DateTime<Utc>>,
    pub(super) by_day: BTreeMap<NaiveDate, DailyCostSummary>,
    pub(super) by_model: BTreeMap<String, CodexModelCostSummary>,
}

/// Fully priced per-file scan covering every day the session file contains.
///
/// File reports are date-agnostic so a cached report can be re-folded against a
/// rolling `today`/lookback window without re-reading the file.
#[derive(Clone, Debug, Default)]
pub(super) struct CodexFileCostReport {
    token_count_events: usize,
    baseline_seeded_events: usize,
    total_cost_usd: f64,
    total_tokens: u64,
    undated_cost_usd: f64,
    undated_tokens: u64,
    priced_tokens: u64,
    unpriced_tokens: u64,
    unpriced_models: BTreeMap<String, u64>,
    by_day: BTreeMap<NaiveDate, DailyCostSummary>,
    by_model: BTreeMap<String, CodexModelCostSummary>,
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
    scan_codex_local_costs_cached_at(cache, roots, pricing, Local::now().date_naive())
}

fn scan_codex_local_costs_cached_at(
    cache: Arc<Mutex<Option<CodexCostCache>>>,
    roots: Vec<PathBuf>,
    pricing: CodexPricingCatalog,
    today: NaiveDate,
) -> anyhow::Result<CodexCostScan> {
    let pricing_revision = pricing.revision();
    let pricing_identity = PricingIdentity {
        source: pricing.source().to_string(),
        version: pricing.version(),
        effective_from: pricing.effective_from(),
        fetched_at: pricing.fetched_at(),
    };
    let session_roots = roots
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    scan_cached_files(
        cache,
        roots,
        "jsonl",
        pricing_revision,
        CODEX_COST_SCAN_MIN_INTERVAL,
        today,
        move |path| scan_codex_session_file_all_days(path, &pricing),
        move |file_order, files, report_date| {
            fold_codex_file_reports(
                &session_roots,
                file_order,
                files,
                report_date,
                &pricing_identity,
            )
        },
    )
}

fn fold_codex_file_reports(
    session_roots: &[String],
    file_order: &[PathBuf],
    files: &BTreeMap<PathBuf, CachedFile<CodexFileCostReport>>,
    today: NaiveDate,
    pricing: &PricingIdentity,
) -> CodexCostReport {
    let lookback_start = lookback_start(today, COST_LOOKBACK_DAYS);
    let mut report = CodexCostReport {
        session_roots: session_roots.to_vec(),
        files_scanned: file_order.len(),
        pricing_source: pricing.source.clone(),
        pricing_version: pricing.version.clone(),
        pricing_effective_from: pricing.effective_from.clone(),
        pricing_fetched_at: pricing.fetched_at,
        ..Default::default()
    };

    for path in file_order {
        let file = files
            .get(path)
            .expect("Codex cached file exists for every traversal entry");
        merge_codex_file_report(&mut report, file.summary(), today, lookback_start);
    }

    report
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

#[cfg(test)]
pub(super) fn scan_codex_session_file(
    path: &Path,
    today: NaiveDate,
    lookback_start: NaiveDate,
    pricing: &CodexPricingCatalog,
    report: &mut CodexCostReport,
) -> anyhow::Result<()> {
    let file_report = scan_codex_session_file_all_days(path, pricing)?;
    merge_codex_file_report(report, &file_report, today, lookback_start);
    Ok(())
}

fn scan_codex_session_file_all_days(
    path: &Path,
    pricing: &CodexPricingCatalog,
) -> anyhow::Result<CodexFileCostReport> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut current_model: Option<String> = None;
    let mut previous_totals: Option<CodexTokenTotals> = None;
    let mut report = CodexFileCostReport::default();

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
        let date = codex_event_date_in_timezone(&event, &Local);

        report.total_tokens = report.total_tokens.saturating_add(tokens);
        if let Some(cost) = cost {
            report.total_cost_usd += cost;
            report.priced_tokens = report.priced_tokens.saturating_add(tokens);
        } else {
            report.unpriced_tokens = report.unpriced_tokens.saturating_add(tokens);
            add_unpriced_model(&mut report.unpriced_models, model, tokens);
        }

        if let Some(date) = date {
            let day = report.by_day.entry(date).or_default();
            day.tokens = day.tokens.saturating_add(tokens);
            day.rows = day.rows.saturating_add(1);
            if let Some(cost) = cost {
                day.cost_usd += cost;
                day.priced_tokens = day.priced_tokens.saturating_add(tokens);
            } else {
                day.unpriced_tokens = day.unpriced_tokens.saturating_add(tokens);
                add_unpriced_model(&mut day.unpriced_models, model, tokens);
            }
        } else {
            report.undated_tokens = report.undated_tokens.saturating_add(tokens);
            if let Some(cost) = cost {
                report.undated_cost_usd += cost;
            }
        }

        let summary = report.by_model.entry(model.to_string()).or_default();
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

    Ok(report)
}

fn merge_codex_file_report(
    report: &mut CodexCostReport,
    file: &CodexFileCostReport,
    today: NaiveDate,
    lookback_start: NaiveDate,
) {
    report.token_count_events = report
        .token_count_events
        .saturating_add(file.token_count_events);
    report.baseline_seeded_events = report
        .baseline_seeded_events
        .saturating_add(file.baseline_seeded_events);
    report.total_cost_usd += file.total_cost_usd;
    report.total_tokens = report.total_tokens.saturating_add(file.total_tokens);
    report.undated_cost_usd += file.undated_cost_usd;
    report.undated_tokens = report.undated_tokens.saturating_add(file.undated_tokens);
    report.priced_tokens = report.priced_tokens.saturating_add(file.priced_tokens);
    report.unpriced_tokens = report.unpriced_tokens.saturating_add(file.unpriced_tokens);
    for (model, tokens) in &file.unpriced_models {
        add_unpriced_model(&mut report.unpriced_models, model, *tokens);
    }

    let daily = DailyRollup::from_range(&file.by_day, today, lookback_start);
    report.today_tokens = report.today_tokens.saturating_add(daily.today.tokens);
    report.today_cost_usd += daily.today.cost_usd;
    report.lookback_tokens = report.lookback_tokens.saturating_add(daily.lookback.tokens);
    report.lookback_cost_usd += daily.lookback.cost_usd;
    merge_daily_summary(&mut report.by_day, &daily.by_day);

    for (model, file_summary) in &file.by_model {
        let summary = report.by_model.entry(model.clone()).or_default();
        summary.input_tokens = summary
            .input_tokens
            .saturating_add(file_summary.input_tokens);
        summary.cached_input_tokens = summary
            .cached_input_tokens
            .saturating_add(file_summary.cached_input_tokens);
        summary.cache_write_input_tokens = summary
            .cache_write_input_tokens
            .saturating_add(file_summary.cache_write_input_tokens);
        summary.output_tokens = summary
            .output_tokens
            .saturating_add(file_summary.output_tokens);
        summary.cost_usd += file_summary.cost_usd;
    }
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

#[cfg(test)]
mod cache_tests {
    use std::{
        fs::OpenOptions,
        io::Write,
        time::{Duration, Instant},
    };

    use chrono::{Days, NaiveTime};

    use super::*;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "usagetracker-codex-cost-cache-{}",
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&path).expect("create test directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn test_date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).expect("valid test date")
    }

    fn timestamp_on(date: NaiveDate) -> String {
        Local
            .from_local_datetime(&date.and_time(NaiveTime::from_hms_opt(12, 0, 0).unwrap()))
            .single()
            .expect("local noon is unambiguous")
            .to_rfc3339()
    }

    fn token_event(date: NaiveDate, input: u64, output: u64) -> String {
        serde_json::to_string(&json!({
            "timestamp": timestamp_on(date),
            "type": "token_count",
            "info": {
                "last_token_usage": {
                    "input_tokens": input,
                    "cached_input_tokens": 0,
                    "output_tokens": output
                }
            }
        }))
        .unwrap()
    }

    fn scan(
        cache: Arc<Mutex<Option<CodexCostCache>>>,
        roots: Vec<PathBuf>,
        today: NaiveDate,
    ) -> anyhow::Result<CodexCostScan> {
        scan_codex_local_costs_cached_at(cache, roots, CodexPricingCatalog::bundled(), today)
    }

    fn expire(cache: &Arc<Mutex<Option<CodexCostCache>>>) {
        cache.lock().unwrap().as_mut().unwrap().scanned_at = Instant::now()
            .checked_sub(CODEX_COST_SCAN_MIN_INTERVAL + Duration::from_secs(1))
            .unwrap();
    }

    #[test]
    fn unchanged_codex_files_reuse_cached_file_report() {
        let root = TestDir::new();
        let path = root.path().join("session.jsonl");
        let today = test_date(2026, 7, 10);
        std::fs::write(&path, token_event(today, 10, 1)).unwrap();
        let cache = Arc::new(Mutex::new(None));

        let first = scan(cache.clone(), vec![root.0.clone()], today).unwrap();
        assert_eq!(first.cache_status, CodexCostCacheStatus::Refreshed);
        let first_file_report = cache
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .files
            .get(&path)
            .unwrap()
            .summary
            .clone();

        expire(&cache);
        let second = scan(cache.clone(), vec![root.0.clone()], today).unwrap();
        let second_file_report = cache
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .files
            .get(&path)
            .unwrap()
            .summary
            .clone();

        assert_eq!(second.cache_status, CodexCostCacheStatus::Hit);
        assert_eq!(second.report.total_tokens, 11);
        assert!(Arc::ptr_eq(&first_file_report, &second_file_report));
    }

    #[test]
    fn codex_cache_reparses_append_and_truncation_then_drops_deletion() {
        let root = TestDir::new();
        let path = root.path().join("session.jsonl");
        let today = test_date(2026, 7, 10);
        std::fs::write(&path, token_event(today, 10, 1)).unwrap();
        let cache = Arc::new(Mutex::new(None));

        let initial = scan(cache.clone(), vec![root.0.clone()], today).unwrap();
        assert_eq!(initial.report.total_tokens, 11);

        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "\n{}", token_event(today, 20, 2)).unwrap();
        drop(file);
        expire(&cache);
        let appended = scan(cache.clone(), vec![root.0.clone()], today).unwrap();
        assert_eq!(appended.cache_status, CodexCostCacheStatus::Refreshed);
        assert_eq!(appended.report.total_tokens, 33);

        std::fs::write(&path, token_event(today, 4, 1)).unwrap();
        expire(&cache);
        let truncated = scan(cache.clone(), vec![root.0.clone()], today).unwrap();
        assert_eq!(truncated.cache_status, CodexCostCacheStatus::Refreshed);
        assert_eq!(truncated.report.total_tokens, 5);

        std::fs::remove_file(&path).unwrap();
        expire(&cache);
        let deleted = scan(cache.clone(), vec![root.0.clone()], today).unwrap();
        assert_eq!(deleted.cache_status, CodexCostCacheStatus::Refreshed);
        assert_eq!(deleted.report.files_scanned, 0);
        assert_eq!(deleted.report.total_tokens, 0);
        assert!(cache.lock().unwrap().as_ref().unwrap().files.is_empty());
    }

    #[test]
    fn codex_cache_refolds_rolling_days_during_throttle_without_traversal() {
        let root = TestDir::new();
        let path = root.path().join("session.jsonl");
        let event_date = test_date(2026, 6, 1);
        std::fs::write(&path, token_event(event_date, 10, 1)).unwrap();
        let cache = Arc::new(Mutex::new(None));

        let same_day = scan(cache.clone(), vec![root.0.clone()], event_date).unwrap();
        assert_eq!(same_day.report.today_tokens, 11);
        assert_eq!(same_day.report.lookback_tokens, 11);

        let next_day = event_date.checked_add_days(Days::new(1)).unwrap();
        let rolled = scan(cache.clone(), vec![root.0.clone()], next_day).unwrap();
        assert_eq!(rolled.cache_status, CodexCostCacheStatus::Throttled);
        assert_eq!(rolled.report.today_tokens, 0);
        assert_eq!(rolled.report.lookback_tokens, 11);
        assert_eq!(rolled.report.by_day.len(), 1);

        let outside_lookback = event_date.checked_add_days(Days::new(30)).unwrap();
        let expired_day = scan(cache, vec![root.0.clone()], outside_lookback).unwrap();
        assert_eq!(expired_day.cache_status, CodexCostCacheStatus::Throttled);
        assert_eq!(expired_day.report.lookback_tokens, 0);
        assert!(expired_day.report.by_day.is_empty());
        assert_eq!(expired_day.report.total_tokens, 11);
    }
}
