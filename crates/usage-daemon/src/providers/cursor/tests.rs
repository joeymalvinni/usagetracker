use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

use chrono::{TimeZone, Utc};
use reqwest::Url;
use rusqlite::Connection;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use usage_core::{UsageDataScope, UsageUnit};

use crate::providers::{AuthoritativeOutcome, CollectionOutcome};

use super::{
    auth::{load_cursor_app_session_from, SessionCredential, SessionSource},
    client::{CursorClient, CursorFetch},
    events::CursorUsageEventsPage,
    model::{normalize_cursor_fetch, CursorUsageResponse, CursorUsageSummary, CursorUserInfo},
    SessionCache,
};

fn parsed_fetch(summary: &str, identity: &str, legacy: Option<&str>) -> CursorFetch {
    CursorFetch {
        summary: serde_json::from_str::<CursorUsageSummary>(summary).unwrap(),
        identity: Some(serde_json::from_str::<CursorUserInfo>(identity).unwrap()),
        legacy: legacy.map(|value| serde_json::from_str::<CursorUsageResponse>(value).unwrap()),
        event_pages: None,
        event_warning: None,
    }
}

#[test]
fn normalizes_complete_event_history_once_by_day_and_model() {
    let mut fetch = parsed_fetch(
        r#"{
            "billingCycleStart":"2026-07-01T00:00:00Z",
            "billingCycleEnd":"2099-08-01T00:00:00Z",
            "individualUsage":{"plan":{"used":25,"limit":100}}
        }"#,
        r#"{"sub":"user-1"}"#,
        None,
    );
    fetch.event_pages = Some(
        [
            r#"{
            "totalUsageEventsCount":2,
            "usageEventsDisplay":[{
                "timestamp":"1783684800000",
                "model":"claude-sonnet",
                "kind":"USAGE_EVENT_KIND_USAGE_BASED",
                "requestsCosts":1,
                "isTokenBasedCall":true,
                "isChargeable":true,
                "chargedCents":30,
                "cursorTokenFee":2,
                "tokenUsage":{
                    "inputTokens":10,
                    "outputTokens":5,
                    "cacheReadTokens":20,
                    "totalCents":25
                }
            }]
        }"#,
            r#"{
            "totalUsageEventsCount":2,
            "usageEventsDisplay":[{
                "timestamp":"1783771200000",
                "model":"claude-sonnet",
                "kind":"USAGE_EVENT_KIND_USAGE_BASED",
                "requestsCosts":1,
                "isTokenBasedCall":true,
                "isChargeable":false,
                "chargedCents":15,
                "cursorTokenFee":1,
                "tokenUsage":{"inputTokens":3,"outputTokens":2,"totalCents":10}
            }]
        }"#,
        ]
        .into_iter()
        .map(|page| serde_json::from_str::<CursorUsageEventsPage>(page).unwrap())
        .collect(),
    );

    let normalized = normalize_cursor_fetch(fetch, SessionSource::CursorApp, "user-1").unwrap();
    let batch = normalized.collection.usage_events.as_ref().unwrap();

    assert_eq!(batch.events.len(), 2);
    assert_ne!(batch.events[0].event_id, batch.events[1].event_id);
    assert_eq!(normalized.collection.daily_usage.len(), 2);
    assert_eq!(
        normalized.collection.usage.metadata["cursor_cost"]["total"]["tokens"],
        40
    );
    assert_eq!(
        normalized.collection.usage.metadata["cursor_cost"]["total"]["metered_cost_usd"],
        0.45
    );
    assert_eq!(
        normalized.collection.usage.metadata["cursor_cost"]["total"]["chargeable_cost_usd"],
        0.3
    );
    assert_eq!(
        normalized.collection.usage.metadata["cursor_cost"]["by_model"][0]["model"],
        "claude-sonnet"
    );
}

#[test]
fn normalizes_plan_lanes_and_personal_on_demand() {
    let fetch = parsed_fetch(
        r#"{
            "billingCycleStart":"2026-07-01T00:00:00Z",
            "billingCycleEnd":"2026-08-01T00:00:00Z",
            "membershipType":"pro",
            "individualUsage":{
                "plan":{
                    "used":1500,
                    "limit":5000,
                    "remaining":3500,
                    "autoPercentUsed":20,
                    "apiPercentUsed":40,
                    "totalPercentUsed":30
                },
                "onDemand":{"used":500,"limit":10000,"remaining":9500}
            }
        }"#,
        r#"{"sub":"auth0|user-1","email":"user@example.com","name":"User"}"#,
        None,
    );

    let normalized = normalize_cursor_fetch(fetch, SessionSource::CursorApp, "user-1").unwrap();

    assert_eq!(normalized.scope, UsageDataScope::AccountWide);
    let windows = &normalized.collection.usage.windows;
    assert_eq!(
        windows
            .iter()
            .map(|window| window.window_id.as_str())
            .collect::<Vec<_>>(),
        [
            "cursor_total",
            "cursor_auto",
            "cursor_api",
            "cursor_on_demand"
        ]
    );
    assert_eq!(windows[0].percent_used, Some(30.0));
    assert_eq!(windows[0].used.as_ref().unwrap().value, 15.0);
    assert!(matches!(
        windows[0].used.as_ref().unwrap().unit,
        UsageUnit::Usd
    ));
    assert_eq!(windows[3].limit.as_ref().unwrap().value, 100.0);
    assert_eq!(
        normalized.collection.account_email.as_deref(),
        Some("user@example.com")
    );
    assert!(normalized.supplemental.is_empty());
}

#[test]
fn treats_fractional_percent_fields_as_percent_units() {
    let fetch = parsed_fetch(
        r#"{
            "membershipType":"pro",
            "individualUsage":{"plan":{"autoPercentUsed":0.36}}
        }"#,
        r#"{"sub":"user-1"}"#,
        None,
    );

    let normalized = normalize_cursor_fetch(fetch, SessionSource::Browser, "user-1").unwrap();

    assert_eq!(
        normalized.collection.usage.windows[0].percent_used,
        Some(0.36)
    );
    assert_eq!(
        normalized.collection.usage.windows[1].percent_used,
        Some(0.36)
    );
}

#[test]
fn enterprise_personal_overall_precedes_team_pool() {
    let fetch = parsed_fetch(
        r#"{
            "billingCycleEnd":"2026-08-01T00:00:00Z",
            "membershipType":"enterprise",
            "limitType":"team",
            "individualUsage":{
                "overall":{"used":7384,"limit":10000,"remaining":2616}
            },
            "teamUsage":{
                "pooled":{"used":12725135,"limit":28122000,"remaining":15396865}
            }
        }"#,
        r#"{"sub":"user-1"}"#,
        None,
    );

    let normalized = normalize_cursor_fetch(fetch, SessionSource::CursorApp, "user-1").unwrap();
    let total = &normalized.collection.usage.windows[0];

    assert_eq!(normalized.scope, UsageDataScope::AccountWide);
    assert_eq!(total.label, "Cursor total");
    assert!((total.percent_used.unwrap() - 73.84).abs() < 0.000_001);
    assert_eq!(total.used.as_ref().unwrap().value, 73.84);
    assert_eq!(
        normalized.collection.usage.metadata["headline_source"],
        "overall"
    );
}

#[test]
fn pooled_only_usage_is_explicitly_organization_scoped() {
    let fetch = parsed_fetch(
        r#"{
            "membershipType":"enterprise",
            "teamUsage":{"pooled":{"used":25,"limit":100}}
        }"#,
        r#"{"sub":"user-1"}"#,
        None,
    );

    let normalized = normalize_cursor_fetch(fetch, SessionSource::CursorApp, "user-1").unwrap();

    assert_eq!(normalized.scope, UsageDataScope::Organization);
    assert_eq!(
        normalized.collection.usage.windows[0].label,
        "Cursor team pool"
    );
    assert_eq!(
        normalized.collection.usage.windows[0].percent_used,
        Some(25.0)
    );
}

#[test]
fn pooled_quota_keeps_personal_event_history_account_scoped() {
    let mut fetch = parsed_fetch(
        r#"{
            "billingCycleStart":"2026-07-01T00:00:00Z",
            "billingCycleEnd":"2099-08-01T00:00:00Z",
            "membershipType":"enterprise",
            "individualUsage":{"onDemand":{"used":10,"limit":100}},
            "teamUsage":{"pooled":{"used":25,"limit":100}}
        }"#,
        r#"{"sub":"user-1"}"#,
        None,
    );
    fetch.event_pages = Some(vec![serde_json::from_str::<CursorUsageEventsPage>(
        r#"{
                "totalUsageEventsCount":1,
                "usageEventsDisplay":[{
                    "timestamp":"1783684800000",
                    "model":"claude-sonnet",
                    "kind":"USAGE_EVENT_KIND_USAGE_BASED",
                    "isTokenBasedCall":true,
                    "tokenUsage":{"inputTokens":10,"outputTokens":5}
                }]
            }"#,
    )
    .unwrap()]);

    let normalized = normalize_cursor_fetch(fetch, SessionSource::CursorApp, "user-1").unwrap();

    assert_eq!(normalized.scope, UsageDataScope::Organization);
    assert_eq!(normalized.collection.usage.windows.len(), 1);
    assert_eq!(
        normalized.collection.usage.windows[0].window_id,
        "cursor_total"
    );
    assert!(normalized.collection.daily_usage.is_empty());
    assert!(normalized.collection.usage_events.is_none());
    assert!(normalized
        .collection
        .usage
        .metadata
        .get("cursor_cost")
        .is_none());
    assert_eq!(normalized.supplemental.len(), 1);
    assert_eq!(
        normalized.supplemental[0].source_id,
        "cursor_personal_usage"
    );
    assert_eq!(
        normalized.supplemental[0].provenance.scope,
        UsageDataScope::AccountWide
    );
    assert_eq!(
        normalized.supplemental[0].collection.usage.windows[0].window_id,
        "cursor_on_demand"
    );
    assert_eq!(normalized.supplemental[0].collection.daily_usage.len(), 1);
    assert_eq!(
        normalized.supplemental[0]
            .collection
            .usage_events
            .as_ref()
            .unwrap()
            .events
            .len(),
        1
    );

    let outcome = CollectionOutcome::collected_scoped_with_supplemental(
        normalized.collection,
        normalized.scope,
        normalized.supplemental,
    );
    let AuthoritativeOutcome::Collected(primary) = outcome.authoritative else {
        panic!("Cursor pooled quota should remain authoritative");
    };
    assert_eq!(primary.provenance.scope, UsageDataScope::Organization);
    assert_eq!(
        outcome.supplemental[0].provenance.scope,
        UsageDataScope::AccountWide
    );
}

#[test]
fn legacy_request_quota_replaces_money_and_lane_windows() {
    let fetch = parsed_fetch(
        r#"{
            "individualUsage":{
                "plan":{"totalPercentUsed":80,"autoPercentUsed":60,"apiPercentUsed":100}
            }
        }"#,
        r#"{"sub":"user-1"}"#,
        Some(r#"{"gpt-4":{"numRequestsTotal":120,"maxRequestUsage":500}}"#),
    );

    let normalized = normalize_cursor_fetch(fetch, SessionSource::CursorApp, "user-1").unwrap();
    let windows = &normalized.collection.usage.windows;

    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].window_id, "cursor_total");
    assert_eq!(windows[0].percent_used, Some(24.0));
    assert!(matches!(
        windows[0].used.as_ref().unwrap().unit,
        UsageUnit::Requests
    ));
}

#[test]
fn team_budget_is_a_non_personal_supplement_when_plan_is_personal() {
    let fetch = parsed_fetch(
        r#"{
            "individualUsage":{"plan":{"used":20,"limit":100}},
            "teamUsage":{"onDemand":{"used":2500,"limit":10000}}
        }"#,
        r#"{"sub":"user-1"}"#,
        None,
    );

    let normalized = normalize_cursor_fetch(fetch, SessionSource::CursorApp, "user-1").unwrap();

    assert_eq!(normalized.scope, UsageDataScope::AccountWide);
    assert_eq!(normalized.supplemental.len(), 1);
    assert_eq!(
        normalized.supplemental[0].provenance.scope,
        UsageDataScope::Organization
    );
    assert!(!normalized.supplemental[0].authoritative);
    assert_eq!(
        normalized.supplemental[0].collection.usage.windows[0].window_id,
        "cursor_team_on_demand"
    );
}

#[test]
fn reads_cursor_app_jwt_without_persisting_it_elsewhere() {
    let path = std::env::temp_dir().join(format!(
        "usagetracker-cursor-auth-{}.sqlite",
        uuid::Uuid::new_v4()
    ));
    let conn = Connection::open(&path).unwrap();
    conn.execute(
        "CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
        [],
    )
    .unwrap();
    let claims = serde_json::json!({
        "sub": "auth0|cursor-user",
        "exp": Utc::now().timestamp() + 3600
    });
    let token = format!(
        "header.{}.signature",
        base64url(&serde_json::to_vec(&claims).unwrap())
    );
    conn.execute(
        "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
        ["cursorAuth/accessToken", token.as_str()],
    )
    .unwrap();
    drop(conn);

    let session = load_cursor_app_session_from(&path).unwrap().unwrap();

    assert_eq!(session.source, SessionSource::CursorApp);
    assert_eq!(session.account_hint.as_deref(), Some("cursor-user"));
    assert_eq!(
        session.cookie_header,
        format!("WorkosCursorSessionToken=cursor-user%3A%3A{token}")
    );
    std::fs::remove_file(path).unwrap();
}

#[test]
fn conditional_cache_removal_does_not_clear_a_replacement_session() {
    let cache = SessionCache::default();
    let first = cache.store("account".to_string(), credential("first"));
    let replacement = cache.store("account".to_string(), credential("replacement"));

    cache.remove_if_generation("account", first.generation);

    assert!(cache.generation_is_current("account", replacement.generation));
}

#[tokio::test]
async fn client_fetches_summary_identity_and_legacy_usage() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let paths = Arc::new(Mutex::new(BTreeSet::new()));
    let observed = paths.clone();
    let server = tokio::spawn(async move {
        for _ in 0..3 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0_u8; 1024];
                let count = stream.read(&mut chunk).await.unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(request).unwrap();
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap()
                .to_string();
            observed.lock().unwrap().insert(path.clone());
            let body = if path == "/api/usage-summary" {
                r#"{"individualUsage":{"plan":{"used":25,"limit":100}}}"#
            } else if path == "/api/auth/me" {
                r#"{"sub":"user-1","email":"user@example.com"}"#
            } else {
                r#"{"gpt-4":{"numRequests":2,"maxRequestUsage":10}}"#
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let client = reqwest::Client::builder().build().unwrap();
    let cursor =
        CursorClient::with_base_url(client, Url::parse(&format!("http://{address}/")).unwrap());

    let result = cursor.fetch("session=test", None).await.unwrap();
    server.await.unwrap();

    assert!(result.identity.is_some());
    assert!(result.legacy.is_some());
    let paths = paths.lock().unwrap();
    assert!(paths.contains("/api/usage-summary"));
    assert!(paths.contains("/api/auth/me"));
    assert!(paths.contains("/api/usage?user=user-1"));
}

#[tokio::test]
async fn client_posts_and_verifies_usage_event_pages_with_origin() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let observed = requests.clone();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0_u8; 1024];
                let count = stream.read(&mut chunk).await.unwrap();
                request.extend_from_slice(&chunk[..count]);
                if count == 0 || request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            observed
                .lock()
                .unwrap()
                .push(String::from_utf8(request).unwrap());
            let body = r#"{"totalUsageEventsCount":1,"usageEventsDisplay":[{
                "timestamp":"1783684800000","model":"claude-sonnet","kind":"usage",
                "chargedCents":1
            }]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let client = reqwest::Client::builder().build().unwrap();
    let cursor =
        CursorClient::with_base_url(client, Url::parse(&format!("http://{address}/")).unwrap());

    let pages = cursor
        .fetch_usage_events(
            "session=test",
            Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 7, 20, 0, 0, 0).unwrap(),
        )
        .await
        .unwrap();
    server.await.unwrap();

    assert_eq!(pages.len(), 1);
    for request in requests.lock().unwrap().iter() {
        let lower = request.to_ascii_lowercase();
        assert!(request.starts_with("POST /api/dashboard/get-filtered-usage-events "));
        assert!(lower.contains(&format!("origin: http://{address}")));
        assert!(lower.contains("content-type: application/json"));
    }
}

fn credential(value: &str) -> SessionCredential {
    SessionCredential {
        cookie_header: format!("WorkosCursorSessionToken={value}"),
        source: SessionSource::Browser,
        account_hint: None,
    }
}

fn base64url(value: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut encoded = String::new();
    for chunk in value.chunks(3) {
        let a = chunk[0];
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        encoded.push(TABLE[(a >> 2) as usize] as char);
        encoded.push(TABLE[(((a & 0x03) << 4) | (b >> 4)) as usize] as char);
        if chunk.len() > 1 {
            encoded.push(TABLE[(((b & 0x0f) << 2) | (c >> 6)) as usize] as char);
        }
        if chunk.len() > 2 {
            encoded.push(TABLE[(c & 0x3f) as usize] as char);
        }
    }
    encoded
}
