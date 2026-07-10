use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use chrono::{Days, Utc};
use serde_json::json;
use usage_core::{AccountId, ProviderId, UsageWindow, UsageWindowKind};

use crate::providers::{ProviderErrorKind, ProviderUsage};

use super::app_server::{normalize_account_token_usage, CodexAccountActivityExt};
use super::cost::{
    codex_cost_usd, codex_event_timestamp, codex_session_roots, codex_token_count_info,
    codex_token_delta, codex_totals_from_value, codex_turn_context_model, normalize_codex_model,
    scan_codex_session_file, CodexCostReport, CodexTokenTotals, CodexUsageCostExt,
    DailyCostSummary,
};
use super::rate_limits::{normalize_app_server_usage, normalize_usage};
use super::{codex_credentials_from_auth_json, PROVIDER_ID};

#[test]
fn adds_standard_codex_sessions_only_for_the_active_account() {
    let profile_home = Path::new("/profiles/personal");
    let local_home = Path::new("/home/.codex");

    let matching = codex_session_roots(
        profile_home,
        local_home,
        Some("personal-account"),
        "personal-account",
    );
    assert_eq!(matching.len(), 2);
    assert!(matching.contains(&profile_home.join("sessions")));
    assert!(matching.contains(&local_home.join("sessions")));

    let different = codex_session_roots(
        Path::new("/profiles/work"),
        local_home,
        Some("personal-account"),
        "work-account",
    );
    assert_eq!(different, vec![PathBuf::from("/profiles/work/sessions")]);
}

#[test]
fn does_not_duplicate_standard_codex_session_root() {
    let local_home = Path::new("/home/.codex");
    let roots = codex_session_roots(
        local_home,
        local_home,
        Some("personal-account"),
        "personal-account",
    );

    assert_eq!(roots, vec![local_home.join("sessions")]);
}

#[test]
fn account_activity_is_authoritative_and_local_cost_does_not_duplicate_tokens() {
    let today = Utc::now().date_naive();
    let yesterday = today.checked_sub_days(Days::new(1)).unwrap();
    let activity = normalize_account_token_usage(&json!({
        "summary": {
            "lifetimeTokens": 300,
            "peakDailyTokens": 200,
            "longestRunningTurnSec": 90,
            "currentStreakDays": 2,
            "longestStreakDays": 4
        },
        "dailyUsageBuckets": [
            {"startDate": yesterday.to_string(), "tokens": 200},
            {"startDate": today.to_string(), "tokens": 100}
        ]
    }))
    .unwrap();
    assert_eq!(activity.daily_usage.len(), 2);
    assert_eq!(activity.daily_usage[0].date, yesterday);
    assert_eq!(activity.daily_usage[1].tokens, 100);

    let mut usage = ProviderUsage {
        provider_id: ProviderId::new(PROVIDER_ID),
        collected_at: Utc::now(),
        windows: Vec::new(),
        metadata: json!({}),
    };
    usage.merge_account_activity(activity);

    let mut by_day = BTreeMap::new();
    by_day.insert(
        today,
        DailyCostSummary {
            cost_usd: 1.25,
            tokens: 100,
        },
    );
    usage.merge_cost_report(
        CodexCostReport {
            today_cost_usd: 1.25,
            today_tokens: 100,
            lookback_cost_usd: 1.25,
            lookback_tokens: 100,
            total_cost_usd: 1.25,
            total_tokens: 100,
            by_day,
            ..Default::default()
        },
        false,
    );

    assert_eq!(usage.metadata["codex_activity"]["lifetime_tokens"], 300);
    assert_eq!(usage.metadata["codex_activity"]["by_day"][1]["tokens"], 100);
    assert_eq!(usage.metadata["codex_cost"]["partial"], true);
    assert_eq!(
        usage
            .windows
            .iter()
            .filter(|window| window.window_id == "codex_tokens_today")
            .count(),
        1
    );
    assert!(usage
        .windows
        .iter()
        .any(|window| window.window_id == "codex_estimated_spend_today"));
}

#[test]
fn reads_codex_identity_from_id_token_claims() {
    let credentials = codex_credentials_from_auth_json(
            r#"{
                "tokens": {
                    "access_token": "access",
                    "account_id": "account-id",
                    "id_token": "header.eyJlbWFpbCI6InVzZXJAZXhhbXBsZS5jb20iLCJuYW1lIjoiRXhhbXBsZSBVc2VyIn0.signature"
                }
            }"#,
        )
        .unwrap();

    assert_eq!(credentials.access_token, "access");
    assert_eq!(credentials.account_id, "account-id");
    assert_eq!(
        credentials.account_display_name.as_deref(),
        Some("user@example.com")
    );
}

#[test]
fn falls_back_to_codex_name_when_id_token_email_is_blank() {
    let credentials = codex_credentials_from_auth_json(
            r#"{
                "tokens": {
                    "access_token": "access",
                    "account_id": "account-id",
                    "id_token": "header.eyJlbWFpbCI6IiAgICIsIm5hbWUiOiJFeGFtcGxlIFVzZXIifQ.signature"
                }
            }"#,
        )
        .unwrap();

    assert_eq!(
        credentials.account_display_name.as_deref(),
        Some("Example User")
    );
}

#[test]
fn ignores_invalid_codex_id_token_identity_claims() {
    let credentials = codex_credentials_from_auth_json(
        r#"{
                "tokens": {
                    "access_token": "access",
                    "account_id": "account-id",
                    "id_token": "not-a-jwt"
                }
            }"#,
    )
    .unwrap();

    assert_eq!(credentials.account_display_name, None);
}

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
    assert_eq!(snapshot.metadata["email"], "user@example.com");
    assert_eq!(snapshot.metadata["credits_has_credits"], false);
    assert_eq!(
        snapshot.metadata["rate_limit_reset_credits_available_count"],
        1.0
    );
}

#[test]
fn normalizes_app_server_rate_limits_with_reset_credit_expiry() {
    let payload = json!({
        "account_read": {
            "account": {
                "type": "chatgpt",
                "email": "user@example.com",
                "planType": "prolite"
            },
            "requiresOpenaiAuth": true
        },
        "rate_limits_read": {
            "rateLimits": {
                "limitId": "codex",
                "limitName": null,
                "primary": {
                    "usedPercent": 7,
                    "windowDurationMins": 300,
                    "resetsAt": 1783626874
                },
                "secondary": {
                    "usedPercent": 39,
                    "windowDurationMins": 10080,
                    "resetsAt": 1784040385
                },
                "credits": {
                    "hasCredits": false,
                    "unlimited": false,
                    "balance": "0"
                },
                "planType": "prolite",
                "rateLimitReachedType": null
            },
            "rateLimitsByLimitId": {
                "codex_bengalfox": {
                    "limitId": "codex_bengalfox",
                    "limitName": "GPT-5.3-Codex-Spark",
                    "primary": {
                        "usedPercent": 0,
                        "windowDurationMins": 300,
                        "resetsAt": 1783627252
                    },
                    "secondary": {
                        "usedPercent": 0,
                        "windowDurationMins": 10080,
                        "resetsAt": 1784214052
                    },
                    "credits": null,
                    "planType": "prolite",
                    "rateLimitReachedType": null
                },
                "codex": {
                    "limitId": "codex",
                    "limitName": null,
                    "primary": {
                        "usedPercent": 7,
                        "windowDurationMins": 300,
                        "resetsAt": 1783626874
                    },
                    "secondary": {
                        "usedPercent": 39,
                        "windowDurationMins": 10080,
                        "resetsAt": 1784040385
                    },
                    "credits": {
                        "hasCredits": false,
                        "unlimited": false,
                        "balance": "0"
                    },
                    "planType": "prolite",
                    "rateLimitReachedType": null
                }
            },
            "rateLimitResetCredits": {
                "availableCount": 4,
                "credits": [
                    {
                        "id": "RateLimitResetCredit_old",
                        "resetType": "codexRateLimits",
                        "status": "available",
                        "grantedAt": 1781230493,
                        "expiresAt": 1783822493,
                        "title": "Full reset (Weekly + 5 hr)",
                        "description": "Thanks for using Codex!"
                    },
                    {
                        "id": "RateLimitResetCredit_new",
                        "resetType": "codexRateLimits",
                        "status": "available",
                        "grantedAt": 1781743124,
                        "expiresAt": 1784335124,
                        "title": "Full reset (Weekly + 5 hr)",
                        "description": "Thanks for using Codex!"
                    }
                ]
            }
        }
    });

    let snapshot = normalize_app_server_usage(&payload, Some("Codex"))
        .unwrap()
        .into_snapshot(AccountId::new("acct"));
    assert_eq!(snapshot.windows.len(), 5);

    let session = find_window(&snapshot.windows, "codex_session");
    assert_eq!(session.percent_used, Some(7.0));
    assert_eq!(session.percent_remaining, Some(93.0));
    assert_eq!(session.reset_at.unwrap().timestamp(), 1783626874);

    let weekly = find_window(&snapshot.windows, "codex_weekly");
    assert_eq!(weekly.percent_used, Some(39.0));
    assert_eq!(weekly.reset_at.unwrap().timestamp(), 1784040385);

    let additional_session = find_window(&snapshot.windows, "codex_additional_0_session");
    assert_eq!(additional_session.label, "GPT-5.3-Codex-Spark session");

    let credits = find_window(&snapshot.windows, "codex_credits");
    assert_eq!(credits.remaining.as_ref().unwrap().value, 0.0);

    assert_eq!(
        snapshot.metadata["collection_mode"],
        "codex_app_server_rate_limits"
    );
    assert_eq!(
        snapshot.metadata["rate_limit_reset_credits_available_count"],
        4.0
    );
    assert_eq!(
        snapshot.metadata["rate_limit_reset_credits"]["next_expires_at"],
        1783822493.0
    );
    assert_eq!(
        snapshot.metadata["rate_limit_reset_credits"]["next_expires_at_iso"],
        "2026-07-12T02:14:53+00:00"
    );
    assert_eq!(
        snapshot.metadata["rate_limit_reset_credits"]["credits"][0]["expires_at"],
        1783822493.0
    );
    assert_eq!(
        snapshot.metadata["rate_limit_reset_credits"]["credits"][0]["expires_at_iso"],
        "2026-07-12T02:14:53+00:00"
    );
    assert_eq!(
        snapshot.metadata["rate_limit_reset_credits"]["credits"][0]["id"],
        "RateLimitResetCredit_old"
    );
    assert_eq!(snapshot.metadata["plan_type"], "prolite");
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
fn reads_nested_codex_event_timestamps() {
    let event = json!({
        "type": "token_count",
        "payload": {
            "timestamp": "2026-06-12T19:11:08.807Z",
            "info": null
        }
    });

    assert_eq!(
        codex_event_timestamp(&event).unwrap().timestamp(),
        1_781_291_468
    );
    assert!(codex_event_timestamp(&json!({"type": "token_count"})).is_none());
}

#[test]
fn seeds_total_only_baseline_before_emitting_deltas() {
    let mut previous = None;
    let first = json!({
        "total_token_usage": {
            "input_tokens": 10_000,
            "cached_input_tokens": 8_000,
            "output_tokens": 500
        }
    });
    let second = json!({
        "total_token_usage": {
            "input_tokens": 11_000,
            "cached_input_tokens": 8_500,
            "output_tokens": 550
        }
    });

    let (empty_delta, seeded) = codex_token_delta(&json!(null), &mut previous);
    assert!(!seeded);
    assert_eq!(empty_delta, CodexTokenTotals::default());
    assert_eq!(previous, None);

    let (first_delta, seeded) = codex_token_delta(&first, &mut previous);
    assert!(seeded);
    assert_eq!(first_delta, CodexTokenTotals::default());

    let (second_delta, seeded) = codex_token_delta(&second, &mut previous);
    assert!(!seeded);
    assert_eq!(
        second_delta,
        CodexTokenTotals {
            input: 1_000,
            cached: 500,
            output: 50,
        }
    );
}

#[test]
fn keeps_undated_codex_usage_out_of_today_and_lookback() {
    let path = std::env::temp_dir().join(format!(
        "usagetracker-codex-undated-{}.jsonl",
        uuid::Uuid::new_v4()
    ));
    let contents = [
        json!({"type": "turn_context", "payload": {"model": "gpt-5"}}),
        json!({
            "type": "token_count",
            "info": {
                "last_token_usage": {
                    "input_tokens": 100,
                    "cached_input_tokens": 50,
                    "output_tokens": 10
                },
                "total_token_usage": {
                    "input_tokens": 100,
                    "cached_input_tokens": 50,
                    "output_tokens": 10
                }
            }
        }),
    ]
    .into_iter()
    .map(|event| serde_json::to_string(&event).unwrap())
    .collect::<Vec<_>>()
    .join("\n");
    std::fs::write(&path, contents).unwrap();
    let today = Utc::now().date_naive();
    let mut report = CodexCostReport::default();

    scan_codex_session_file(&path, today, today, &mut report).unwrap();

    assert_eq!(report.total_tokens, 110);
    assert_eq!(report.undated_tokens, 110);
    assert_eq!(report.today_tokens, 0);
    assert_eq!(report.lookback_tokens, 0);
    assert!(report.by_day.is_empty());
    let _ = std::fs::remove_file(path);
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

#[test]
fn token_total_does_not_double_count_cached_input() {
    let totals = CodexTokenTotals {
        input: 1_000,
        cached: 800,
        output: 100,
    };

    assert_eq!(totals.total(), 1_100);
}

fn find_window<'a>(windows: &'a [UsageWindow], window_id: &str) -> &'a UsageWindow {
    windows
        .iter()
        .find(|window| window.window_id == window_id)
        .unwrap_or_else(|| panic!("missing window {window_id}"))
}
