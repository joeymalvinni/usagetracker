//! Best-effort local Grok session diagnostics.
//!
//! Local token counts are intentionally never projected into quota windows:
//! they cover this machine only and do not have the product weighting used by
//! Grok's account-wide billing pool.

use std::{
    cmp::Reverse,
    collections::{BTreeSet, BinaryHeap},
    fs,
    path::{Path, PathBuf},
};

use chrono::{DateTime, TimeDelta, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::providers::{ProviderError, ProviderErrorKind};

const DEFAULT_LOOKBACK_DAYS: i64 = 30;
const MAX_SIGNAL_BYTES: u64 = 1024 * 1024;
const MAX_SESSIONS: usize = 10_000;

#[derive(Debug, Default, Eq, PartialEq)]
pub(super) struct LocalSessionSummary {
    session_count: usize,
    total_tokens: u64,
    last_session_at: Option<DateTime<Utc>>,
    primary_model_id: Option<String>,
    models_used: BTreeSet<String>,
}

impl LocalSessionSummary {
    pub(super) fn metadata(&self) -> Value {
        json!({
            "source": "grok_local_sessions",
            "lookback_days": DEFAULT_LOOKBACK_DAYS,
            "session_count": self.session_count,
            "total_tokens": self.total_tokens,
            "last_session_at": self.last_session_at,
            "primary_model_id": self.primary_model_id,
            "models_used": self.models_used,
            "scope": "this_device",
        })
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct SessionSignals {
    #[serde(alias = "total_tokens_before_compaction")]
    total_tokens_before_compaction: u64,
    #[serde(alias = "context_tokens_used")]
    context_tokens_used: u64,
    #[serde(alias = "primary_model_id", alias = "modelId", alias = "model")]
    primary_model_id: Option<String>,
    #[serde(alias = "models_used")]
    models_used: Vec<String>,
    #[serde(
        alias = "last_session_at",
        alias = "updatedAt",
        alias = "updated_at",
        alias = "createdAt",
        alias = "created_at"
    )]
    last_session_at: Option<Value>,
}

pub(super) fn scan_default() -> Result<LocalSessionSummary, ProviderError> {
    scan(&super::auth::grok_home()?.join("sessions"), Utc::now())
}

fn scan(root: &Path, now: DateTime<Utc>) -> Result<LocalSessionSummary, ProviderError> {
    if !root.is_dir() {
        return Ok(LocalSessionSummary::default());
    }
    let cutoff = now - TimeDelta::days(DEFAULT_LOOKBACK_DAYS);
    let mut signal_paths = BinaryHeap::new();
    for encoded_cwd in read_dirs(root)? {
        for session in read_dirs(&encoded_cwd)? {
            let path = session.join("signals.json");
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            let Some(modified_at) = metadata.modified().ok() else {
                continue;
            };
            if !metadata.file_type().is_file()
                || metadata.len() > MAX_SIGNAL_BYTES
                || DateTime::<Utc>::from(modified_at) < cutoff
            {
                continue;
            }
            signal_paths.push(Reverse((modified_at, path)));
            if signal_paths.len() > MAX_SESSIONS {
                signal_paths.pop();
            }
        }
    }

    let mut summary = LocalSessionSummary::default();
    let mut primary_model_time = None;
    for Reverse((modified_at, path)) in signal_paths {
        let modified_at = Some(DateTime::<Utc>::from(modified_at));
        let Ok(data) = fs::read(&path) else { continue };
        let Ok(signals) = serde_json::from_slice::<SessionSignals>(&data) else {
            continue;
        };
        let observed_at = signals
            .last_session_at
            .as_ref()
            .and_then(parse_timestamp)
            .or(modified_at)
            .unwrap_or(now);
        if observed_at < cutoff || observed_at > now + TimeDelta::days(1) {
            continue;
        }

        summary.session_count += 1;
        summary.total_tokens = summary.total_tokens.saturating_add(
            signals
                .total_tokens_before_compaction
                .saturating_add(signals.context_tokens_used),
        );
        summary.last_session_at = Some(
            summary
                .last_session_at
                .map_or(observed_at, |current| current.max(observed_at)),
        );
        let primary = clean_model(signals.primary_model_id);
        if let Some(model) = primary.as_ref() {
            summary.models_used.insert(model.clone());
        }
        summary.models_used.extend(
            signals
                .models_used
                .into_iter()
                .filter_map(|model| clean_model(Some(model))),
        );
        if primary.is_some() && primary_model_time.is_none_or(|current| observed_at > current) {
            summary.primary_model_id = primary;
            primary_model_time = Some(observed_at);
        }
    }
    Ok(summary)
}

fn read_dirs(root: &Path) -> Result<Vec<PathBuf>, ProviderError> {
    let entries = fs::read_dir(root).map_err(|err| {
        ProviderError::new(
            ProviderErrorKind::ProviderUnavailable,
            format!("could not scan Grok sessions at {}: {err}", root.display()),
        )
    })?;
    Ok(entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect())
}

fn clean_model(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn parse_timestamp(value: &Value) -> Option<DateTime<Utc>> {
    if let Some(value) = value.as_str() {
        return DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|value| value.with_timezone(&Utc));
    }
    let raw = value.as_i64()?;
    let seconds = if raw > 10_000_000_000 {
        raw / 1_000
    } else {
        raw
    };
    DateTime::from_timestamp(seconds, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregates_recent_sessions_without_creating_quota_data() {
        let root = std::env::temp_dir().join(format!("grok-sessions-{}", uuid::Uuid::new_v4()));
        let session = root.join("encoded-cwd/session-1");
        fs::create_dir_all(&session).unwrap();
        fs::write(
            session.join("signals.json"),
            br#"{
                "totalTokensBeforeCompaction": 1200,
                "contextTokensUsed": 300,
                "primaryModelId": "grok-code-fast-1",
                "modelsUsed": ["grok-code-fast-1", "grok-4"],
                "lastSessionAt": "2026-07-10T12:00:00Z"
            }"#,
        )
        .unwrap();

        let now = DateTime::parse_from_rfc3339("2026-07-11T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let summary = scan(&root, now).unwrap();
        fs::remove_dir_all(root).unwrap();

        assert_eq!(summary.session_count, 1);
        assert_eq!(summary.total_tokens, 1500);
        assert_eq!(
            summary.primary_model_id.as_deref(),
            Some("grok-code-fast-1")
        );
        assert_eq!(summary.models_used.len(), 2);
    }

    #[test]
    fn timestamp_parser_accepts_rfc3339_and_unix_milliseconds() {
        assert_eq!(
            parse_timestamp(&json!(1_800_000_000_000_i64))
                .unwrap()
                .timestamp(),
            1_800_000_000
        );
        assert_eq!(
            parse_timestamp(&json!("2026-07-11T12:00:00Z"))
                .unwrap()
                .timestamp(),
            1_783_771_200
        );
    }

    #[test]
    fn ignores_oversized_signal_files() {
        let root = std::env::temp_dir().join(format!("grok-sessions-{}", uuid::Uuid::new_v4()));
        let session = root.join("encoded-cwd/session-1");
        fs::create_dir_all(&session).unwrap();
        fs::write(
            session.join("signals.json"),
            vec![b' '; MAX_SIGNAL_BYTES as usize + 1],
        )
        .unwrap();
        let summary = scan(&root, Utc::now()).unwrap();
        fs::remove_dir_all(root).unwrap();
        assert_eq!(summary.session_count, 0);
    }
}
