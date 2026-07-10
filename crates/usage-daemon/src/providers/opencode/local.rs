//! Local OpenCode SQLite usage fallback.

use std::path::PathBuf;

use chrono::{DateTime, TimeDelta, Utc};
use rusqlite::Connection;
use serde_json::{json, Value};
use usage_core::{ProviderId, UsageAmount, UsageUnit, UsageWindow, UsageWindowKind};

use crate::providers::{ProviderCollectionResult, ProviderError, ProviderErrorKind, ProviderUsage};

use super::{
    history::{local_usage_history_report, usage_history_windows},
    utils::{
        local_db_error, monthly_window_start, next_monthly_anchor, normalize_percent, table_exists,
        utc_week_start,
    },
    MAX_PERCENT, OPENCODE_GO_PROVIDER_ID,
};

pub(super) fn collect_go_local_usage() -> Result<ProviderCollectionResult, ProviderError> {
    if !local_go_auth_exists() {
        return Err(ProviderError::new(
            ProviderErrorKind::CredentialsMissing,
            "OpenCode Go local auth key was not found",
        ));
    }
    let db_path = opencode_data_dir()?.join("opencode.db");
    let conn = Connection::open(&db_path).map_err(|err| {
        ProviderError::new(
            ProviderErrorKind::ProviderUnavailable,
            format!("failed to open OpenCode local database: {err}"),
        )
    })?;
    let rows = read_local_usage_rows(&conn)?;
    if rows.is_empty() {
        return Err(ProviderError::new(
            ProviderErrorKind::Parse,
            "OpenCode Go local database had no usage rows",
        ));
    }

    let now = Utc::now();
    let history_report = local_usage_history_report(&rows, now);
    let mut windows = local_usage_windows(&rows, now);
    if let Some(report) = &history_report {
        windows.extend(usage_history_windows(OPENCODE_GO_PROVIDER_ID, report, now));
    }
    let mut metadata = json!({
        "collection_mode": "opencode_go_local_sqlite",
        "estimate": true,
        "database": db_path.display().to_string(),
        "rows": rows.len(),
        "web_authoritative": false,
    });
    if let Some(report) = history_report {
        if let Some(object) = metadata.as_object_mut() {
            object.insert("opencode_go_cost".to_string(), report.metadata_value());
        }
    }
    Ok(ProviderCollectionResult {
        usage: ProviderUsage {
            provider_id: ProviderId::new(OPENCODE_GO_PROVIDER_ID),
            collected_at: now,
            windows,
            metadata,
        },
        daily_usage: Vec::new(),
        collection_mode: "opencode_go_local_sqlite".to_string(),
        account_display_name: Some("OpenCode Go local".to_string()),
        raw_payload: None,
        warnings: Vec::new(),
    })
}

#[derive(Clone, Debug)]
pub(super) struct LocalUsageRow {
    pub(super) created_at: DateTime<Utc>,
    pub(super) cost: f64,
}

pub(super) fn read_local_usage_rows(
    conn: &Connection,
) -> Result<Vec<LocalUsageRow>, ProviderError> {
    let has_part_table = table_exists(conn, "part")?;
    let sql = if has_part_table {
        r#"
        WITH message_costs AS (
          SELECT
            id AS messageID,
            CAST(COALESCE(json_extract(data, '$.time.created'), time_created) AS INTEGER) AS createdMs,
            CAST(json_extract(data, '$.cost') AS REAL) AS cost
          FROM message
          WHERE json_valid(data)
            AND json_extract(data, '$.providerID') = 'opencode-go'
            AND json_extract(data, '$.role') = 'assistant'
            AND json_type(data, '$.cost') IN ('integer', 'real')
        )
        SELECT createdMs, cost FROM message_costs
        UNION ALL
        SELECT
          CAST(COALESCE(json_extract(p.data, '$.time.created'), json_extract(m.data, '$.time.created'), m.time_created) AS INTEGER) AS createdMs,
          CAST(json_extract(p.data, '$.cost') AS REAL) AS cost
        FROM part p
        JOIN message m ON m.id = p.message_id
        WHERE json_valid(p.data)
          AND json_valid(m.data)
          AND json_extract(m.data, '$.providerID') = 'opencode-go'
          AND json_extract(m.data, '$.role') = 'assistant'
          AND json_extract(p.data, '$.type') = 'step-finish'
          AND json_type(p.data, '$.cost') IN ('integer', 'real')
          AND NOT EXISTS (SELECT 1 FROM message_costs WHERE messageID = p.message_id)
        "#
    } else {
        r#"
        SELECT
          CAST(COALESCE(json_extract(data, '$.time.created'), time_created) AS INTEGER) AS createdMs,
          CAST(json_extract(data, '$.cost') AS REAL) AS cost
        FROM message
        WHERE json_valid(data)
          AND json_extract(data, '$.providerID') = 'opencode-go'
          AND json_extract(data, '$.role') = 'assistant'
          AND json_type(data, '$.cost') IN ('integer', 'real')
        "#
    };
    let mut stmt = conn.prepare(sql).map_err(local_db_error)?;
    let rows = stmt
        .query_map([], |row| {
            let created_ms: i64 = row.get(0)?;
            let cost: f64 = row.get(1)?;
            Ok((created_ms, cost))
        })
        .map_err(local_db_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(local_db_error)?;

    Ok(rows
        .into_iter()
        .filter_map(|(created_ms, cost)| {
            cost.is_finite().then(|| {
                DateTime::from_timestamp_millis(created_ms)
                    .map(|created_at| LocalUsageRow { created_at, cost })
            })?
        })
        .collect())
}

fn local_usage_windows(rows: &[LocalUsageRow], now: DateTime<Utc>) -> Vec<UsageWindow> {
    let session_start = now - TimeDelta::hours(5);
    let session_cost = rows
        .iter()
        .filter(|row| row.created_at >= session_start && row.created_at <= now)
        .map(|row| row.cost)
        .sum::<f64>();
    let session_reset = rows
        .iter()
        .filter(|row| row.created_at >= session_start && row.created_at <= now)
        .map(|row| row.created_at + TimeDelta::hours(5))
        .min();

    let weekly_start = utc_week_start(now);
    let weekly_cost = rows
        .iter()
        .filter(|row| row.created_at >= weekly_start && row.created_at <= now)
        .map(|row| row.cost)
        .sum::<f64>();
    let weekly_reset = weekly_start + TimeDelta::weeks(1);

    let anchor = rows.iter().map(|row| row.created_at).min().unwrap_or(now);
    let monthly_start = monthly_window_start(anchor, now);
    let monthly_cost = rows
        .iter()
        .filter(|row| row.created_at >= monthly_start && row.created_at <= now)
        .map(|row| row.cost)
        .sum::<f64>();
    let monthly_reset = next_monthly_anchor(anchor, monthly_start);

    vec![
        local_cost_limit_window(
            "opencode_go_session",
            "OpenCode Go session",
            UsageWindowKind::Session,
            session_cost,
            12.0,
            session_reset,
        ),
        local_cost_limit_window(
            "opencode_go_weekly",
            "OpenCode Go weekly",
            UsageWindowKind::Weekly,
            weekly_cost,
            30.0,
            Some(weekly_reset),
        ),
        local_cost_limit_window(
            "opencode_go_monthly",
            "OpenCode Go monthly",
            UsageWindowKind::Monthly,
            monthly_cost,
            60.0,
            Some(monthly_reset),
        ),
    ]
}

fn local_cost_limit_window(
    window_id: &str,
    label: &str,
    kind: UsageWindowKind,
    used: f64,
    limit: f64,
    reset_at: Option<DateTime<Utc>>,
) -> UsageWindow {
    let percent_used = normalize_percent((used / limit) * 100.0);
    UsageWindow {
        window_id: window_id.to_string(),
        label: label.to_string(),
        kind,
        used: Some(UsageAmount {
            value: used,
            unit: UsageUnit::Usd,
        }),
        limit: Some(UsageAmount {
            value: limit,
            unit: UsageUnit::Usd,
        }),
        remaining: Some(UsageAmount {
            value: (limit - used).max(0.0),
            unit: UsageUnit::Usd,
        }),
        percent_used: Some(percent_used),
        percent_remaining: Some(MAX_PERCENT - percent_used),
        reset_at,
    }
}

pub(super) fn local_go_auth_exists() -> bool {
    let Ok(data_dir) = opencode_data_dir() else {
        return false;
    };
    let Ok(contents) = std::fs::read_to_string(data_dir.join("auth.json")) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(&contents) else {
        return false;
    };
    value
        .get("opencode-go")
        .and_then(|value| value.get("key"))
        .and_then(Value::as_str)
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
}

fn opencode_data_dir() -> Result<PathBuf, ProviderError> {
    let home = dirs::home_dir().ok_or_else(|| {
        ProviderError::new(
            ProviderErrorKind::ProviderUnavailable,
            "failed to resolve home directory for OpenCode",
        )
    })?;
    Ok(home.join(".local/share/opencode"))
}
