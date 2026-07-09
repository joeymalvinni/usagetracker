use std::{
    collections::{BTreeMap, HashMap},
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use chrono::{DateTime, Days, Local, NaiveDate, Utc};
use serde_json::{json, Value};
use usage_core::{UsageAmount, UsageUnit, UsageWindow, UsageWindowKind};

use crate::providers::ProviderUsage;

const COST_LOOKBACK_DAYS: u64 = 30;

pub(super) fn merge_local_cost_report(usage: &mut ProviderUsage, report: ClaudeCostReport) {
    if report.total_tokens == 0 {
        usage.metadata["claude_cost"] = json!({
            "source": "local_project_logs",
            "estimate": true,
            "project_roots": report.project_roots,
            "files_scanned": report.files_scanned,
            "assistant_messages": report.assistant_messages,
            "unpriced_tokens": report.unpriced_tokens,
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
        "by_day": daily_cost_rows(&report.by_day),
        "by_model": report.by_model,
    });
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

fn token_window(window_id: &str, label: &str, tokens: u64, kind: UsageWindowKind) -> UsageWindow {
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

#[derive(Debug, Default)]
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

#[derive(Debug, Default)]
struct DailyCostSummary {
    cost_usd: f64,
    tokens: u64,
}

#[derive(Debug, serde::Serialize)]
struct DailyCostRow {
    date: String,
    cost_usd: f64,
    tokens: u64,
}

#[derive(Debug, Default, serde::Serialize)]
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

#[derive(Clone, Copy, Debug, Default)]
struct ClaudeTokenTotals {
    input: u64,
    cache_creation: u64,
    cache_creation_1h: u64,
    cache_read: u64,
    output: u64,
}

impl ClaudeTokenTotals {
    fn total(self) -> u64 {
        self.input
            .saturating_add(self.cache_creation)
            .saturating_add(self.cache_read)
            .saturating_add(self.output)
    }
}

pub(super) fn scan_claude_local_costs_from_roots(
    configured_roots: Vec<PathBuf>,
) -> anyhow::Result<ClaudeCostReport> {
    let roots = if configured_roots.is_empty() {
        claude_project_roots()?
    } else {
        configured_roots
    };
    let today = Local::now().date_naive();
    let lookback_start = today
        .checked_sub_days(Days::new(COST_LOOKBACK_DAYS.saturating_sub(1)))
        .unwrap_or(today);
    let mut report = ClaudeCostReport {
        project_roots: roots
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>(),
        ..Default::default()
    };

    for root in roots {
        collect_claude_project_files(&root, &mut |path| {
            report.files_scanned += 1;
            scan_claude_project_file(path, today, lookback_start, &mut report)
        })?;
    }

    Ok(report)
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

fn collect_claude_project_files(
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
            collect_claude_project_files(&path, visit)?;
        } else if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
            visit(&path)?;
        }
    }
    Ok(())
}

fn scan_claude_project_file(
    path: &Path,
    today: NaiveDate,
    lookback_start: NaiveDate,
    report: &mut ClaudeCostReport,
) -> anyhow::Result<()> {
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

    for row in rows.into_values() {
        add_claude_usage_row(row, today, lookback_start, report);
    }

    Ok(())
}

fn add_claude_usage_row(
    row: ClaudeUsageRow,
    today: NaiveDate,
    lookback_start: NaiveDate,
    report: &mut ClaudeCostReport,
) {
    let tokens = row.tokens.total();
    if tokens == 0 {
        return;
    }

    report.assistant_messages += 1;
    report.total_tokens = report.total_tokens.saturating_add(tokens);
    let cost = claude_cost_usd(&row.model, row.tokens);
    if let Some(cost) = cost {
        report.total_cost_usd += cost;
    } else {
        report.unpriced_tokens = report.unpriced_tokens.saturating_add(tokens);
    }

    let date = row.timestamp.with_timezone(&Local).date_naive();
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
        }
    }

    let summary = report
        .by_model
        .entry(normalize_claude_model(&row.model))
        .or_default();
    summary.input_tokens = summary.input_tokens.saturating_add(row.tokens.input);
    summary.cache_creation_input_tokens = summary
        .cache_creation_input_tokens
        .saturating_add(row.tokens.cache_creation);
    summary.cache_creation_1h_input_tokens = summary
        .cache_creation_1h_input_tokens
        .saturating_add(row.tokens.cache_creation_1h);
    summary.cache_read_input_tokens = summary
        .cache_read_input_tokens
        .saturating_add(row.tokens.cache_read);
    summary.output_tokens = summary.output_tokens.saturating_add(row.tokens.output);
    if let Some(cost) = cost {
        summary.cost_usd += cost;
    }
}

fn daily_cost_rows(by_day: &BTreeMap<NaiveDate, DailyCostSummary>) -> Vec<DailyCostRow> {
    by_day
        .iter()
        .map(|(date, summary)| DailyCostRow {
            date: date.to_string(),
            cost_usd: summary.cost_usd,
            tokens: summary.tokens,
        })
        .collect()
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

fn claude_cost_usd(model: &str, totals: ClaudeTokenTotals) -> Option<f64> {
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
    let p = |input_per_million: f64, output_per_million: f64| ClaudePricing {
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
    let lc = |input_per_million: f64,
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
        "claude-fable-5" => p(10.00, 50.00),
        "claude-haiku-4-5" => p(1.00, 5.00),
        "claude-opus-4-5" | "claude-opus-4-6" | "claude-opus-4-7" | "claude-opus-4-8" => {
            p(5.00, 25.00)
        }
        "claude-sonnet-4-5" => lc(3.00, 15.00, 200_000, 6.00, 22.50),
        "claude-sonnet-4-6" => p(3.00, 15.00),
        "claude-opus-4-1" => p(15.00, 75.00),
        _ => return None,
    })
}

fn normalize_claude_model(model: &str) -> String {
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
                .all(|char| char.is_ascii_digit() || char == ':')
        })
        .map(|(base, _)| base)
        .unwrap_or(model);

    if model.len() > 9 {
        let suffix = &model[model.len() - 8..];
        if model.as_bytes()[model.len() - 9] == b'-'
            && suffix.as_bytes().iter().all(u8::is_ascii_digit)
        {
            return model[..model.len() - 9].to_string();
        }
    }

    model.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
