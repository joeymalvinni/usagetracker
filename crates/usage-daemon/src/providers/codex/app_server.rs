//! Codex app-server JSON-RPC collection and account activity normalization.

use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use chrono::{Days, NaiveDate, Utc};
use serde_json::{json, Value};
use tracing::{debug, warn};
use usage_core::UsageWindowKind;
use wait_timeout::ChildExt;

use crate::providers::{DailyUsageBucket, ProviderError, ProviderErrorKind, ProviderUsage};

use super::{
    cost::{token_window, u64_from_json_value},
    rate_limits::normalize_app_server_usage,
    CodexCollectedUsage, CodexProfile, CODEX_ACCOUNT_USAGE_GRACE_TIMEOUT, CODEX_APP_SERVER_TIMEOUT,
    COST_LOOKBACK_DAYS,
};

const MAX_APP_SERVER_STDOUT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_APP_SERVER_STDERR_BYTES: u64 = 64 * 1024;

/// Ensures the codex app-server child is always killed and reaped, even when an
/// early `?` return (e.g. a broken-pipe write or a JSON parse error) skips the
/// explicit cleanup below. `std::process::Child` does not reap on drop, so
/// without this a failed collection would leak a zombie process.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub(super) fn collect_usage_from_app_server(
    profile: &CodexProfile,
) -> Result<CodexCollectedUsage, ProviderError> {
    let payload = run_codex_app_server_rate_limits(&profile.codex_home).map_err(|err| {
        ProviderError::new(
            ProviderErrorKind::ProviderUnavailable,
            format!("Codex app-server rate limit request failed: {err}"),
        )
    })?;
    let account_display_name = payload
        .get("account_read")
        .and_then(|value| value.get("account"))
        .and_then(|value| value.get("email"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let mut usage = normalize_app_server_usage(&payload, account_display_name.as_deref())?;
    let mut warnings = Vec::new();
    let (daily_usage, account_activity_available) = match payload
        .get("account_usage_read")
        .filter(|value| !value.is_null())
    {
        Some(value) => match normalize_account_token_usage(value) {
            Ok(activity) => {
                let daily_usage = activity.daily_usage.clone();
                usage.merge_account_activity(activity);
                (daily_usage, true)
            }
            Err(err) => {
                warnings.push(format!(
                    "Codex account activity could not be parsed; using local activity fallback: {}",
                    err.short_message()
                ));
                (Vec::new(), false)
            }
        },
        None => {
            let detail = payload
                .get("account_usage_error")
                .and_then(Value::as_str)
                .unwrap_or("account/usage/read returned no result");
            warnings.push(format!(
                "Codex account activity was unavailable; using local activity fallback: {detail}"
            ));
            (Vec::new(), false)
        }
    };

    Ok(CodexCollectedUsage {
        usage,
        daily_usage,
        account_activity_available,
        collection_mode: "codex_app_server_rate_limits".to_string(),
        account_display_name,
        raw_payload: payload,
        warnings,
    })
}

fn run_codex_app_server_rate_limits(codex_home: &Path) -> anyhow::Result<Value> {
    let started = Instant::now();
    debug!("codex app-server process starting");
    let mut child = ChildGuard(
        Command::new("codex")
            .arg("app-server")
            .env("CODEX_HOME", codex_home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?,
    );

    let mut stdin = child
        .0
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to open codex app-server stdin"))?;
    let stdout = child
        .0
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to open codex app-server stdout"))?;
    let stderr = child
        .0
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to open codex app-server stderr"))?;

    let (line_tx, line_rx) = mpsc::channel::<std::io::Result<String>>();
    let _stdout_thread = thread::spawn(move || {
        for line in BufReader::new(stdout.take(MAX_APP_SERVER_STDOUT_BYTES)).lines() {
            let stop = line.is_err();
            if line_tx.send(line).is_err() || stop {
                break;
            }
        }
    });

    let (stderr_tx, stderr_rx) = mpsc::channel::<String>();
    let _stderr_thread = thread::spawn(move || {
        let mut contents = String::new();
        let _ = stderr
            .take(MAX_APP_SERVER_STDERR_BYTES)
            .read_to_string(&mut contents);
        let _ = stderr_tx.send(contents);
    });

    write_json_rpc(
        &mut stdin,
        &json!({
            "method": "initialize",
            "id": 1,
            "params": {
                "clientInfo": {
                    "name": "usagetracker",
                    "title": "usagetracker",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }
        }),
    )?;
    write_json_rpc(
        &mut stdin,
        &json!({ "method": "initialized", "params": {} }),
    )?;
    write_json_rpc(
        &mut stdin,
        &json!({
            "method": "account/read",
            "id": 2,
            "params": { "refreshToken": false }
        }),
    )?;
    write_json_rpc(
        &mut stdin,
        &json!({ "method": "account/rateLimits/read", "id": 3 }),
    )?;
    write_json_rpc(
        &mut stdin,
        &json!({ "method": "account/usage/read", "id": 4 }),
    )?;

    let deadline = Instant::now() + CODEX_APP_SERVER_TIMEOUT;
    let mut account_read: Option<Value> = None;
    let mut rate_limits_read: Option<Value> = None;
    let mut account_usage_read: Option<Value> = None;
    let mut account_usage_error: Option<String> = None;
    let mut account_usage_complete = false;
    let mut required_completed_at = None;

    while account_read.is_none() || rate_limits_read.is_none() || !account_usage_complete {
        let now = Instant::now();
        if account_read.is_some() && rate_limits_read.is_some() {
            if account_usage_complete {
                break;
            }
            required_completed_at.get_or_insert(now);
        }
        let response_deadline = required_completed_at
            .map(|completed_at| completed_at + CODEX_ACCOUNT_USAGE_GRACE_TIMEOUT)
            .map_or(deadline, |optional_deadline| {
                optional_deadline.min(deadline)
            });
        let Some(remaining) = response_deadline.checked_duration_since(now) else {
            break;
        };
        let line = match line_rx.recv_timeout(remaining) {
            Ok(line) => line?,
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("codex app-server stdout closed before expected responses");
            }
        };

        let message: Value = serde_json::from_str(&line)?;
        debug!(
            id = message.get("id").and_then(|value| value.as_i64()),
            has_error = message.get("error").is_some(),
            elapsed_ms = started.elapsed().as_millis(),
            "codex app-server message received"
        );
        match message.get("id").and_then(Value::as_i64) {
            Some(2) => account_read = Some(json_rpc_result(message, "account/read")?),
            Some(3) => {
                rate_limits_read = Some(json_rpc_result(message, "account/rateLimits/read")?)
            }
            Some(4) => {
                account_usage_complete = true;
                if let Some(error) = message.get("error") {
                    account_usage_error = Some(error.to_string());
                } else {
                    account_usage_read = message.get("result").cloned();
                    if account_usage_read.is_none() {
                        account_usage_error =
                            Some("account/usage/read response was missing result".to_string());
                    }
                }
            }
            _ => {}
        }
    }

    drop(stdin);
    let _ = child.0.kill();
    match child.0.wait_timeout(Duration::from_secs(2)) {
        Ok(Some(status)) => {
            debug!(
                status = %status,
                elapsed_ms = started.elapsed().as_millis(),
                "codex app-server process exited"
            );
        }
        Ok(None) => {
            warn!(
                elapsed_ms = started.elapsed().as_millis(),
                "codex app-server process did not exit after kill timeout"
            );
        }
        Err(err) => {
            warn!(error = %err, "failed to wait for codex app-server process");
        }
    }
    let stderr = stderr_rx
        .recv_timeout(Duration::from_millis(100))
        .unwrap_or_default();

    let account_read = account_read.ok_or_else(|| {
        warn!(
            elapsed_ms = started.elapsed().as_millis(),
            stderr = stderr.trim(),
            "codex app-server account/read timed out"
        );
        anyhow::anyhow!(
            "codex app-server account/read timed out after {:?}; stderr: {}",
            CODEX_APP_SERVER_TIMEOUT,
            stderr.trim()
        )
    })?;
    let rate_limits_read = rate_limits_read.ok_or_else(|| {
        warn!(
            elapsed_ms = started.elapsed().as_millis(),
            stderr = stderr.trim(),
            "codex app-server account/rateLimits/read timed out"
        );
        anyhow::anyhow!(
            "codex app-server account/rateLimits/read timed out after {:?}; stderr: {}",
            CODEX_APP_SERVER_TIMEOUT,
            stderr.trim()
        )
    })?;
    if !account_usage_complete {
        account_usage_error = Some(format!(
            "account/usage/read did not respond within {:?} after rate limits were ready",
            CODEX_ACCOUNT_USAGE_GRACE_TIMEOUT
        ));
    }

    debug!(
        elapsed_ms = started.elapsed().as_millis(),
        "codex app-server process completed"
    );

    Ok(json!({
        "account_read": account_read,
        "rate_limits_read": rate_limits_read,
        "account_usage_read": account_usage_read,
        "account_usage_error": account_usage_error,
    }))
}

fn write_json_rpc(stdin: &mut impl Write, message: &Value) -> anyhow::Result<()> {
    serde_json::to_writer(&mut *stdin, message)?;
    stdin.write_all(b"\n")?;
    stdin.flush()?;
    Ok(())
}

fn json_rpc_result(message: Value, method: &str) -> anyhow::Result<Value> {
    if let Some(error) = message.get("error") {
        anyhow::bail!("{method} returned error: {error}");
    }
    message
        .get("result")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("{method} response missing result"))
}

pub(super) struct CodexAccountActivity {
    pub(super) daily_usage: Vec<DailyUsageBucket>,
    pub(super) lifetime_tokens: Option<u64>,
    pub(super) peak_daily_tokens: Option<u64>,
    pub(super) longest_running_turn_sec: Option<u64>,
    pub(super) current_streak_days: Option<u64>,
    pub(super) longest_streak_days: Option<u64>,
}

pub(super) fn normalize_account_token_usage(
    value: &Value,
) -> Result<CodexAccountActivity, ProviderError> {
    let object = value.as_object().ok_or_else(|| {
        ProviderError::new(
            ProviderErrorKind::Parse,
            "Codex account usage response was not an object",
        )
    })?;
    let summary = object
        .get("summary")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::Parse,
                "Codex account usage response was missing summary",
            )
        })?;

    let mut by_date = BTreeMap::<NaiveDate, u64>::new();
    if let Some(buckets) = object
        .get("dailyUsageBuckets")
        .filter(|value| !value.is_null())
    {
        let buckets = buckets.as_array().ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::Parse,
                "Codex account usage daily buckets were not an array",
            )
        })?;
        for bucket in buckets {
            let bucket = bucket.as_object().ok_or_else(|| {
                ProviderError::new(
                    ProviderErrorKind::Parse,
                    "Codex account usage contained a non-object daily bucket",
                )
            })?;
            let date = bucket
                .get("startDate")
                .and_then(Value::as_str)
                .and_then(|value| NaiveDate::parse_from_str(value, "%Y-%m-%d").ok())
                .ok_or_else(|| {
                    ProviderError::new(
                        ProviderErrorKind::Parse,
                        "Codex account usage daily bucket had an invalid startDate",
                    )
                })?;
            let tokens = bucket
                .get("tokens")
                .and_then(u64_from_json_value)
                .ok_or_else(|| {
                    ProviderError::new(
                        ProviderErrorKind::Parse,
                        "Codex account usage daily bucket had invalid tokens",
                    )
                })?;
            let entry = by_date.entry(date).or_default();
            *entry = entry.saturating_add(tokens);
        }
    }

    Ok(CodexAccountActivity {
        daily_usage: by_date
            .into_iter()
            .map(|(date, tokens)| DailyUsageBucket {
                date,
                tokens,
                cost_usd: None,
                source: "codex_account_usage".to_string(),
            })
            .collect(),
        lifetime_tokens: summary.get("lifetimeTokens").and_then(u64_from_json_value),
        peak_daily_tokens: summary.get("peakDailyTokens").and_then(u64_from_json_value),
        longest_running_turn_sec: summary
            .get("longestRunningTurnSec")
            .and_then(u64_from_json_value),
        current_streak_days: summary
            .get("currentStreakDays")
            .and_then(u64_from_json_value),
        longest_streak_days: summary
            .get("longestStreakDays")
            .and_then(u64_from_json_value),
    })
}

pub(super) trait CodexAccountActivityExt {
    fn merge_account_activity(&mut self, activity: CodexAccountActivity);
}

impl CodexAccountActivityExt for ProviderUsage {
    fn merge_account_activity(&mut self, activity: CodexAccountActivity) {
        let today = Utc::now().date_naive();
        let lookback_start = today
            .checked_sub_days(Days::new(COST_LOOKBACK_DAYS.saturating_sub(1)))
            .unwrap_or(today);
        let today_tokens = activity
            .daily_usage
            .iter()
            .find(|bucket| bucket.date == today)
            .map(|bucket| bucket.tokens)
            .unwrap_or(0);
        let lookback_tokens = activity
            .daily_usage
            .iter()
            .filter(|bucket| bucket.date >= lookback_start && bucket.date <= today)
            .fold(0_u64, |total, bucket| total.saturating_add(bucket.tokens));
        let bucket_sum = activity
            .daily_usage
            .iter()
            .fold(0_u64, |total, bucket| total.saturating_add(bucket.tokens));
        let lifetime_tokens = activity.lifetime_tokens.unwrap_or(bucket_sum);

        if today_tokens > 0 {
            self.windows.push(token_window(
                "codex_tokens_today",
                "Codex tokens today",
                today_tokens,
                UsageWindowKind::Daily,
            ));
        }
        if lookback_tokens > 0 {
            self.windows.push(token_window(
                "codex_tokens_30d",
                "Codex tokens 30 days",
                lookback_tokens,
                UsageWindowKind::Monthly,
            ));
        }
        if lifetime_tokens > 0 {
            self.windows.push(token_window(
                "codex_tokens_lifetime",
                "Codex lifetime tokens",
                lifetime_tokens,
                UsageWindowKind::Other("lifetime".to_string()),
            ));
        }

        let by_day = activity
            .daily_usage
            .iter()
            .map(|bucket| {
                json!({
                    "date": bucket.date.to_string(),
                    "tokens": bucket.tokens,
                })
            })
            .collect::<Vec<_>>();
        self.metadata["codex_activity"] = json!({
            "source": "codex_account_usage",
            "server_authoritative": true,
            "daily_bucket_count": activity.daily_usage.len(),
            "today_tokens": today_tokens,
            "lookback_days": COST_LOOKBACK_DAYS,
            "lookback_tokens": lookback_tokens,
            "lifetime_tokens": lifetime_tokens,
            "peak_daily_tokens": activity.peak_daily_tokens,
            "longest_running_turn_sec": activity.longest_running_turn_sec,
            "current_streak_days": activity.current_streak_days,
            "longest_streak_days": activity.longest_streak_days,
            "by_day": by_day,
        });
    }
}
