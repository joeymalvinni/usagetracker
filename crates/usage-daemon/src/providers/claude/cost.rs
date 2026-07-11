use std::{
    collections::{BTreeMap, HashMap},
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use chrono::{DateTime, Local, NaiveDate, Utc};
use serde_json::{json, Value};
use usage_core::UsageWindowKind;

use crate::providers::{
    local_usage::{
        cost_window, daily_cost_rows, lookback_start, merge_daily_summary, scan_cached_files,
        token_window, CachedFile, DailyCostSummary, DailyRollup, LocalFileCache, LocalFileScan,
    },
    ProviderUsage,
};

use super::pricing::{
    claude_cost_usd, normalize_claude_model, ClaudeTokenTotals, CLAUDE_PRICING_EFFECTIVE_FROM,
    CLAUDE_PRICING_SOURCE, CLAUDE_PRICING_VERSION,
};

const COST_LOOKBACK_DAYS: u64 = 30;
const COST_SCAN_MIN_INTERVAL: Duration = Duration::from_secs(60);

#[cfg(test)]
use crate::providers::local_usage::CacheStatus;

pub(super) fn merge_local_cost_report(usage: &mut ProviderUsage, report: ClaudeCostReport) {
    if report.total_tokens == 0 {
        usage.metadata["claude_cost"] = json!({
            "source": "local_project_logs",
            "estimate": true,
            "project_roots": report.project_roots,
            "files_scanned": report.files_scanned,
            "assistant_messages": report.assistant_messages,
            "unpriced_tokens": report.unpriced_tokens,
            "pricing_source": CLAUDE_PRICING_SOURCE,
            "pricing_version": CLAUDE_PRICING_VERSION,
            "pricing_effective_from": CLAUDE_PRICING_EFFECTIVE_FROM,
        });
        return;
    }

    if report.today_tokens > 0 {
        usage.windows.push(cost_window(
            "claude_estimated_spend_today",
            "Claude spend today",
            report.today_cost_usd,
        ));
        usage.windows.push(token_window(
            "claude_tokens_today",
            "Claude tokens today",
            report.today_tokens,
            UsageWindowKind::Daily,
        ));
    }

    if report.lookback_tokens > 0 {
        usage.windows.push(cost_window(
            "claude_estimated_spend_30d",
            "Claude spend 30 days",
            report.lookback_cost_usd,
        ));
        usage.windows.push(token_window(
            "claude_tokens_30d",
            "Claude tokens 30 days",
            report.lookback_tokens,
            UsageWindowKind::Monthly,
        ));
    }

    usage.metadata["claude_cost"] = json!({
        "source": "local_project_logs",
        "estimate": true,
        "hint": "Estimated from local Claude logs at API rates.",
        "project_roots": report.project_roots,
        "files_scanned": report.files_scanned,
        "assistant_messages": report.assistant_messages,
        "today_cost_usd": report.today_cost_usd,
        "today_tokens": report.today_tokens,
        "lookback_days": COST_LOOKBACK_DAYS,
        "lookback_cost_usd": report.lookback_cost_usd,
        "lookback_tokens": report.lookback_tokens,
        "total_cost_usd": report.total_cost_usd,
        "total_tokens": report.total_tokens,
        "unpriced_tokens": report.unpriced_tokens,
        "pricing_source": CLAUDE_PRICING_SOURCE,
        "pricing_version": CLAUDE_PRICING_VERSION,
        "pricing_effective_from": CLAUDE_PRICING_EFFECTIVE_FROM,
        "by_day": daily_cost_rows(&report.by_day),
        "by_model": report.by_model,
    });
}

#[derive(Clone, Debug, Default)]
pub(super) struct ClaudeCostReport {
    project_roots: Vec<String>,
    files_scanned: usize,
    assistant_messages: usize,
    today_cost_usd: f64,
    today_tokens: u64,
    lookback_cost_usd: f64,
    lookback_tokens: u64,
    total_cost_usd: f64,
    total_tokens: u64,
    unpriced_tokens: u64,
    by_day: BTreeMap<NaiveDate, DailyCostSummary>,
    by_model: BTreeMap<String, ClaudeModelCostSummary>,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
struct ClaudeModelCostSummary {
    input_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_creation_1h_input_tokens: u64,
    cache_read_input_tokens: u64,
    output_tokens: u64,
    cost_usd: f64,
}

#[derive(Clone, Debug)]
struct ClaudeUsageRow {
    key: String,
    model: String,
    timestamp: DateTime<Utc>,
    tokens: ClaudeTokenTotals,
}

pub(super) type ClaudeCostCache = LocalFileCache<ClaudeCostFileSummary, ClaudeCostReport>;
pub(super) type ClaudeCostScan = LocalFileScan<ClaudeCostReport>;

#[derive(Clone, Debug, Default)]
pub(super) struct ClaudeCostFileSummary {
    assistant_messages: usize,
    total_cost_usd: f64,
    total_tokens: u64,
    unpriced_tokens: u64,
    by_day: BTreeMap<NaiveDate, DailyCostSummary>,
    by_model: BTreeMap<String, ClaudeModelCostSummary>,
}

pub(super) fn scan_claude_local_costs_cached(
    cache: Arc<Mutex<Option<ClaudeCostCache>>>,
    configured_roots: Vec<PathBuf>,
) -> anyhow::Result<ClaudeCostScan> {
    scan_claude_local_costs_cached_on(cache, configured_roots, Local::now().date_naive())
}

fn scan_claude_local_costs_cached_on(
    cache: Arc<Mutex<Option<ClaudeCostCache>>>,
    configured_roots: Vec<PathBuf>,
    today: NaiveDate,
) -> anyhow::Result<ClaudeCostScan> {
    let roots = resolved_project_roots(configured_roots)?;
    let project_roots = roots
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    scan_cached_files(
        cache,
        roots,
        "jsonl",
        0,
        COST_SCAN_MIN_INTERVAL,
        today,
        scan_claude_project_file,
        move |file_order, files, report_date| {
            claude_cost_report(&project_roots, file_order, files, report_date)
        },
    )
}

fn resolved_project_roots(configured_roots: Vec<PathBuf>) -> anyhow::Result<Vec<PathBuf>> {
    let mut roots = if configured_roots.is_empty() {
        claude_project_roots()?
    } else {
        configured_roots
    };
    roots.sort();
    roots.dedup();
    Ok(roots)
}

fn claude_cost_report(
    project_roots: &[String],
    file_order: &[PathBuf],
    files: &BTreeMap<PathBuf, CachedFile<ClaudeCostFileSummary>>,
    today: NaiveDate,
) -> ClaudeCostReport {
    let lookback_start = lookback_start(today, COST_LOOKBACK_DAYS);
    let mut report = ClaudeCostReport {
        project_roots: project_roots.to_vec(),
        files_scanned: file_order.len(),
        ..Default::default()
    };

    for path in file_order {
        if let Some(file) = files.get(path) {
            add_claude_cost_file_summary(file.summary(), today, lookback_start, &mut report);
        }
    }

    report
}

fn claude_project_roots() -> anyhow::Result<Vec<PathBuf>> {
    let mut roots = Vec::new();
    if let Ok(value) = std::env::var("CLAUDE_CONFIG_DIR") {
        roots.extend(
            value
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| PathBuf::from(value).join("projects")),
        );
    }

    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("failed to resolve home directory for Claude logs"))?;
    roots.push(home.join(".config/claude/projects"));
    roots.push(home.join(".claude/projects"));
    roots.sort();
    roots.dedup();
    Ok(roots)
}

fn scan_claude_project_file(path: &Path) -> anyhow::Result<ClaudeCostFileSummary> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut rows = HashMap::<String, ClaudeUsageRow>::new();

    for line in reader.lines() {
        let line = line?;
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(row) = claude_usage_row(&event) else {
            continue;
        };
        rows.insert(row.key.clone(), row);
    }

    let mut summary = ClaudeCostFileSummary::default();
    for row in rows.into_values() {
        add_claude_usage_row(row, &mut summary);
    }

    Ok(summary)
}

fn add_claude_usage_row(row: ClaudeUsageRow, summary: &mut ClaudeCostFileSummary) {
    let tokens = row.tokens.total();
    if tokens == 0 {
        return;
    }

    summary.assistant_messages += 1;
    summary.total_tokens = summary.total_tokens.saturating_add(tokens);
    let cost = claude_cost_usd(&row.model, row.tokens);
    if let Some(cost) = cost {
        summary.total_cost_usd += cost;
    } else {
        summary.unpriced_tokens = summary.unpriced_tokens.saturating_add(tokens);
    }

    let date = row.timestamp.with_timezone(&Local).date_naive();
    let day = summary.by_day.entry(date).or_default();
    day.tokens = day.tokens.saturating_add(tokens);
    day.rows = day.rows.saturating_add(1);
    if let Some(cost) = cost {
        day.cost_usd += cost;
        day.priced_tokens = day.priced_tokens.saturating_add(tokens);
    } else {
        day.unpriced_tokens = day.unpriced_tokens.saturating_add(tokens);
        let unpriced = day
            .unpriced_models
            .entry(normalize_claude_model(&row.model))
            .or_default();
        *unpriced = unpriced.saturating_add(tokens);
    }

    let model = summary
        .by_model
        .entry(normalize_claude_model(&row.model))
        .or_default();
    model.input_tokens = model.input_tokens.saturating_add(row.tokens.input);
    model.cache_creation_input_tokens = model
        .cache_creation_input_tokens
        .saturating_add(row.tokens.cache_creation);
    model.cache_creation_1h_input_tokens = model
        .cache_creation_1h_input_tokens
        .saturating_add(row.tokens.cache_creation_1h);
    model.cache_read_input_tokens = model
        .cache_read_input_tokens
        .saturating_add(row.tokens.cache_read);
    model.output_tokens = model.output_tokens.saturating_add(row.tokens.output);
    if let Some(cost) = cost {
        model.cost_usd += cost;
    }
}

fn add_claude_cost_file_summary(
    summary: &ClaudeCostFileSummary,
    today: NaiveDate,
    lookback_start: NaiveDate,
    report: &mut ClaudeCostReport,
) {
    report.assistant_messages += summary.assistant_messages;
    report.total_tokens = report.total_tokens.saturating_add(summary.total_tokens);
    report.total_cost_usd += summary.total_cost_usd;
    report.unpriced_tokens = report
        .unpriced_tokens
        .saturating_add(summary.unpriced_tokens);

    let daily = DailyRollup::from_range(&summary.by_day, today, lookback_start);
    report.today_tokens = report.today_tokens.saturating_add(daily.today.tokens);
    report.today_cost_usd += daily.today.cost_usd;
    report.lookback_tokens = report.lookback_tokens.saturating_add(daily.lookback.tokens);
    report.lookback_cost_usd += daily.lookback.cost_usd;
    merge_daily_summary(&mut report.by_day, &daily.by_day);

    for (model, source_model) in &summary.by_model {
        let target_model = report.by_model.entry(model.clone()).or_default();
        target_model.input_tokens = target_model
            .input_tokens
            .saturating_add(source_model.input_tokens);
        target_model.cache_creation_input_tokens = target_model
            .cache_creation_input_tokens
            .saturating_add(source_model.cache_creation_input_tokens);
        target_model.cache_creation_1h_input_tokens = target_model
            .cache_creation_1h_input_tokens
            .saturating_add(source_model.cache_creation_1h_input_tokens);
        target_model.cache_read_input_tokens = target_model
            .cache_read_input_tokens
            .saturating_add(source_model.cache_read_input_tokens);
        target_model.output_tokens = target_model
            .output_tokens
            .saturating_add(source_model.output_tokens);
        target_model.cost_usd += source_model.cost_usd;
    }
}

fn claude_usage_row(event: &Value) -> Option<ClaudeUsageRow> {
    if event.get("type").and_then(Value::as_str) != Some("assistant") {
        return None;
    }

    let message = event.get("message")?;
    let model = message.get("model")?.as_str()?;
    if model == "<synthetic>" {
        return None;
    }
    let usage = message.get("usage")?;
    let tokens = claude_tokens_from_usage(usage)?;
    let timestamp = event
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(|timestamp| {
            DateTime::parse_from_rfc3339(timestamp)
                .ok()
                .map(|timestamp| timestamp.with_timezone(&Utc))
        })?;
    let message_id = message.get("id").and_then(Value::as_str);
    let request_id = event.get("requestId").and_then(Value::as_str);
    let uuid = event.get("uuid").and_then(Value::as_str).unwrap_or("");
    let key = match (message_id, request_id) {
        (Some(message_id), Some(request_id)) => format!("{message_id}:{request_id}"),
        (Some(message_id), None) => message_id.to_string(),
        _ => uuid.to_string(),
    };
    if key.is_empty() {
        return None;
    }

    Some(ClaudeUsageRow {
        key,
        model: model.to_string(),
        timestamp,
        tokens,
    })
}

fn claude_tokens_from_usage(usage: &Value) -> Option<ClaudeTokenTotals> {
    let cache_creation = u64_field(usage, "cache_creation_input_tokens").unwrap_or(0);
    let cache_creation_1h = usage
        .get("cache_creation")
        .and_then(|cache_creation| u64_field(cache_creation, "ephemeral_1h_input_tokens"))
        .unwrap_or(0);

    Some(ClaudeTokenTotals {
        input: u64_field(usage, "input_tokens")?,
        cache_creation,
        cache_creation_1h: cache_creation_1h.min(cache_creation),
        cache_read: u64_field(usage, "cache_read_input_tokens").unwrap_or(0),
        output: u64_field(usage, "output_tokens")?,
    })
}

fn u64_field(value: &Value, key: &str) -> Option<u64> {
    match value.get(key)? {
        Value::Number(number) => number.as_u64(),
        Value::String(value) => value.parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::{fs::OpenOptions, io::Write, time::Instant};

    use chrono::{Days, TimeZone};

    use super::*;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("claude-cost-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
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

    fn local_timestamp(date: NaiveDate) -> String {
        let local = Local
            .from_local_datetime(&date.and_hms_opt(12, 0, 0).unwrap())
            .single()
            .expect("local noon");
        local.to_rfc3339()
    }

    fn usage_event(id: &str, date: NaiveDate, input: u64, output: u64) -> String {
        json!({
            "type": "assistant",
            "timestamp": local_timestamp(date),
            "requestId": format!("req_{id}"),
            "message": {
                "id": format!("msg_{id}"),
                "model": "claude-sonnet-4-6",
                "usage": {
                    "input_tokens": input,
                    "output_tokens": output
                }
            }
        })
        .to_string()
    }

    fn write_usage_file(path: &Path, events: &[String]) {
        std::fs::write(path, format!("{}\n", events.join("\n"))).unwrap();
    }

    fn append_usage_event(path: &Path, event: &str) {
        let mut file = OpenOptions::new().append(true).open(path).unwrap();
        writeln!(file, "{event}").unwrap();
    }

    fn expire_cache(cache: &Arc<Mutex<Option<ClaudeCostCache>>>) {
        cache.lock().unwrap().as_mut().unwrap().scanned_at =
            Instant::now() - COST_SCAN_MIN_INTERVAL;
    }

    #[test]
    fn extracts_and_prices_assistant_usage_rows() {
        let event = json!({
            "type": "assistant",
            "timestamp": "2026-04-29T01:13:42.901Z",
            "requestId": "req_123",
            "message": {
                "id": "msg_123",
                "model": "claude-sonnet-4-6",
                "usage": {
                    "input_tokens": 3,
                    "cache_creation_input_tokens": 6193,
                    "cache_read_input_tokens": 10615,
                    "output_tokens": 120,
                    "cache_creation": {
                        "ephemeral_1h_input_tokens": 6193,
                        "ephemeral_5m_input_tokens": 0
                    }
                }
            }
        });

        let row = claude_usage_row(&event).expect("usage row");
        assert_eq!(row.key, "msg_123:req_123");
        assert_eq!(row.model, "claude-sonnet-4-6");
        assert_eq!(row.tokens.input, 3);
        assert_eq!(row.tokens.cache_creation, 6193);
        assert_eq!(row.tokens.cache_creation_1h, 6193);
        assert_eq!(row.tokens.cache_read, 10615);
        assert_eq!(row.tokens.output, 120);

        let cost = claude_cost_usd(&row.model, row.tokens).expect("priced row");
        assert!((cost - 0.0421515).abs() < 0.0000001);
    }

    #[test]
    fn ignores_synthetic_and_non_assistant_rows() {
        let synthetic = json!({
            "type": "assistant",
            "timestamp": "2026-04-29T01:13:42.901Z",
            "message": {
                "id": "msg_123",
                "model": "<synthetic>",
                "usage": {
                    "input_tokens": 1,
                    "output_tokens": 1
                }
            }
        });
        let user = json!({ "type": "user" });

        assert!(claude_usage_row(&synthetic).is_none());
        assert!(claude_usage_row(&user).is_none());
    }

    #[test]
    fn normalizes_claude_model_names() {
        assert_eq!(
            normalize_claude_model("anthropic.claude-sonnet-4-5-20251101"),
            "claude-sonnet-4-5"
        );
        assert_eq!(
            normalize_claude_model("claude-opus-4-6@20251101"),
            "claude-opus-4-6"
        );
        assert_eq!(
            normalize_claude_model("claude-haiku-4-5-v2:3"),
            "claude-haiku-4-5"
        );
    }

    #[test]
    fn applies_sonnet_4_5_long_context_rates() {
        let cost = claude_cost_usd(
            "claude-sonnet-4-5",
            ClaudeTokenTotals {
                input: 200_001,
                cache_creation: 0,
                cache_creation_1h: 0,
                cache_read: 0,
                output: 1000,
            },
        )
        .expect("priced long context");

        assert!((cost - 1.222506).abs() < 0.0000001);
    }

    #[test]
    fn caches_unchanged_project_scans() {
        let root = TestDir::new();
        let cache = Arc::new(Mutex::new(None));

        let first =
            scan_claude_local_costs_cached(cache.clone(), vec![root.path().to_path_buf()]).unwrap();
        let second =
            scan_claude_local_costs_cached(cache.clone(), vec![root.path().to_path_buf()]).unwrap();
        expire_cache(&cache);
        let third = scan_claude_local_costs_cached(cache, vec![root.path().to_path_buf()]).unwrap();

        assert_eq!(first.cache_status, CacheStatus::Refreshed);
        assert_eq!(second.cache_status, CacheStatus::Throttled);
        assert_eq!(third.cache_status, CacheStatus::Hit);
    }

    #[test]
    fn reparses_only_changed_files_and_drops_deleted_files() {
        let root = TestDir::new();
        let today = NaiveDate::from_ymd_opt(2026, 5, 15).unwrap();
        let changed_path = root.path().join("changed.jsonl");
        let unchanged_path = root.path().join("unchanged.jsonl");
        write_usage_file(&changed_path, &[usage_event("a1", today, 10, 1)]);
        write_usage_file(&unchanged_path, &[usage_event("b1", today, 10, 1)]);
        let cache = Arc::new(Mutex::new(None));

        let first = scan_claude_local_costs_cached_on(
            cache.clone(),
            vec![root.path().to_path_buf()],
            today,
        )
        .unwrap();
        let (changed_summary, unchanged_summary) = {
            let guard = cache.lock().unwrap();
            let cached = guard.as_ref().unwrap();
            (
                cached.files.get(&changed_path).unwrap().summary.clone(),
                cached.files.get(&unchanged_path).unwrap().summary.clone(),
            )
        };

        append_usage_event(&changed_path, &usage_event("a2", today, 20, 2));
        let throttled = scan_claude_local_costs_cached_on(
            cache.clone(),
            vec![root.path().to_path_buf()],
            today,
        )
        .unwrap();
        expire_cache(&cache);
        let refreshed = scan_claude_local_costs_cached_on(
            cache.clone(),
            vec![root.path().to_path_buf()],
            today,
        )
        .unwrap();

        assert_eq!(first.report.total_tokens, 22);
        assert_eq!(throttled.cache_status, CacheStatus::Throttled);
        assert_eq!(throttled.report.total_tokens, 22);
        assert_eq!(refreshed.cache_status, CacheStatus::Refreshed);
        assert_eq!(refreshed.report.total_tokens, 44);
        assert_eq!(refreshed.report.assistant_messages, 3);
        {
            let guard = cache.lock().unwrap();
            let cached = guard.as_ref().unwrap();
            assert!(!Arc::ptr_eq(
                &changed_summary,
                &cached.files.get(&changed_path).unwrap().summary
            ));
            assert!(Arc::ptr_eq(
                &unchanged_summary,
                &cached.files.get(&unchanged_path).unwrap().summary
            ));
        }

        write_usage_file(&changed_path, &[usage_event("replacement", today, 3, 1)]);
        expire_cache(&cache);
        let after_truncate = scan_claude_local_costs_cached_on(
            cache.clone(),
            vec![root.path().to_path_buf()],
            today,
        )
        .unwrap();
        assert_eq!(after_truncate.cache_status, CacheStatus::Refreshed);
        assert_eq!(after_truncate.report.total_tokens, 15);
        assert_eq!(after_truncate.report.assistant_messages, 2);

        std::fs::remove_file(&changed_path).unwrap();
        expire_cache(&cache);
        let after_delete = scan_claude_local_costs_cached_on(
            cache.clone(),
            vec![root.path().to_path_buf()],
            today,
        )
        .unwrap();

        assert_eq!(after_delete.cache_status, CacheStatus::Refreshed);
        assert_eq!(after_delete.report.files_scanned, 1);
        assert_eq!(after_delete.report.total_tokens, 11);
        assert!(!cache
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .files
            .contains_key(&changed_path));
    }

    #[test]
    fn refolds_cached_days_when_the_local_date_rolls_over() {
        let root = TestDir::new();
        let today = NaiveDate::from_ymd_opt(2026, 5, 15).unwrap();
        let oldest_lookback_day = today.checked_sub_days(Days::new(29)).unwrap();
        let path = root.path().join("session.jsonl");
        write_usage_file(
            &path,
            &[
                usage_event("old", oldest_lookback_day, 5, 2),
                usage_event("today", today, 10, 1),
            ],
        );
        let cache = Arc::new(Mutex::new(None));

        let first = scan_claude_local_costs_cached_on(
            cache.clone(),
            vec![root.path().to_path_buf()],
            today,
        )
        .unwrap();
        let next_day = today.succ_opt().unwrap();
        let rolled =
            scan_claude_local_costs_cached_on(cache, vec![root.path().to_path_buf()], next_day)
                .unwrap();

        assert_eq!(first.report.today_tokens, 11);
        assert_eq!(first.report.lookback_tokens, 18);
        assert_eq!(first.report.by_day.len(), 2);
        assert_eq!(rolled.cache_status, CacheStatus::Throttled);
        assert_eq!(rolled.report.today_tokens, 0);
        assert_eq!(rolled.report.lookback_tokens, 11);
        assert_eq!(rolled.report.by_day.len(), 1);
        assert_eq!(rolled.report.total_tokens, 18);
    }

    #[test]
    fn deduplicates_overlapping_roots() {
        let root = TestDir::new();
        let today = NaiveDate::from_ymd_opt(2026, 5, 15).unwrap();
        let path = root.path().join("session.jsonl");
        write_usage_file(&path, &[usage_event("one", today, 10, 1)]);
        let cache = Arc::new(Mutex::new(None));

        let scan = scan_claude_local_costs_cached_on(
            cache.clone(),
            vec![root.path().to_path_buf(), root.path().to_path_buf()],
            today,
        )
        .unwrap();

        assert_eq!(scan.report.files_scanned, 1);
        assert_eq!(scan.report.total_tokens, 11);
        let guard = cache.lock().unwrap();
        let cached = guard.as_ref().unwrap();
        assert_eq!(cached.file_order, vec![path]);
        assert_eq!(cached.files.len(), 1);
    }
}
