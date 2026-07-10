use std::{
    collections::BTreeMap,
    io::Read,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::Duration,
};

use chrono::{DateTime, Datelike, Days, TimeZone, Utc};
use chrono_tz::Tz;
use serde::Deserialize;
use serde_json::json;
use usage_core::{ProviderId, UsageAmount, UsageUnit, UsageWindow, UsageWindowKind};
use wait_timeout::ChildExt;

use crate::providers::{ProviderError, ProviderErrorKind, ProviderUsage};

use super::{CLAUDE_CLI_COLLECTION_MODE, PROVIDER_ID};

const CLAUDE_CLI_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_CLAUDE_CLI_STDOUT_BYTES: u64 = 1024 * 1024;
const MAX_CLAUDE_CLI_STDERR_BYTES: u64 = 64 * 1024;
const MAX_PERCENT: f64 = 100.0;

pub(super) struct ClaudeCliUsage {
    pub usage: ProviderUsage,
    pub raw_output: serde_json::Value,
}

pub(super) fn collect_usage_from_cli(
    config_dir: Option<&Path>,
) -> Result<ClaudeCliUsage, ProviderError> {
    let raw_output = run_claude_usage_cli(config_dir).map_err(|err| {
        ProviderError::new(
            ProviderErrorKind::ProviderUnavailable,
            format!("Claude CLI usage fallback failed: {err}"),
        )
    })?;
    let response: ClaudePrintResponse = serde_json::from_str(&raw_output).map_err(|err| {
        ProviderError::new(
            ProviderErrorKind::Parse,
            format!("Claude CLI usage fallback returned invalid JSON: {err}"),
        )
    })?;

    let raw_payload = serde_json::from_str(&raw_output).unwrap_or_else(|_| json!({}));
    let usage = parse_usage_text(&response.result, Utc::now())?;
    Ok(ClaudeCliUsage {
        usage,
        raw_output: raw_payload,
    })
}

fn run_claude_usage_cli(config_dir: Option<&Path>) -> anyhow::Result<String> {
    let mut command = Command::new("claude");
    command
        .arg("-p")
        .arg("/usage")
        .arg("--output-format")
        .arg("json")
        .arg("--no-session-persistence")
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .env_remove("all_proxy")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(config_dir) = config_dir {
        command
            .env("CLAUDE_CONFIG_DIR", config_dir)
            .env_remove("CLAUDE_SECURESTORAGE_CONFIG_DIR");
    }
    let mut child = command.spawn()?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to open Claude CLI stdout"))?;
    let stdout_thread = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(MAX_CLAUDE_CLI_STDOUT_BYTES + 1)
            .read_to_end(&mut bytes)?;
        Ok::<_, std::io::Error>(bytes)
    });
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to open Claude CLI stderr"))?;
    let stderr_thread = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr
            .take(MAX_CLAUDE_CLI_STDERR_BYTES + 1)
            .read_to_end(&mut bytes)?;
        Ok::<_, std::io::Error>(bytes)
    });

    let status = match child.wait_timeout(CLAUDE_CLI_TIMEOUT)? {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("claude -p /usage timed out after {CLAUDE_CLI_TIMEOUT:?}");
        }
    };

    let stdout = stdout_thread
        .join()
        .map_err(|_| anyhow::anyhow!("Claude CLI stdout reader panicked"))??;
    let stderr = stderr_thread
        .join()
        .map_err(|_| anyhow::anyhow!("Claude CLI stderr reader panicked"))??;
    if stdout.len() > MAX_CLAUDE_CLI_STDOUT_BYTES as usize {
        anyhow::bail!(
            "claude -p /usage exceeded the {MAX_CLAUDE_CLI_STDOUT_BYTES}-byte stdout limit"
        );
    }
    if stderr.len() > MAX_CLAUDE_CLI_STDERR_BYTES as usize {
        anyhow::bail!(
            "claude -p /usage exceeded the {MAX_CLAUDE_CLI_STDERR_BYTES}-byte stderr limit"
        );
    }
    let stdout = String::from_utf8(stdout)?;
    let stderr = String::from_utf8_lossy(&stderr);

    if !status.success() {
        anyhow::bail!(
            "claude -p /usage exited with status {status}; stderr: {}",
            stderr.trim()
        );
    }

    Ok(stdout)
}

#[derive(Debug, Deserialize)]
struct ClaudePrintResponse {
    result: String,
}

fn parse_usage_text(
    text: &str,
    collected_at: DateTime<Utc>,
) -> Result<ProviderUsage, ProviderError> {
    let mut windows = Vec::new();
    let mut reset_text_by_window = BTreeMap::new();
    let mut pending: Option<ParsedUsageWindow> = None;

    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if let Some(window) = single_line_usage_window(line) {
            push_pending_window(
                pending.take(),
                collected_at,
                &mut windows,
                &mut reset_text_by_window,
            );
            push_pending_window(
                Some(window),
                collected_at,
                &mut windows,
                &mut reset_text_by_window,
            );
            continue;
        }

        if let Some(heading) = usage_heading(line) {
            push_pending_window(
                pending.take(),
                collected_at,
                &mut windows,
                &mut reset_text_by_window,
            );
            pending = Some(ParsedUsageWindow {
                heading,
                percent_used: None,
                reset_text: None,
            });
            continue;
        }

        let Some(window) = pending.as_mut() else {
            continue;
        };

        if window.percent_used.is_none() {
            if let Some(percent_used) = percent_used_from_line(line) {
                window.percent_used = Some(percent_used);
            }
            continue;
        }

        if let Some(reset_text) = reset_text_from_line(line) {
            window.reset_text = Some(reset_text.to_string());
            push_pending_window(
                pending.take(),
                collected_at,
                &mut windows,
                &mut reset_text_by_window,
            );
        }
    }

    push_pending_window(
        pending,
        collected_at,
        &mut windows,
        &mut reset_text_by_window,
    );

    if windows.is_empty() {
        return Err(ProviderError::new(
            ProviderErrorKind::Parse,
            "Claude CLI usage output did not contain usage windows",
        ));
    }

    Ok(ProviderUsage {
        provider_id: ProviderId::new(PROVIDER_ID),
        collected_at,
        windows,
        metadata: json!({
            "collection_mode": CLAUDE_CLI_COLLECTION_MODE,
            "command": "claude -p /usage --output-format json --no-session-persistence",
            "reset_text_by_window": reset_text_by_window,
        }),
    })
}

#[derive(Debug)]
struct ParsedUsageWindow {
    heading: String,
    percent_used: Option<f64>,
    reset_text: Option<String>,
}

fn single_line_usage_window(line: &str) -> Option<ParsedUsageWindow> {
    let (heading, detail) = line.split_once(':')?;
    let heading = usage_heading(heading)?;
    let percent_used = percent_used_from_line(detail)?;
    let reset_text = reset_text_from_line(detail).map(str::to_string);

    Some(ParsedUsageWindow {
        heading,
        percent_used: Some(percent_used),
        reset_text,
    })
}

fn push_pending_window(
    pending: Option<ParsedUsageWindow>,
    collected_at: DateTime<Utc>,
    windows: &mut Vec<UsageWindow>,
    reset_text_by_window: &mut BTreeMap<String, String>,
) {
    let Some(pending) = pending else {
        return;
    };
    let Some(percent_used) = pending.percent_used else {
        return;
    };

    let window_id = format!(
        "claude_cli_usage_{}",
        stable_window_fragment(&pending.heading)
    );
    if let Some(reset_text) = pending.reset_text.as_ref() {
        reset_text_by_window.insert(window_id.clone(), reset_text.clone());
    }

    windows.push(percent_window(
        window_id,
        claude_label(&pending.heading),
        usage_kind_from_name(&pending.heading),
        percent_used,
        pending
            .reset_text
            .as_deref()
            .and_then(|value| parse_reset_at(value, collected_at)),
    ));
}

fn usage_heading(line: &str) -> Option<String> {
    let value = line.trim();
    let lower = value.to_ascii_lowercase();
    if lower.starts_with("current ") && !lower.contains("% used") {
        Some(value.to_string())
    } else {
        None
    }
}

fn percent_window(
    window_id: String,
    label: String,
    kind: UsageWindowKind,
    percent_used: f64,
    reset_at: Option<DateTime<Utc>>,
) -> UsageWindow {
    let percent_used = percent_used.clamp(0.0, MAX_PERCENT);
    let percent_remaining = MAX_PERCENT - percent_used;

    UsageWindow {
        window_id,
        label,
        kind,
        used: Some(UsageAmount {
            value: percent_used,
            unit: UsageUnit::Percent,
        }),
        limit: Some(UsageAmount {
            value: MAX_PERCENT,
            unit: UsageUnit::Percent,
        }),
        remaining: Some(UsageAmount {
            value: percent_remaining,
            unit: UsageUnit::Percent,
        }),
        percent_used: Some(percent_used),
        percent_remaining: Some(percent_remaining),
        reset_at,
    }
}

fn percent_used_from_line(line: &str) -> Option<f64> {
    let marker = line.find("% used")?;
    let prefix = &line[..marker];
    let prefix = prefix.trim_end();
    let start = prefix
        .char_indices()
        .rev()
        .find(|(_, char)| !char.is_ascii_digit() && *char != '.')
        .map(|(index, char)| index + char.len_utf8())
        .unwrap_or(0);

    prefix[start..].parse().ok()
}

fn reset_text_from_line(line: &str) -> Option<&str> {
    let lower = line.to_ascii_lowercase();
    let index = lower.find("resets ")?;
    Some(line[index + "resets ".len()..].trim())
}

fn parse_reset_at(value: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let (body, tz) = split_timezone(value);
    let tz = tz
        .and_then(|value| value.parse::<Tz>().ok())
        .unwrap_or(chrono_tz::UTC);
    let local_now = now.with_timezone(&tz);

    if let Some((date, time)) = body.split_once(" at ") {
        let (month, day) = parse_month_day(date)?;
        let (hour, minute) = parse_time_of_day(time)?;
        let mut reset = local_datetime(tz, local_now.year(), month, day, hour, minute)?;
        if reset.with_timezone(&Utc) + chrono::Duration::hours(24) < now {
            reset = local_datetime(tz, local_now.year() + 1, month, day, hour, minute)?;
        }
        return Some(reset.with_timezone(&Utc));
    }

    let (hour, minute) = parse_time_of_day(body)?;
    let date = local_now.date_naive();
    let mut reset = local_datetime(tz, date.year(), date.month(), date.day(), hour, minute)?;
    if reset.with_timezone(&Utc) <= now {
        let tomorrow = date.checked_add_days(Days::new(1))?;
        reset = local_datetime(
            tz,
            tomorrow.year(),
            tomorrow.month(),
            tomorrow.day(),
            hour,
            minute,
        )?;
    }
    Some(reset.with_timezone(&Utc))
}

fn split_timezone(value: &str) -> (&str, Option<&str>) {
    let value = value.trim();
    let Some(open) = value.rfind('(') else {
        return (value, None);
    };
    let Some(close) = value[open..].find(')').map(|offset| open + offset) else {
        return (value, None);
    };

    (value[..open].trim(), Some(value[open + 1..close].trim()))
}

fn parse_month_day(value: &str) -> Option<(u32, u32)> {
    let mut parts = value.split_whitespace();
    let month = month_number(parts.next()?)?;
    let day = parts.next()?.trim_end_matches(',').parse().ok()?;
    Some((month, day))
}

fn month_number(value: &str) -> Option<u32> {
    match value.to_ascii_lowercase().as_str() {
        "jan" | "january" => Some(1),
        "feb" | "february" => Some(2),
        "mar" | "march" => Some(3),
        "apr" | "april" => Some(4),
        "may" => Some(5),
        "jun" | "june" => Some(6),
        "jul" | "july" => Some(7),
        "aug" | "august" => Some(8),
        "sep" | "sept" | "september" => Some(9),
        "oct" | "october" => Some(10),
        "nov" | "november" => Some(11),
        "dec" | "december" => Some(12),
        _ => None,
    }
}

fn parse_time_of_day(value: &str) -> Option<(u32, u32)> {
    let compact = value
        .trim()
        .to_ascii_lowercase()
        .replace(char::is_whitespace, "");
    let (time, is_pm) = compact
        .strip_suffix("am")
        .map(|time| (time, false))
        .or_else(|| compact.strip_suffix("pm").map(|time| (time, true)))?;
    let (hour, minute) = match time.split_once(':') {
        Some((hour, minute)) => (hour.parse::<u32>().ok()?, minute.parse::<u32>().ok()?),
        None => (time.parse::<u32>().ok()?, 0),
    };
    if hour == 0 || hour > 12 || minute > 59 {
        return None;
    }

    let hour = match (hour, is_pm) {
        (12, false) => 0,
        (12, true) => 12,
        (_, true) => hour + 12,
        _ => hour,
    };
    Some((hour, minute))
}

fn local_datetime(
    tz: Tz,
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
) -> Option<DateTime<Tz>> {
    tz.with_ymd_and_hms(year, month, day, hour, minute, 0)
        .earliest()
}

fn usage_kind_from_name(name: &str) -> UsageWindowKind {
    let name = name.to_ascii_lowercase();
    if name.contains("session") || name.contains("hour") {
        UsageWindowKind::Session
    } else if name.contains("daily") || name.contains("day") {
        UsageWindowKind::Daily
    } else if name.contains("weekly") || name.contains("week") {
        UsageWindowKind::Weekly
    } else if name.contains("monthly") || name.contains("month") {
        UsageWindowKind::Monthly
    } else {
        UsageWindowKind::Other(name)
    }
}

fn claude_label(value: &str) -> String {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return "Claude".to_string();
    };
    format!("Claude {}{}", first.to_ascii_lowercase(), chars.as_str())
}

fn stable_window_fragment(value: &str) -> String {
    value
        .chars()
        .map(|char| {
            if char.is_ascii_alphanumeric() {
                char.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_claude_usage_print_windows() {
        let now = Utc.with_ymd_and_hms(2026, 7, 7, 20, 0, 0).unwrap();
        let usage = parse_usage_text(
            r#"
You are currently using your subscription to power your Claude Code usage

Current session: 20% used · resets Jul 7 at 9:39pm (America/Los_Angeles)
Current week (all models): 25% used · resets Jul 7 at 6pm (America/Los_Angeles)
Current week (Fable): 17% used
"#,
            now,
        )
        .unwrap();

        assert_eq!(usage.windows.len(), 3);

        let session = find_window(&usage.windows, "claude_cli_usage_current_session");
        assert!(matches!(session.kind, UsageWindowKind::Session));
        assert_eq!(session.label, "Claude current session");
        assert_eq!(session.percent_used, Some(20.0));
        assert_eq!(session.percent_remaining, Some(80.0));
        assert_eq!(
            session.reset_at.unwrap(),
            Utc.with_ymd_and_hms(2026, 7, 8, 4, 39, 0).unwrap()
        );

        let all_models = find_window(&usage.windows, "claude_cli_usage_current_week__all_models_");
        assert!(matches!(all_models.kind, UsageWindowKind::Weekly));
        assert_eq!(all_models.percent_used, Some(25.0));
    }

    #[test]
    fn still_parses_multiline_usage_windows() {
        let now = Utc.with_ymd_and_hms(2026, 7, 7, 20, 0, 0).unwrap();
        let usage = parse_usage_text(
            r#"
Current session
██████████                                         20% used
Resets 9:40pm (America/Los_Angeles)
"#,
            now,
        )
        .unwrap();

        let session = find_window(&usage.windows, "claude_cli_usage_current_session");
        assert_eq!(session.percent_used, Some(20.0));
        assert_eq!(
            session.reset_at.unwrap(),
            Utc.with_ymd_and_hms(2026, 7, 8, 4, 40, 0).unwrap()
        );
    }

    fn find_window<'a>(windows: &'a [UsageWindow], window_id: &str) -> &'a UsageWindow {
        windows
            .iter()
            .find(|window| window.window_id == window_id)
            .unwrap_or_else(|| panic!("missing window {window_id}"))
    }
}
