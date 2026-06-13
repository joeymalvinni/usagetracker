use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use chrono::{DateTime, Days, Local, NaiveDate, TimeDelta, Utc};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};
use usage_core::{ProviderId, UsageAmount, UsageUnit, UsageWindow, UsageWindowKind};

use crate::providers::{
    DiscoveredAccount, ProviderCollectionResult, ProviderCollector, ProviderError,
    ProviderErrorKind, ProviderUsage, HTTP_CONNECT_TIMEOUT, HTTP_REQUEST_TIMEOUT,
};

pub const PROVIDER_ID: &str = "codex";
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const MAX_PERCENT: f64 = 100.0;
const COST_LOOKBACK_DAYS: u64 = 30;

#[derive(Clone)]
pub struct CodexCollector {
    auth_path: PathBuf,
    client: reqwest::Client,
    capture_raw_payloads: bool,
}

impl CodexCollector {
    pub fn new(capture_raw_payloads: bool) -> anyhow::Result<Self> {
        let home = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("failed to resolve home directory for Codex auth"))?;
        let client = reqwest::Client::builder()
            .connect_timeout(HTTP_CONNECT_TIMEOUT)
            .timeout(HTTP_REQUEST_TIMEOUT)
            .user_agent("codex-cli")
            .build()?;
        Ok(Self {
            auth_path: home.join(".codex/auth.json"),
            client,
            capture_raw_payloads,
        })
    }

    async fn load_credentials(&self) -> Result<CodexCredentials, ProviderError> {
        let contents = tokio::fs::read_to_string(&self.auth_path)
            .await
            .map_err(|err| {
                if err.kind() == std::io::ErrorKind::NotFound {
                    ProviderError::new(
                        ProviderErrorKind::CredentialsMissing,
                        "~/.codex/auth.json is missing",
                    )
                } else {
                    ProviderError::new(
                        ProviderErrorKind::CredentialsInvalid,
                        "failed to read Codex auth file",
                    )
                }
            })?;

        let auth: CodexAuth = serde_json::from_str(&contents).map_err(|err| {
            ProviderError::new(
                ProviderErrorKind::CredentialsInvalid,
                format!("Codex auth file is not valid JSON: {err}"),
            )
        })?;

        let tokens = auth.tokens.ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::CredentialsInvalid,
                "Codex auth file is missing tokens",
            )
        })?;

        if tokens.access_token.trim().is_empty() || tokens.account_id.trim().is_empty() {
            return Err(ProviderError::new(
                ProviderErrorKind::CredentialsInvalid,
                "Codex auth file is missing token fields",
            ));
        }

        Ok(CodexCredentials {
            access_token: tokens.access_token,
            account_id: tokens.account_id,
        })
    }
}

#[async_trait]
impl ProviderCollector for CodexCollector {
    fn provider_id(&self) -> ProviderId {
        ProviderId::new(PROVIDER_ID)
    }

    async fn discover_accounts(&self) -> Result<Vec<DiscoveredAccount>, ProviderError> {
        let credentials = self.load_credentials().await?;
        Ok(vec![DiscoveredAccount {
            external_account_id: credentials.account_id,
            display_name: Some("Codex".to_string()),
        }])
    }

    async fn collect_usage(
        &self,
        account: &DiscoveredAccount,
    ) -> Result<ProviderCollectionResult, ProviderError> {
        let credentials = self.load_credentials().await?;
        if credentials.account_id != account.external_account_id {
            return Err(ProviderError::new(
                ProviderErrorKind::CredentialsInvalid,
                "Codex account changed since discovery",
            ));
        }

        let response = self
            .client
            .get(CODEX_USAGE_URL)
            .bearer_auth(&credentials.access_token)
            .header("ChatGPT-Account-Id", &credentials.account_id)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|err| {
                ProviderError::new(
                    ProviderErrorKind::Network,
                    format!("Codex usage request failed: {err}"),
                )
            })?;

        let status = response.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(ProviderError::new(
                ProviderErrorKind::Unauthorized,
                "Codex credentials were rejected",
            ));
        }
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(ProviderError::new(
                ProviderErrorKind::RateLimited,
                "Codex usage endpoint is rate limited",
            ));
        }
        if !status.is_success() {
            return Err(ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                format!("Codex usage endpoint returned HTTP {}", status.as_u16()),
            ));
        }

        let payload: Value = response.json().await.map_err(|err| {
            ProviderError::new(
                ProviderErrorKind::Parse,
                format!("Codex usage JSON was invalid: {err}"),
            )
        })?;
        let mut usage = normalize_usage(&payload, account.display_name.as_deref())?;
        let mut warnings = Vec::new();
        match tokio::task::spawn_blocking(scan_codex_local_costs).await {
            Ok(Ok(report)) => usage.merge_cost_report(report),
            Ok(Err(err)) => warnings.push(format!("Codex local cost scan failed: {err}")),
            Err(err) => warnings.push(format!("Codex local cost scan task failed: {err}")),
        }

        Ok(ProviderCollectionResult {
            usage,
            collection_mode: "wham_usage_api".to_string(),
            raw_payload: self.capture_raw_payloads.then_some(payload),
            warnings,
        })
    }
}

#[derive(Debug, Deserialize)]
struct CodexAuth {
    tokens: Option<CodexTokens>,
}

#[derive(Debug, Deserialize)]
struct CodexTokens {
    access_token: String,
    account_id: String,
}

#[derive(Debug)]
struct CodexCredentials {
    access_token: String,
    account_id: String,
}

fn normalize_usage(
    payload: &Value,
    display_name: Option<&str>,
) -> Result<ProviderUsage, ProviderError> {
    let object = payload.as_object().ok_or_else(|| {
        ProviderError::new(
            ProviderErrorKind::Parse,
            "Codex usage response was not a JSON object",
        )
    })?;

    let mut windows = collect_codex_rate_limit_windows(RateLimitGroupSpec {
        id_prefix: "codex",
        label_prefix: "Codex",
        rate_limit: object.get("rate_limit"),
    });
    windows.extend(collect_codex_rate_limit_windows(RateLimitGroupSpec {
        id_prefix: "codex_code_review",
        label_prefix: "Codex code review",
        rate_limit: object.get("code_review_rate_limit"),
    }));
    windows.extend(collect_additional_rate_limit_windows(
        object.get("additional_rate_limits"),
    ));
    windows.extend(collect_codex_credits_window(object.get("credits")));

    let top_level_keys = object.keys().cloned().collect::<Vec<_>>();
    Ok(ProviderUsage {
        provider_id: ProviderId::new(PROVIDER_ID),
        collected_at: Utc::now(),
        windows,
        metadata: json!({
            "account_display_name": display_name,
            "collection_mode": "wham_usage_api",
            "credits_has_credits": object.get("credits").and_then(|value| value.get("has_credits")).and_then(Value::as_bool),
            "credits_overage_limit_reached": object.get("credits").and_then(|value| value.get("overage_limit_reached")).and_then(Value::as_bool),
            "credits_unlimited": object.get("credits").and_then(|value| value.get("unlimited")).and_then(Value::as_bool),
            "plan_type": object.get("plan_type").and_then(Value::as_str),
            "rate_limit_reached_type": object.get("rate_limit_reached_type").and_then(Value::as_str),
            "rate_limit_reset_credits_available_count": object
                .get("rate_limit_reset_credits")
                .and_then(|value| value.get("available_count"))
                .and_then(number_from_json_value),
            "spend_control_reached": object.get("spend_control").and_then(|value| value.get("reached")).and_then(Value::as_bool),
            "top_level_keys": top_level_keys,
        }),
    })
}

trait CodexUsageCostExt {
    fn merge_cost_report(&mut self, report: CodexCostReport);
}

impl CodexUsageCostExt for ProviderUsage {
    fn merge_cost_report(&mut self, report: CodexCostReport) {
        if report.total_tokens == 0 {
            self.metadata["codex_cost"] = json!({
                "source": "local_session_logs",
                "estimate": true,
                "session_roots": report.session_roots,
                "files_scanned": report.files_scanned,
                "token_count_events": report.token_count_events,
                "unpriced_tokens": report.unpriced_tokens,
            });
            return;
        }

        if report.today_tokens > 0 {
            self.windows.push(cost_window(
                "codex_estimated_spend_today",
                "Codex spend today",
                report.today_cost_usd,
            ));
            self.windows.push(token_window(
                "codex_tokens_today",
                "Codex tokens today",
                report.today_tokens,
                UsageWindowKind::Daily,
            ));
        }

        if report.lookback_tokens > 0 {
            self.windows.push(cost_window(
                "codex_estimated_spend_30d",
                "Codex spend 30 days",
                report.lookback_cost_usd,
            ));
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
            "hint": "Estimated from local Codex logs for the selected account.",
            "session_roots": report.session_roots,
            "files_scanned": report.files_scanned,
            "token_count_events": report.token_count_events,
            "today_cost_usd": report.today_cost_usd,
            "today_tokens": report.today_tokens,
            "lookback_days": COST_LOOKBACK_DAYS,
            "lookback_cost_usd": report.lookback_cost_usd,
            "lookback_tokens": report.lookback_tokens,
            "total_cost_usd": report.total_cost_usd,
            "total_tokens": report.total_tokens,
            "unpriced_tokens": report.unpriced_tokens,
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
struct CodexCostReport {
    session_roots: Vec<String>,
    files_scanned: usize,
    token_count_events: usize,
    today_cost_usd: f64,
    today_tokens: u64,
    lookback_cost_usd: f64,
    lookback_tokens: u64,
    total_cost_usd: f64,
    total_tokens: u64,
    unpriced_tokens: u64,
    by_model: BTreeMap<String, CodexModelCostSummary>,
}

#[derive(Debug, Default, serde::Serialize)]
struct CodexModelCostSummary {
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    cost_usd: f64,
}

#[derive(Clone, Copy, Debug, Default)]
struct CodexTokenTotals {
    input: u64,
    cached: u64,
    output: u64,
}

impl CodexTokenTotals {
    fn total(self) -> u64 {
        self.input
            .saturating_add(self.cached)
            .saturating_add(self.output)
    }

    fn saturating_delta(self, previous: Self) -> Self {
        Self {
            input: self.input.saturating_sub(previous.input),
            cached: self.cached.saturating_sub(previous.cached),
            output: self.output.saturating_sub(previous.output),
        }
    }
}

fn scan_codex_local_costs() -> anyhow::Result<CodexCostReport> {
    let roots = codex_session_roots()?;
    let today = Local::now().date_naive();
    let lookback_start = today
        .checked_sub_days(Days::new(COST_LOOKBACK_DAYS.saturating_sub(1)))
        .unwrap_or(today);
    let mut report = CodexCostReport {
        session_roots: roots
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>(),
        ..Default::default()
    };

    for root in roots {
        collect_codex_session_files(&root, &mut |path| {
            report.files_scanned += 1;
            scan_codex_session_file(path, today, lookback_start, &mut report)
        })?;
    }

    Ok(report)
}

fn codex_session_roots() -> anyhow::Result<Vec<PathBuf>> {
    let codex_home = match std::env::var("CODEX_HOME") {
        Ok(value) if !value.trim().is_empty() => PathBuf::from(value),
        _ => dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("failed to resolve home directory for Codex logs"))?
            .join(".codex"),
    };
    Ok(vec![codex_home.join("sessions")])
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

fn scan_codex_session_file(
    path: &Path,
    today: NaiveDate,
    lookback_start: NaiveDate,
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
            current_model = Some(model.to_string());
        }

        let Some(info) = codex_token_count_info(&event) else {
            continue;
        };

        report.token_count_events += 1;
        let delta = info
            .get("last_token_usage")
            .and_then(codex_totals_from_value)
            .or_else(|| {
                let current = info
                    .get("total_token_usage")
                    .and_then(codex_totals_from_value)?;
                let previous = previous_totals.unwrap_or_default();
                Some(current.saturating_delta(previous))
            })
            .unwrap_or_default();

        if let Some(total) = info
            .get("total_token_usage")
            .and_then(codex_totals_from_value)
        {
            previous_totals = Some(total);
        } else {
            previous_totals = Some(previous_totals.unwrap_or_default().add(delta));
        }

        if delta.total() == 0 {
            continue;
        }

        let model = current_model.as_deref().unwrap_or("unknown");
        let cost = codex_cost_usd(model, delta);
        let tokens = delta.total();
        let date = codex_event_timestamp(&event)
            .map(|timestamp| timestamp.with_timezone(&Local).date_naive())
            .unwrap_or(today);

        report.total_tokens = report.total_tokens.saturating_add(tokens);
        if let Some(cost) = cost {
            report.total_cost_usd += cost;
        } else {
            report.unpriced_tokens = report.unpriced_tokens.saturating_add(tokens);
        }

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
        }

        let summary = report
            .by_model
            .entry(normalize_codex_model(model))
            .or_default();
        summary.input_tokens = summary.input_tokens.saturating_add(delta.input);
        summary.cached_input_tokens = summary.cached_input_tokens.saturating_add(delta.cached);
        summary.output_tokens = summary.output_tokens.saturating_add(delta.output);
        if let Some(cost) = cost {
            summary.cost_usd += cost;
        }
    }

    Ok(())
}

trait CodexTotalsAdd {
    fn add(self, delta: CodexTokenTotals) -> Self;
}

impl CodexTotalsAdd for CodexTokenTotals {
    fn add(self, delta: CodexTokenTotals) -> Self {
        Self {
            input: self.input.saturating_add(delta.input),
            cached: self.cached.saturating_add(delta.cached),
            output: self.output.saturating_add(delta.output),
        }
    }
}

fn codex_token_count_info(event: &Value) -> Option<&Value> {
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

fn codex_turn_context_model(event: &Value) -> Option<&str> {
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

fn codex_event_timestamp(event: &Value) -> Option<DateTime<Utc>> {
    let timestamp = event.get("timestamp").and_then(Value::as_str)?;
    DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

fn codex_totals_from_value(value: &Value) -> Option<CodexTokenTotals> {
    Some(CodexTokenTotals {
        input: u64_from_json_value(value.get("input_tokens")?)?,
        cached: value
            .get("cached_input_tokens")
            .and_then(u64_from_json_value)
            .unwrap_or(0),
        output: u64_from_json_value(value.get("output_tokens")?)?,
    })
}

fn u64_from_json_value(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64(),
        Value::String(value) => value.parse().ok(),
        _ => None,
    }
}

#[derive(Clone, Copy)]
struct CodexPricing {
    input: f64,
    cached_input: Option<f64>,
    output: f64,
    long_context_threshold: Option<u64>,
    long_context_multiplier: f64,
}

fn codex_cost_usd(model: &str, totals: CodexTokenTotals) -> Option<f64> {
    let pricing = codex_pricing(model)?;
    let cached = totals.cached.min(totals.input);
    let non_cached = totals.input.saturating_sub(cached);
    let multiplier = pricing
        .long_context_threshold
        .filter(|threshold| totals.input > *threshold)
        .map(|_| pricing.long_context_multiplier)
        .unwrap_or(1.0);
    let cached_rate = pricing.cached_input.unwrap_or(pricing.input);

    Some(
        non_cached as f64 * pricing.input * multiplier
            + cached as f64 * cached_rate * multiplier
            + totals.output as f64 * pricing.output * multiplier,
    )
}

fn codex_pricing(model: &str) -> Option<CodexPricing> {
    let model = normalize_codex_model(model);
    let p = |input_per_million: f64, output_per_million: f64, cache_per_million: Option<f64>| {
        CodexPricing {
            input: input_per_million / 1_000_000.0,
            cached_input: cache_per_million.map(|value| value / 1_000_000.0),
            output: output_per_million / 1_000_000.0,
            long_context_threshold: None,
            long_context_multiplier: 1.0,
        }
    };
    let lc =
        |input_per_million: f64, output_per_million: f64, cache_per_million: f64| CodexPricing {
            input: input_per_million / 1_000_000.0,
            cached_input: Some(cache_per_million / 1_000_000.0),
            output: output_per_million / 1_000_000.0,
            long_context_threshold: Some(272_000),
            long_context_multiplier: 2.0,
        };

    Some(match model.as_str() {
        "gpt-5" | "gpt-5-codex" | "gpt-5.1" | "gpt-5.1-codex" | "gpt-5.1-codex-max" => {
            p(1.25, 10.00, Some(0.125))
        }
        "gpt-5-mini" => p(0.25, 2.00, Some(0.025)),
        "gpt-5-nano" => p(0.05, 0.40, Some(0.005)),
        "gpt-5-pro" => p(15.00, 120.00, None),
        "gpt-5.2" | "gpt-5.2-codex" | "gpt-5.3-codex" => p(1.75, 14.00, Some(0.175)),
        "gpt-5.2-pro" => p(21.00, 168.00, None),
        "gpt-5.3-codex-spark" => p(0.0, 0.0, Some(0.0)),
        "gpt-5.4" => lc(2.50, 15.00, 0.25),
        "gpt-5.4-mini" => p(0.75, 4.50, Some(0.075)),
        "gpt-5.4-nano" => p(0.20, 1.25, Some(0.02)),
        "gpt-5.4-pro" | "gpt-5.5-pro" => p(30.00, 180.00, None),
        "gpt-5.5" => lc(5.00, 30.00, 0.50),
        _ => return None,
    })
}

fn normalize_codex_model(model: &str) -> String {
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

struct RateLimitGroupSpec<'a> {
    id_prefix: &'a str,
    label_prefix: &'a str,
    rate_limit: Option<&'a Value>,
}

struct RateLimitWindowSpec<'a> {
    window_id: String,
    label: String,
    kind: UsageWindowKind,
    value: Option<&'a Value>,
}

fn collect_codex_rate_limit_windows(spec: RateLimitGroupSpec<'_>) -> Vec<UsageWindow> {
    let Some(rate_limit) = spec.rate_limit.and_then(Value::as_object) else {
        return Vec::new();
    };

    [
        rate_limit_window(RateLimitWindowSpec {
            window_id: format!("{}_session", spec.id_prefix),
            label: format!("{} session", spec.label_prefix),
            kind: UsageWindowKind::Session,
            value: rate_limit.get("primary_window"),
        }),
        rate_limit_window(RateLimitWindowSpec {
            window_id: format!("{}_weekly", spec.id_prefix),
            label: format!("{} weekly", spec.label_prefix),
            kind: UsageWindowKind::Weekly,
            value: rate_limit.get("secondary_window"),
        }),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn collect_additional_rate_limit_windows(value: Option<&Value>) -> Vec<UsageWindow> {
    let Some(rate_limits) = value.and_then(Value::as_array) else {
        return Vec::new();
    };

    rate_limits
        .iter()
        .enumerate()
        .filter_map(|(index, rate_limit)| {
            let rate_limit = rate_limit.as_object()?;
            let label = rate_limit
                .get("limit_name")
                .and_then(Value::as_str)
                .or_else(|| rate_limit.get("metered_feature").and_then(Value::as_str))
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| format!("Codex additional limit {}", index + 1));
            let id_prefix = format!("codex_additional_{index}");
            Some(collect_codex_rate_limit_windows(RateLimitGroupSpec {
                id_prefix: &id_prefix,
                label_prefix: &label,
                rate_limit: rate_limit.get("rate_limit"),
            }))
        })
        .flatten()
        .collect()
}

fn collect_codex_credits_window(credits: Option<&Value>) -> Option<UsageWindow> {
    let credits = credits.and_then(Value::as_object)?;

    let balance = credits.get("balance").and_then(number_from_json_value);
    let unlimited = credits
        .get("unlimited")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if balance.is_none() && !unlimited {
        return None;
    }

    let remaining = if unlimited { None } else { balance };
    let mut metadata_label = "Codex credits".to_string();
    if unlimited {
        metadata_label.push_str(" (unlimited)");
    }

    Some(UsageWindow {
        window_id: "codex_credits".to_string(),
        label: metadata_label,
        kind: UsageWindowKind::Credits,
        used: None,
        limit: None,
        remaining: remaining.map(|value| UsageAmount {
            value,
            unit: UsageUnit::Credits,
        }),
        percent_used: None,
        percent_remaining: None,
        reset_at: None,
    })
}

fn rate_limit_window(spec: RateLimitWindowSpec<'_>) -> Option<UsageWindow> {
    let object = spec.value?.as_object()?;
    let percent_used = object
        .get("used_percent")
        .and_then(number_from_json_value)
        .map(|value| value.clamp(0.0, MAX_PERCENT));
    let percent_remaining = percent_used.map(|value| MAX_PERCENT - value);
    let reset_at = object
        .get("reset_at")
        .and_then(unix_timestamp_from_json_value)
        .or_else(|| {
            object
                .get("reset_after_seconds")
                .and_then(number_from_json_value)
                .and_then(|seconds| TimeDelta::try_seconds(seconds.round() as i64))
                .map(|duration| Utc::now() + duration)
        });

    if percent_used.is_none() && reset_at.is_none() {
        return None;
    }

    Some(UsageWindow {
        window_id: spec.window_id,
        label: spec.label,
        kind: spec.kind,
        used: None,
        limit: None,
        remaining: None,
        percent_used,
        percent_remaining,
        reset_at,
    })
}

fn number_from_json_value(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(value) => value.parse().ok(),
        _ => None,
    }
}

fn unix_timestamp_from_json_value(value: &Value) -> Option<DateTime<Utc>> {
    let seconds = number_from_json_value(value)?.round() as i64;
    DateTime::from_timestamp(seconds, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use usage_core::AccountId;

    #[test]
    fn normalizes_codex_rate_limits() {
        let payload = json!({
            "account_id": "external-account",
            "email": "user@example.com",
            "plan_type": "prolite",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {
                    "limit_window_seconds": 18000,
                    "reset_after_seconds": 1486,
                    "reset_at": 1781233774,
                    "used_percent": 23
                },
                "secondary_window": {
                    "limit_window_seconds": 604800,
                    "reset_after_seconds": 588286,
                    "reset_at": 1781820574,
                    "used_percent": 4
                }
            },
            "additional_rate_limits": [
                {
                    "limit_name": "GPT-5.3-Codex-Spark",
                    "metered_feature": "codex_bengalfox",
                    "rate_limit": {
                        "allowed": true,
                        "limit_reached": false,
                        "primary_window": {
                            "limit_window_seconds": 18000,
                            "reset_after_seconds": 18000,
                            "reset_at": 1781250288,
                            "used_percent": 0
                        },
                        "secondary_window": {
                            "limit_window_seconds": 604800,
                            "reset_after_seconds": 398008,
                            "reset_at": 1781630296,
                            "used_percent": 0
                        }
                    }
                }
            ],
            "credits": {
                "balance": "0",
                "has_credits": false,
                "unlimited": false
            },
            "rate_limit_reset_credits": {
                "available_count": 1
            }
        });

        let snapshot = normalize_usage(&payload, Some("Codex"))
            .unwrap()
            .into_snapshot(AccountId::new("acct"));
        assert_eq!(snapshot.windows.len(), 5);

        let session = find_window(&snapshot.windows, "codex_session");
        assert_eq!(session.label, "Codex session");
        assert!(matches!(session.kind, UsageWindowKind::Session));
        assert_eq!(session.percent_used, Some(23.0));
        assert_eq!(session.percent_remaining, Some(77.0));
        assert_eq!(session.reset_at.unwrap().timestamp(), 1781233774);

        let weekly = find_window(&snapshot.windows, "codex_weekly");
        assert_eq!(weekly.label, "Codex weekly");
        assert!(matches!(weekly.kind, UsageWindowKind::Weekly));
        assert_eq!(weekly.percent_used, Some(4.0));
        assert_eq!(weekly.percent_remaining, Some(96.0));
        assert_eq!(weekly.reset_at.unwrap().timestamp(), 1781820574);

        let additional_session = find_window(&snapshot.windows, "codex_additional_0_session");
        assert_eq!(additional_session.label, "GPT-5.3-Codex-Spark session");
        assert_eq!(additional_session.percent_used, Some(0.0));

        let credits = find_window(&snapshot.windows, "codex_credits");
        assert_eq!(credits.label, "Codex credits");
        assert!(matches!(credits.kind, UsageWindowKind::Credits));
        assert_eq!(credits.remaining.as_ref().unwrap().value, 0.0);
        assert!(credits.limit.is_none());

        assert_eq!(snapshot.metadata["plan_type"], "prolite");
        assert_eq!(snapshot.metadata["credits_has_credits"], false);
        assert_eq!(
            snapshot.metadata["rate_limit_reset_credits_available_count"],
            1.0
        );
    }

    #[test]
    fn rejects_non_object_payloads() {
        let err = normalize_usage(&json!([1, 2, 3]), None).unwrap_err();
        assert_eq!(err.kind(), ProviderErrorKind::Parse);
    }

    #[test]
    fn reads_current_token_count_event_shape() {
        let event = json!({
            "timestamp": "2026-06-12T19:11:08.807Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "last_token_usage": {
                        "input_tokens": 1000,
                        "cached_input_tokens": 100,
                        "output_tokens": 50
                    }
                }
            }
        });

        let info = codex_token_count_info(&event).expect("token_count info");
        let totals = codex_totals_from_value(&info["last_token_usage"]).expect("token totals");
        assert_eq!(totals.input, 1000);
        assert_eq!(totals.cached, 100);
        assert_eq!(totals.output, 50);
        assert_eq!(
            codex_event_timestamp(&event).unwrap().timestamp(),
            1_781_291_468
        );
    }

    #[test]
    fn reads_turn_context_model_shapes() {
        let current = json!({
            "type": "turn_context",
            "payload": { "model": "gpt-5.5" }
        });
        let nested = json!({
            "type": "event_msg",
            "payload": {
                "type": "turn_context",
                "payload": { "model": "gpt-5.4-mini" }
            }
        });

        assert_eq!(codex_turn_context_model(&current), Some("gpt-5.5"));
        assert_eq!(codex_turn_context_model(&nested), Some("gpt-5.4-mini"));
    }

    #[test]
    fn prices_codex_tokens_with_cache_and_model_normalization() {
        let cost = codex_cost_usd(
            "openai/gpt-5.5-2026-06-01",
            CodexTokenTotals {
                input: 1000,
                cached: 400,
                output: 100,
            },
        )
        .unwrap();

        assert_eq!(
            normalize_codex_model("openai/gpt-5.5-2026-06-01"),
            "gpt-5.5"
        );
        assert!((cost - 0.0062).abs() < f64::EPSILON);
    }

    fn find_window<'a>(windows: &'a [UsageWindow], window_id: &str) -> &'a UsageWindow {
        windows
            .iter()
            .find(|window| window.window_id == window_id)
            .unwrap_or_else(|| panic!("missing window {window_id}"))
    }
}
