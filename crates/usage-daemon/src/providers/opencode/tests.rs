use chrono::{Local, TimeDelta, TimeZone, Utc};
use rusqlite::{params, Connection};
use uuid::Uuid;

use super::cookies::{
    import_cookie_db_copy, import_firefox_cookie_db_copy, normalize_cookie_header,
    BrowserCookieStore, BrowserCookieStoreKind,
};
use super::history::{
    local_usage_history_report, parse_usage_history_report, parse_usage_history_rows,
    usage_history_report_from_rows, usage_history_windows,
};
use super::local::{read_local_usage_rows, LocalUsageRow};
use super::usage::{account_email_from_text, parse_usage_text, parse_zen_balance};
use super::OPENCODE_GO_PROVIDER_ID;

#[test]
fn parses_seroval_usage_windows() {
    let text = r#"
            rollingUsage:{usagePercent:12.5,resetInSec:3600},
            weeklyUsage:{usagePercent:60,resetInSec:604800},
            monthlyUsage:{usagePercent:75,resetInSec:1209600}
        "#;
    let parsed = parse_usage_text(text, true).unwrap();
    assert_eq!(parsed.rolling.percent_used, 12.5);
    assert_eq!(parsed.weekly.unwrap().percent_used, 60.0);
    assert_eq!(parsed.monthly.unwrap().percent_used, 75.0);
}

#[test]
fn parses_rendered_go_usage_cards() {
    let parsed = parse_usage_text(
            r#"
            <div data-slot="usage-item">
              <span data-slot="usage-label">Rolling Usage</span>
              <span data-slot="usage-value"><!--$-->3<!--/-->%</span>
              <span data-slot="reset-time"><!--$-->Resets in<!--/--> <!--$-->4 hours 29 minutes<!--/--></span>
            </div>
            <div data-slot="usage-item">
              <span data-slot="usage-label">Weekly Usage</span>
              <span data-slot="usage-value"><!--$-->11<!--/-->%</span>
              <span data-slot="reset-time">Resets in 3 days 20 hours</span>
            </div>
            <div data-slot="usage-item">
              <span data-slot="usage-label">Monthly Usage</span>
              <span data-slot="usage-value"><!--$-->6<!--/-->%</span>
              <span data-slot="reset-time">Resets in 24 days 15 hours</span>
            </div>
            "#,
            true,
        )
        .unwrap();

    assert_eq!(parsed.rolling.percent_used, 3.0);
    assert_eq!(parsed.weekly.unwrap().percent_used, 11.0);
    let monthly = parsed.monthly.unwrap();
    assert_eq!(monthly.percent_used, 6.0);
    assert!(monthly.reset_at.unwrap() > Utc::now() + TimeDelta::days(24));
}

#[test]
fn parses_json_usage_windows() {
    let parsed = parse_usage_text(
        r#"{
                "usage": {
                    "rollingUsage": {"used": 2, "limit": 10, "resetInSec": 300},
                    "weeklyUsage": {"usagePercent": 0.5, "resetInSec": 600}
                }
            }"#,
        false,
    )
    .unwrap();
    assert_eq!(parsed.rolling.percent_used, 20.0);
    assert_eq!(parsed.weekly.unwrap().percent_used, 50.0);
}

#[test]
fn parses_billing_scaled_zen_balance() {
    let text = r#""customerID":$R[1]="cus_123","balance":$R[0]=1234567890"#;
    assert_eq!(parse_zen_balance(text), Some(12.3456789));
}

#[test]
fn extracts_account_email_from_ssr_html() {
    let text = r#"<span data-hk="x">malvinniguitar@gmail.com</span>"#;
    assert_eq!(
        account_email_from_text(text).as_deref(),
        Some("malvinniguitar@gmail.com")
    );
}

#[test]
fn summarizes_web_usage_history_payload() {
    let text = r#"
          _$HY.r['usage.list["wrk_123",0]'] = $R[21];
          {
            timeCreated: $R[27] = new Date("2026-07-09T03:07:31.000Z"),
            inputTokens: 2733,
            outputTokens: 7296,
            reasoningTokens: 98,
            cacheReadTokens: 81920,
            cost: 3133334
          },
          {
            timeCreated: $R[31] = new Date("2026-07-09T03:05:51.000Z"),
            inputTokens: 5321,
            outputTokens: 171,
            reasoningTokens: null,
            cacheReadTokens: 76544,
            cost: 1096351
          }
        "#;

    let report = parse_usage_history_report(text).unwrap();
    let metadata = report.metadata_value();
    let expected_day = Utc
        .with_ymd_and_hms(2026, 7, 9, 3, 7, 31)
        .unwrap()
        .with_timezone(&Local)
        .date_naive()
        .to_string();

    assert_eq!(metadata["row_count"], 2);
    assert_eq!(metadata["total_tokens"], 174083);
    assert_eq!(metadata["by_day"][0]["date"], expected_day);
    assert!(
        (metadata["by_day"][0]["cost_usd"].as_f64().unwrap() - 0.04229685).abs() < f64::EPSILON
    );

    let windows = usage_history_windows(
        OPENCODE_GO_PROVIDER_ID,
        &report,
        Utc.with_ymd_and_hms(2026, 7, 9, 4, 0, 0).unwrap(),
    );
    assert!(windows
        .iter()
        .any(|window| window.window_id == "opencode_go_spend_today"));
    assert!(windows
        .iter()
        .any(|window| window.window_id == "opencode_go_tokens_30d"));
}

#[test]
fn summarizes_direct_usage_history_page_payload() {
    let text = r#"
          [
            {
              timeCreated: new Date("2026-07-09T03:07:31.000Z"),
              inputTokens: 2733,
              outputTokens: 7296,
              reasoningTokens: 98,
              cacheReadTokens: 81920,
              cacheWrite5mTokens: null,
              cacheWrite1hTokens: null,
              cost: 3133334
            }
          ]
        "#;

    let report = usage_history_report_from_rows(
        parse_usage_history_rows(text),
        "opencode_usage_page",
        false,
        true,
    )
    .unwrap();
    let metadata = report.metadata_value();

    assert_eq!(metadata["row_count"], 1);
    assert_eq!(metadata["partial"], false);
    assert_eq!(metadata["complete_lookback"], true);
    assert_eq!(metadata["total_tokens"], 92047);
    assert!((metadata["total_cost_usd"].as_f64().unwrap() - 0.03133334).abs() < f64::EPSILON);
}

#[test]
fn filters_auth_cookies_when_possible() {
    let header = normalize_cookie_header("foo=1; auth=a; __Host-auth=b; bar=2").unwrap();
    assert_eq!(header, "auth=a; __Host-auth=b");
}

#[test]
fn rejects_cookie_headers_without_usable_auth_cookies() {
    assert!(normalize_cookie_header("foo=1; bar=2").is_none());
    assert!(normalize_cookie_header("auth= ; foo=1").is_none());
}

#[test]
fn excludes_future_rows_from_local_history_counts() {
    let now = Utc.with_ymd_and_hms(2026, 7, 9, 12, 0, 0).unwrap();
    let rows = vec![
        LocalUsageRow {
            created_at: now - TimeDelta::minutes(1),
            cost: 1.25,
        },
        LocalUsageRow {
            created_at: now + TimeDelta::minutes(1),
            cost: 9.0,
        },
    ];

    let report = local_usage_history_report(&rows, now).unwrap();
    let metadata = report.metadata_value();
    assert_eq!(metadata["row_count"], 1);
    assert_eq!(metadata["total_cost_usd"], 1.25);

    assert!(local_usage_history_report(&rows[1..], now).is_none());
}

#[test]
fn local_fallback_exposes_activity_without_inventing_quotas() {
    let now = Utc.with_ymd_and_hms(2026, 7, 9, 12, 0, 0).unwrap();
    let report = local_usage_history_report(
        &[LocalUsageRow {
            created_at: now - TimeDelta::minutes(1),
            cost: 4.25,
        }],
        now,
    )
    .unwrap();
    let windows = usage_history_windows(OPENCODE_GO_PROVIDER_ID, &report, now);

    assert!(!windows.is_empty());
    assert!(windows.iter().all(|window| window.limit.is_none()
        && window.remaining.is_none()
        && window.percent_used.is_none()
        && window.percent_remaining.is_none()
        && window.reset_at.is_none()));
    assert!(windows.iter().all(|window| !matches!(
        window.kind,
        usage_core::UsageWindowKind::Session | usage_core::UsageWindowKind::Weekly
    )));
}

#[test]
fn reads_local_sqlite_message_and_part_usage() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        r#"
            CREATE TABLE message (
                id TEXT PRIMARY KEY,
                time_created INTEGER,
                data TEXT NOT NULL
            );
            CREATE TABLE part (
                id TEXT PRIMARY KEY,
                message_id TEXT NOT NULL,
                data TEXT NOT NULL
            );
            "#,
    )
    .unwrap();
    conn.execute(
            "INSERT INTO message (id, time_created, data) VALUES (?1, ?2, ?3)",
            params![
                "m1",
                1_800_000_000_000_i64,
                r#"{"providerID":"opencode-go","role":"assistant","time":{"created":1800000000000},"cost":1.25}"#,
            ],
        )
        .unwrap();
    conn.execute(
        "INSERT INTO message (id, time_created, data) VALUES (?1, ?2, ?3)",
        params![
            "m2",
            1_800_000_100_000_i64,
            r#"{"providerID":"opencode-go","role":"assistant","time":{"created":1800000100000}}"#,
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO part (id, message_id, data) VALUES (?1, ?2, ?3)",
        params![
            "p1",
            "m2",
            r#"{"type":"step-finish","time":{"created":1800000100000},"cost":2.5}"#,
        ],
    )
    .unwrap();

    let rows = read_local_usage_rows(&conn).unwrap();
    assert_eq!(rows.len(), 2);
    assert!((rows.iter().map(|row| row.cost).sum::<f64>() - 3.75).abs() < f64::EPSILON);

    let now = rows.iter().map(|row| row.created_at).max().unwrap() + TimeDelta::seconds(1);
    let report = local_usage_history_report(&rows, now).unwrap();
    let metadata = report.metadata_value();
    assert_eq!(metadata["source"], "opencode_local_sqlite");
    assert_eq!(metadata["total_cost_usd"], 3.75);
    assert!(!metadata["by_day"].as_array().unwrap().is_empty());
}

#[test]
fn imports_plaintext_browser_cookie_db() {
    let path = std::env::temp_dir().join(format!(
        "usagetracker-cookie-test-{}.sqlite",
        Uuid::new_v4()
    ));
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        r#"
            CREATE TABLE cookies (
                host_key TEXT NOT NULL,
                name TEXT NOT NULL,
                value TEXT NOT NULL,
                encrypted_value BLOB NOT NULL,
                expires_utc INTEGER,
                last_access_utc INTEGER,
                creation_utc INTEGER
            );
            "#,
    )
    .unwrap();
    conn.execute(
            "INSERT INTO cookies (host_key, name, value, encrypted_value, expires_utc, last_access_utc, creation_utc) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![".opencode.ai", "auth", "a", Vec::<u8>::new(), 10_i64, 10_i64, 10_i64],
        )
        .unwrap();
    conn.execute(
            "INSERT INTO cookies (host_key, name, value, encrypted_value, expires_utc, last_access_utc, creation_utc) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params!["opencode.ai", "__Host-auth", "b", Vec::<u8>::new(), 10_i64, 10_i64, 10_i64],
        )
        .unwrap();
    drop(conn);

    let browser = BrowserCookieStore {
        label: "Test",
        app_support_path: "Test",
        keychain_service: "Test Safe Storage",
        keychain_account: "Test",
        kind: BrowserCookieStoreKind::Chromium,
    };
    let header = import_cookie_db_copy(&path, browser).unwrap().unwrap();
    let _ = std::fs::remove_file(path);
    assert_eq!(header, "auth=a; __Host-auth=b");
}

#[test]
fn imports_firefox_cookie_db() {
    let path = std::env::temp_dir().join(format!(
        "usagetracker-firefox-cookie-test-{}.sqlite",
        Uuid::new_v4()
    ));
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        r#"
            CREATE TABLE moz_cookies (
                host TEXT NOT NULL,
                name TEXT NOT NULL,
                value TEXT NOT NULL,
                expiry INTEGER,
                lastAccessed INTEGER,
                creationTime INTEGER
            );
            "#,
    )
    .unwrap();
    conn.execute(
            "INSERT INTO moz_cookies (host, name, value, expiry, lastAccessed, creationTime) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![".opencode.ai", "auth", "a", 10_i64, 10_i64, 10_i64],
        )
        .unwrap();
    conn.execute(
            "INSERT INTO moz_cookies (host, name, value, expiry, lastAccessed, creationTime) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params!["app.opencode.ai", "__Host-auth", "b", 10_i64, 10_i64, 10_i64],
        )
        .unwrap();

    let header = import_firefox_cookie_db_copy(&conn).unwrap().unwrap();
    drop(conn);
    let _ = std::fs::remove_file(path);
    assert_eq!(header, "auth=a; __Host-auth=b");
}
