use std::{
    collections::{BTreeMap, BTreeSet},
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
};

use aes::Aes128;
use async_trait::async_trait;
use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use chrono::{DateTime, Datelike, Local, TimeDelta, TimeZone, Timelike, Utc};
use keyring::Entry;
use pbkdf2::pbkdf2_hmac;
use regex::Regex;
use reqwest::{redirect::Policy, StatusCode, Url};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Map, Value};
use sha1::Sha1;
use usage_core::{ProviderId, UsageAmount, UsageUnit, UsageWindow, UsageWindowKind};
use uuid::Uuid;

use crate::{
    config::ProviderConfig,
    providers::{
        DiscoveredAccount, ProviderCollectionResult, ProviderCollector, ProviderError,
        ProviderErrorKind, ProviderUsage, HTTP_CONNECT_TIMEOUT, HTTP_REQUEST_TIMEOUT,
    },
};

pub const OPENCODE_GO_PROVIDER_ID: &str = "opencode_go";

const OPENCODE_HOST: &str = "opencode.ai";
const WORKSPACES_SERVER_ID: &str =
    "def39973159c7f0483d8793a822b8dbb10d067e12c65455fcb4608459ba0234f";
const ZEN_BALANCE_SERVER_ID: &str =
    "c83b78a614689c38ebee981f9b39a8b377716db85c1fd7dbab604adc02d3313d";
const USAGE_HISTORY_SERVER_ID: &str =
    "bfd684bfc2e4eed05cd0b518f5e4eafd3f3376e3938abb9e536e7c03df831e5c";
const MAX_PERCENT: f64 = 100.0;
const COST_LOOKBACK_DAYS: i64 = 30;
const USAGE_HISTORY_PAGE_SIZE: usize = 50;
const MAX_USAGE_HISTORY_PAGES: u32 = 20;
const COOKIE_CACHE_SERVICE: &str = "usagetracker.opencode.cookies";
const COOKIE_NAMES: [&str; 2] = ["auth", "__Host-auth"];

#[derive(Clone)]
pub struct OpenCodeCollector {
    provider_id: &'static str,
    config: ProviderConfig,
    client: reqwest::Client,
    capture_raw_payloads: bool,
}

impl OpenCodeCollector {
    pub fn new(config: ProviderConfig, capture_raw_payloads: bool) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(HTTP_CONNECT_TIMEOUT)
            .timeout(HTTP_REQUEST_TIMEOUT)
            .user_agent("Mozilla/5.0 AppleWebKit/537.36 Chrome/143.0.0.0 Safari/537.36")
            .redirect(Policy::custom(|attempt| {
                let previous = attempt.previous().last();
                let same_https_host = previous
                    .and_then(|url| url.host_str())
                    .zip(attempt.url().host_str())
                    .is_some_and(|(source, destination)| {
                        source.eq_ignore_ascii_case(destination)
                            && attempt.url().scheme().eq_ignore_ascii_case("https")
                    });
                if same_https_host {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
            .build()?;
        Ok(Self {
            provider_id: OPENCODE_GO_PROVIDER_ID,
            config,
            client,
            capture_raw_payloads,
        })
    }

    async fn collect_web_usage(
        &self,
        workspace_hint: Option<&str>,
        allow_cached_cookie: bool,
    ) -> Result<ProviderCollectionResult, ProviderError> {
        let cookie_header = self.resolve_cookie_header(allow_cached_cookie)?;
        let workspace_id = match workspace_hint
            .map(str::trim)
            .filter(|value| value.starts_with("wrk_"))
        {
            Some(workspace_id) => workspace_id.to_string(),
            None => self.resolve_workspace_id(&cookie_header.value).await?,
        };

        self.collect_go_web_usage(
            cookie_header.value,
            cookie_header.source.as_str(),
            workspace_id,
        )
        .await
    }

    async fn collect_go_web_usage(
        &self,
        cookie_header: String,
        cookie_source: &'static str,
        workspace_id: String,
    ) -> Result<ProviderCollectionResult, ProviderError> {
        let usage_url = format!("https://{OPENCODE_HOST}/workspace/{workspace_id}/go");
        let dashboard_url = format!("https://{OPENCODE_HOST}/workspace/{workspace_id}");
        let usage_request = self
            .client
            .get(&usage_url)
            .header("Cookie", &cookie_header)
            .header(
                "Accept",
                "text/html, text/javascript, application/json;q=0.9, */*;q=0.8",
            )
            .header(
                "Referer",
                format!("https://{OPENCODE_HOST}/workspace/{workspace_id}"),
            )
            .send();
        let zen_request = async {
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                self.fetch_zen_balance(&cookie_header, &workspace_id, &dashboard_url),
            )
            .await
            .ok()
            .and_then(Result::ok)
        };
        let history_request = async {
            tokio::time::timeout(
                std::time::Duration::from_secs(20),
                self.fetch_usage_history(&cookie_header, &workspace_id),
            )
            .await
            .ok()
            .and_then(Result::ok)
        };

        let (usage_response, zen_balance, history_body) =
            tokio::join!(usage_request, zen_request, history_request);
        let response = usage_response.map_err(|err| {
            ProviderError::new(
                ProviderErrorKind::Network,
                format!("OpenCode Go usage page request failed: {err}"),
            )
        })?;
        let body = response_text(response, "OpenCode Go usage page").await?;
        let parsed = parse_usage_text(&body, true)?;
        let account_email = account_email_from_text(&body).or_else(|| {
            history_body
                .as_ref()
                .and_then(|history| history.account_email.clone())
        });
        let usage_history = history_body
            .as_ref()
            .and_then(|history| history.report.clone());
        let usage = parsed.to_provider_usage(
            self.provider_id,
            "opencode_go_web_console",
            zen_balance,
            Some(&workspace_id),
            account_email.as_deref(),
            cookie_source,
            usage_history,
        );

        Ok(ProviderCollectionResult {
            usage,
            daily_usage: Vec::new(),
            collection_mode: "opencode_go_web_console".to_string(),
            account_display_name: account_email.or_else(|| Some(workspace_id.clone())),
            raw_payload: self.capture_raw_payloads.then_some(json!({
                "workspace_id": workspace_id,
                "zen_balance_usd": zen_balance,
                "body": body,
                "usage_history_pages": history_body.map(|history| history.raw_pages),
            })),
            warnings: Vec::new(),
        })
    }

    async fn resolve_workspace_id(&self, cookie_header: &str) -> Result<String, ProviderError> {
        if let Some(workspace_id) = self
            .config
            .workspace_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Ok(workspace_id.to_string());
        }
        if let Ok(value) = std::env::var(provider_workspace_env()) {
            let value = value.trim();
            if !value.is_empty() {
                return Ok(value.to_string());
            }
        }

        let body = self
            .fetch_server_function(cookie_header, WORKSPACES_SERVER_ID, None)
            .await?;
        let ids = workspace_ids_from_text(&body);
        ids.into_iter().next().ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::Parse,
                "OpenCode workspace discovery returned no workspace ids",
            )
        })
    }

    async fn fetch_server_function(
        &self,
        cookie_header: &str,
        server_id: &str,
        workspace_id: Option<&str>,
    ) -> Result<String, ProviderError> {
        let mut url = format!("https://{OPENCODE_HOST}/_server?id={server_id}");
        if let Some(workspace_id) = workspace_id {
            url.push_str("&args=");
            url.push_str(&url_encode_json_arg(workspace_id));
        }

        let request = self
            .client
            .get(&url)
            .header("Cookie", cookie_header)
            .header("X-Server-Id", server_id)
            .header("X-Server-Instance", format!("server-fn:{}", Uuid::new_v4()))
            .header("Origin", format!("https://{OPENCODE_HOST}"))
            .header(
                "Accept",
                "text/javascript, application/json;q=0.9, */*;q=0.8",
            );
        let request = if let Some(workspace_id) = workspace_id {
            request.header(
                "Referer",
                format!("https://{OPENCODE_HOST}/workspace/{workspace_id}/billing"),
            )
        } else {
            request.header("Referer", format!("https://{OPENCODE_HOST}/"))
        };

        let response = request.send().await.map_err(|err| {
            ProviderError::new(
                ProviderErrorKind::Network,
                format!("OpenCode server function request failed: {err}"),
            )
        })?;
        response_text(response, "OpenCode server function").await
    }

    async fn fetch_usage_history(
        &self,
        cookie_header: &str,
        workspace_id: &str,
    ) -> Result<UsageHistoryCollection, ProviderError> {
        let mut rows = Vec::new();
        let mut raw_pages = Vec::new();
        let mut account_email = None;
        let lookback_start = usage_history_lookback_start(Local::now());
        let mut complete_lookback = false;

        for page in 0..MAX_USAGE_HISTORY_PAGES {
            let body = match self
                .fetch_usage_history_page(cookie_header, workspace_id, page)
                .await
            {
                Ok(body) => body,
                Err(_) if page == 0 => {
                    return self
                        .fetch_usage_history_html(cookie_header, workspace_id)
                        .await;
                }
                Err(_) => break,
            };
            if account_email.is_none() {
                account_email = account_email_from_text(&body);
            }
            let page_rows = parse_usage_history_rows(&body);
            let row_count = page_rows.len();
            if self.capture_raw_payloads {
                raw_pages.push(body);
            }
            if row_count == 0 {
                complete_lookback = true;
                break;
            }

            let page_is_before_lookback = page_rows
                .iter()
                .all(|row| row.created_at.with_timezone(&Local).date_naive() < lookback_start);
            rows.extend(page_rows);

            if page_is_before_lookback || row_count < USAGE_HISTORY_PAGE_SIZE {
                complete_lookback = true;
                break;
            }
        }

        if rows.is_empty() {
            return self
                .fetch_usage_history_html(cookie_header, workspace_id)
                .await;
        }

        let report = usage_history_report_from_rows(
            rows,
            "opencode_usage_page",
            !complete_lookback,
            complete_lookback,
        );
        Ok(UsageHistoryCollection {
            report,
            raw_pages,
            account_email,
        })
    }

    async fn fetch_usage_history_page(
        &self,
        cookie_header: &str,
        workspace_id: &str,
        page: u32,
    ) -> Result<String, ProviderError> {
        let mut url = Url::parse(&format!("https://{OPENCODE_HOST}/_server")).map_err(|err| {
            ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                format!("failed to build OpenCode usage history URL: {err}"),
            )
        })?;
        let args = serde_json::to_string(&json!([workspace_id, page])).map_err(|err| {
            ProviderError::new(
                ProviderErrorKind::Parse,
                format!("failed to encode OpenCode usage history args: {err}"),
            )
        })?;
        url.query_pairs_mut()
            .append_pair("id", USAGE_HISTORY_SERVER_ID)
            .append_pair("args", &args);

        let response = self
            .client
            .get(url)
            .header("Cookie", cookie_header)
            .header("X-Server-Id", USAGE_HISTORY_SERVER_ID)
            .header("X-Server-Instance", format!("server-fn:{}", Uuid::new_v4()))
            .header("Origin", format!("https://{OPENCODE_HOST}"))
            .header(
                "Accept",
                "text/javascript, application/json;q=0.9, text/plain;q=0.8, */*;q=0.7",
            )
            .header(
                "Referer",
                format!("https://{OPENCODE_HOST}/workspace/{workspace_id}/usage"),
            )
            .send()
            .await
            .map_err(|err| {
                ProviderError::new(
                    ProviderErrorKind::Network,
                    format!("OpenCode usage history page {page} request failed: {err}"),
                )
            })?;
        response_text(response, "OpenCode usage history").await
    }

    async fn fetch_usage_history_html(
        &self,
        cookie_header: &str,
        workspace_id: &str,
    ) -> Result<UsageHistoryCollection, ProviderError> {
        let history_url = format!("https://{OPENCODE_HOST}/workspace/{workspace_id}/usage");
        let response = self
            .client
            .get(&history_url)
            .header("Cookie", cookie_header)
            .header("Accept", "text/html, application/json;q=0.9, */*;q=0.8")
            .header(
                "Referer",
                format!("https://{OPENCODE_HOST}/workspace/{workspace_id}/go"),
            )
            .send()
            .await
            .map_err(|err| {
                ProviderError::new(
                    ProviderErrorKind::Network,
                    format!("OpenCode usage history page request failed: {err}"),
                )
            })?;
        let body = response_text(response, "OpenCode usage history page").await?;
        let account_email = account_email_from_text(&body);
        let report = parse_usage_history_report(&body);
        let raw_pages = self
            .capture_raw_payloads
            .then_some(body)
            .into_iter()
            .collect();
        Ok(UsageHistoryCollection {
            report,
            raw_pages,
            account_email,
        })
    }

    async fn fetch_zen_balance(
        &self,
        cookie_header: &str,
        workspace_id: &str,
        dashboard_url: &str,
    ) -> Result<f64, ProviderError> {
        let dashboard = self
            .client
            .get(dashboard_url)
            .header("Cookie", cookie_header)
            .header("Accept", "text/html, application/json;q=0.9, */*;q=0.8")
            .send()
            .await
            .map_err(|err| {
                ProviderError::new(
                    ProviderErrorKind::Network,
                    format!("OpenCode Zen dashboard request failed: {err}"),
                )
            })?;
        let dashboard_body = response_text(dashboard, "OpenCode Zen dashboard").await?;
        if let Some(balance) = parse_zen_balance(&dashboard_body) {
            return Ok(balance);
        }

        let billing = self
            .fetch_server_function(cookie_header, ZEN_BALANCE_SERVER_ID, Some(workspace_id))
            .await?;
        parse_zen_balance(&billing).ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::Parse,
                "OpenCode Zen balance was not found in dashboard or billing response",
            )
        })
    }

    fn resolve_cookie_header(
        &self,
        allow_cached: bool,
    ) -> Result<ResolvedCookieHeader, ProviderError> {
        if let Some(value) = self.manual_cookie_header() {
            return Ok(ResolvedCookieHeader {
                value,
                source: CookieHeaderSource::Manual,
            });
        }

        if allow_cached {
            if let Some(value) = load_cached_cookie_header(self.provider_id) {
                return Ok(ResolvedCookieHeader {
                    value,
                    source: CookieHeaderSource::Cache,
                });
            }
        }

        let value = import_browser_cookie_header(self.provider_id)?;
        store_cached_cookie_header(self.provider_id, &value);
        Ok(ResolvedCookieHeader {
            value,
            source: CookieHeaderSource::Browser,
        })
    }

    fn manual_cookie_header(&self) -> Option<String> {
        std::env::var(provider_cookie_env())
            .ok()
            .or_else(|| std::env::var("USAGE_TRACKER_OPENCODE_COOKIE").ok())
            .or_else(|| self.config.cookie_header.clone())
            .or_else(|| read_cookie_file(self.provider_id))
            .and_then(|value| normalize_cookie_header(&value))
    }

    fn has_manual_cookie_header(&self) -> bool {
        self.manual_cookie_header().is_some()
    }

    pub(crate) async fn discover_workspace_options(&self) -> Result<Vec<String>, ProviderError> {
        let cookie_header = self.resolve_cookie_header(true)?;
        let body = self
            .fetch_server_function(&cookie_header.value, WORKSPACES_SERVER_ID, None)
            .await?;
        let mut ids = workspace_ids_from_text(&body);
        ids.sort();
        ids.dedup();
        if ids.is_empty() {
            return Err(ProviderError::new(
                ProviderErrorKind::Parse,
                "OpenCode workspace discovery returned no workspace ids",
            ));
        }
        Ok(ids)
    }
}

#[async_trait]
impl ProviderCollector for OpenCodeCollector {
    fn provider_id(&self) -> ProviderId {
        ProviderId::new(self.provider_id)
    }

    async fn discover_accounts(&self) -> Result<Vec<DiscoveredAccount>, ProviderError> {
        if let Some(workspace_id) = self
            .config
            .workspace_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Ok(vec![DiscoveredAccount {
                external_account_id: workspace_id.to_string(),
                display_name: Some(workspace_id.to_string()),
                profile_id: None,
            }]);
        }

        if let Ok(cookie_header) = self.resolve_cookie_header(true) {
            if let Ok(workspace_id) = self.resolve_workspace_id(&cookie_header.value).await {
                return Ok(vec![DiscoveredAccount {
                    external_account_id: workspace_id.clone(),
                    display_name: Some(workspace_id),
                    profile_id: None,
                }]);
            }
        }

        if local_go_auth_exists() {
            return Ok(vec![DiscoveredAccount {
                external_account_id: "opencode_go_local".to_string(),
                display_name: Some("OpenCode Go local".to_string()),
                profile_id: None,
            }]);
        }

        Err(ProviderError::new(
            ProviderErrorKind::CredentialsMissing,
            format!(
                "{} credentials are missing",
                provider_display_name(self.provider_id)
            ),
        ))
    }

    async fn collect_usage(
        &self,
        account: &DiscoveredAccount,
    ) -> Result<ProviderCollectionResult, ProviderError> {
        let web_result = self
            .collect_web_usage(Some(&account.external_account_id), true)
            .await;
        let web_result = match web_result {
            Err(error) if is_auth_error(&error) && !self.has_manual_cookie_header() => {
                clear_cached_cookie_header(self.provider_id);
                self.collect_web_usage(Some(&account.external_account_id), false)
                    .await
            }
            result => result,
        };

        match web_result {
            Ok(result) => Ok(result),
            Err(web_error) => {
                let local_result = tokio::task::spawn_blocking(collect_go_local_usage)
                    .await
                    .map_err(|err| {
                        ProviderError::new(
                            ProviderErrorKind::ProviderUnavailable,
                            format!("OpenCode Go local usage task failed: {err}"),
                        )
                    })
                    .and_then(|result| result);
                match local_result {
                    Ok(mut result) => {
                        result.warnings.push(format!(
                            "OpenCode Go web usage failed, used local estimate: {}",
                            web_error.short_message()
                        ));
                        Ok(result)
                    }
                    Err(_) => Err(web_error),
                }
            }
        }
    }
}

struct ResolvedCookieHeader {
    value: String,
    source: CookieHeaderSource,
}

#[derive(Clone, Copy)]
enum CookieHeaderSource {
    Manual,
    Cache,
    Browser,
}

impl CookieHeaderSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Cache => "keychain_cache",
            Self::Browser => "browser_import",
        }
    }
}

#[derive(Clone, Debug)]
struct ParsedUsage {
    rolling: ParsedWindow,
    weekly: Option<ParsedWindow>,
    monthly: Option<ParsedWindow>,
}

#[derive(Clone, Debug)]
struct ParsedWindow {
    percent_used: f64,
    reset_at: Option<DateTime<Utc>>,
}

impl ParsedUsage {
    fn to_provider_usage(
        &self,
        provider_id: &'static str,
        collection_mode: &str,
        zen_balance_usd: Option<f64>,
        workspace_id: Option<&str>,
        account_email: Option<&str>,
        cookie_source: &'static str,
        usage_history: Option<UsageHistoryReport>,
    ) -> ProviderUsage {
        let mut windows = vec![usage_percent_window(
            &format!("{provider_id}_session"),
            &format!("{} session", provider_display_name(provider_id)),
            UsageWindowKind::Session,
            &self.rolling,
        )];
        if let Some(weekly) = &self.weekly {
            windows.push(usage_percent_window(
                &format!("{provider_id}_weekly"),
                &format!("{} weekly", provider_display_name(provider_id)),
                UsageWindowKind::Weekly,
                weekly,
            ));
        }
        if let Some(monthly) = &self.monthly {
            windows.push(usage_percent_window(
                &format!("{provider_id}_monthly"),
                &format!("{} monthly", provider_display_name(provider_id)),
                UsageWindowKind::Monthly,
                monthly,
            ));
        }
        if let Some(balance) = zen_balance_usd {
            windows.push(zen_balance_window(balance));
        }
        if let Some(report) = &usage_history {
            windows.extend(usage_history_windows(provider_id, report, Utc::now()));
        }

        let mut metadata = json!({
            "collection_mode": collection_mode,
            "workspace_id": workspace_id,
            "email": account_email,
            "account_email": account_email,
            "zen_balance_usd": zen_balance_usd,
            "web_authoritative": true,
            "cookie_source": cookie_source,
        });
        if let Some(usage_history) = usage_history.as_ref() {
            if let Some(object) = metadata.as_object_mut() {
                object.insert(
                    format!("{provider_id}_cost"),
                    usage_history.metadata_value(),
                );
            }
        }

        ProviderUsage {
            provider_id: ProviderId::new(provider_id),
            collected_at: Utc::now(),
            windows,
            metadata,
        }
    }
}

fn parse_usage_text(text: &str, include_monthly: bool) -> Result<ParsedUsage, ProviderError> {
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        if let Some(parsed) = parse_usage_json(&value, include_monthly) {
            return Ok(parsed);
        }
    }

    if let Some(parsed) = parse_usage_regex(text, include_monthly) {
        return Ok(parsed);
    }

    Err(ProviderError::new(
        ProviderErrorKind::Parse,
        "OpenCode usage response did not contain recognizable usage windows",
    ))
}

fn parse_usage_json(value: &Value, include_monthly: bool) -> Option<ParsedUsage> {
    let rolling = find_usage_window_json(
        value,
        &["rollingUsage", "rolling", "rolling_usage", "sessionUsage"],
    )?;
    let weekly = find_usage_window_json(
        value,
        &["weeklyUsage", "weekly", "weekly_usage", "weeklyWindow"],
    );
    let monthly = include_monthly
        .then(|| {
            find_usage_window_json(
                value,
                &["monthlyUsage", "monthly", "monthly_usage", "monthlyWindow"],
            )
        })
        .flatten();
    Some(ParsedUsage {
        rolling,
        weekly,
        monthly,
    })
}

fn find_usage_window_json(value: &Value, names: &[&str]) -> Option<ParsedWindow> {
    match value {
        Value::Object(object) => {
            for name in names {
                if let Some(window) = object.get(*name).and_then(parse_usage_window_object) {
                    return Some(window);
                }
            }
            if let Some(window) = parse_usage_window_object(value) {
                return Some(window);
            }
            object
                .values()
                .find_map(|child| find_usage_window_json(child, names))
        }
        Value::Array(values) => values
            .iter()
            .find_map(|child| find_usage_window_json(child, names)),
        _ => None,
    }
}

fn parse_usage_window_object(value: &Value) -> Option<ParsedWindow> {
    let object = value.as_object()?;
    let percent = usage_percent_from_object(object)?;
    Some(ParsedWindow {
        percent_used: percent,
        reset_at: reset_at_from_object(object),
    })
}

fn usage_percent_from_object(object: &Map<String, Value>) -> Option<f64> {
    let keys = [
        "usagePercent",
        "usedPercent",
        "percentUsed",
        "percent",
        "usage_percent",
        "used_percent",
        "utilization",
        "utilizationPercent",
        "usage",
    ];
    for key in keys {
        if let Some(value) = object.get(key).and_then(number_from_json_value) {
            return Some(normalize_percent(value));
        }
    }

    let used = ["used", "consumed", "count", "usedTokens", "cost"]
        .iter()
        .find_map(|key| object.get(*key).and_then(number_from_json_value))?;
    let limit = ["limit", "total", "quota", "max", "cap", "tokenLimit"]
        .iter()
        .find_map(|key| object.get(*key).and_then(number_from_json_value))?;
    (limit > 0.0).then(|| normalize_percent((used / limit) * 100.0))
}

fn reset_at_from_object(object: &Map<String, Value>) -> Option<DateTime<Utc>> {
    for key in [
        "resetInSec",
        "resetInSeconds",
        "resetSeconds",
        "reset_sec",
        "reset_in_sec",
        "resetsInSec",
        "resetsInSeconds",
        "resetIn",
        "resetSec",
    ] {
        if let Some(seconds) = object.get(key).and_then(number_from_json_value) {
            return TimeDelta::try_seconds(seconds.round() as i64).map(|delta| Utc::now() + delta);
        }
    }
    for key in [
        "resetAt",
        "resetsAt",
        "reset_at",
        "resets_at",
        "nextReset",
        "next_reset",
        "renewAt",
        "renew_at",
    ] {
        if let Some(reset_at) = object.get(key).and_then(datetime_from_json_value) {
            return Some(reset_at);
        }
    }
    None
}

fn parse_usage_regex(text: &str, include_monthly: bool) -> Option<ParsedUsage> {
    if let Some(parsed) = parse_usage_card_text(text, include_monthly) {
        return Some(parsed);
    }

    let rolling = find_usage_window_text(
        text,
        &["rollingUsage", "rolling_usage", "rolling", "sessionUsage"],
    )?;
    let weekly = find_usage_window_text(
        text,
        &["weeklyUsage", "weekly_usage", "weekly", "weeklyWindow"],
    );
    let monthly = include_monthly
        .then(|| {
            find_usage_window_text(
                text,
                &["monthlyUsage", "monthly_usage", "monthly", "monthlyWindow"],
            )
        })
        .flatten();
    Some(ParsedUsage {
        rolling,
        weekly,
        monthly,
    })
}

fn find_usage_window_text(text: &str, names: &[&str]) -> Option<ParsedWindow> {
    for name in names {
        let Some(index) = text.find(name) else {
            continue;
        };
        let end = (index + 1500).min(text.len());
        let segment = &text[index..end];
        let percent = regex_number(
            segment,
            r#"(usagePercent|usedPercent|percentUsed|percent|usage_percent|used_percent|utilizationPercent|utilization|usage)\s*["':=]*\s*([0-9]+(?:\.[0-9]+)?)"#,
        )
        .map(normalize_percent)?;
        let reset_at = regex_number(
            segment,
            r#"(resetInSec|resetInSeconds|resetSeconds|reset_sec|reset_in_sec|resetsInSec|resetsInSeconds|resetIn|resetSec)\s*["':=]*\s*([0-9]+(?:\.[0-9]+)?)"#,
        )
        .and_then(|seconds| TimeDelta::try_seconds(seconds.round() as i64))
        .map(|delta| Utc::now() + delta);
        return Some(ParsedWindow {
            percent_used: percent,
            reset_at,
        });
    }
    None
}

fn parse_usage_card_text(text: &str, include_monthly: bool) -> Option<ParsedUsage> {
    let rolling = find_labeled_usage_card(text, &["Rolling Usage"])?;
    let weekly = find_labeled_usage_card(text, &["Weekly Usage"]);
    let monthly = include_monthly
        .then(|| find_labeled_usage_card(text, &["Monthly Usage"]))
        .flatten();
    Some(ParsedUsage {
        rolling,
        weekly,
        monthly,
    })
}

fn find_labeled_usage_card(text: &str, labels: &[&str]) -> Option<ParsedWindow> {
    for label in labels {
        let pattern = format!(
            r#"(?is){}"#,
            regex::escape(label).replace(r#"\ "#, r#"\s+"#)
        );
        let Some(match_) = Regex::new(&pattern).ok()?.find(text) else {
            continue;
        };
        let end = (match_.end() + 1500).min(text.len());
        let segment = &text[match_.end()..end];
        let percent = usage_card_percent(segment)?;
        return Some(ParsedWindow {
            percent_used: percent,
            reset_at: usage_card_reset_at(segment),
        });
    }
    None
}

fn usage_card_percent(segment: &str) -> Option<f64> {
    for pattern in [
        r#"(?is)data-slot=["']usage-value["'][^>]*>.*?([0-9]+(?:\.[0-9]+)?)\s*(?:<!--/-->)?\s*%"#,
        r#"(?is)style=["'][^"']*width\s*:\s*([0-9]+(?:\.[0-9]+)?)%"#,
        r#"(?is)([0-9]+(?:\.[0-9]+)?)\s*(?:<!--/-->)?\s*%"#,
    ] {
        if let Some(value) = regex_number(segment, pattern).map(normalize_percent) {
            return Some(value);
        }
    }
    None
}

fn usage_card_reset_at(segment: &str) -> Option<DateTime<Utc>> {
    let reset_html = Regex::new(r#"(?is)data-slot=["']reset-time["'][^>]*>(.*?)</span>"#)
        .ok()
        .and_then(|regex| {
            regex
                .captures(segment)
                .and_then(|captures| captures.get(1).map(|match_| match_.as_str().to_string()))
        })
        .unwrap_or_else(|| segment.to_string());
    reset_at_from_human_text(&html_text(&reset_html))
}

fn reset_at_from_human_text(text: &str) -> Option<DateTime<Utc>> {
    let mut seconds = 0_i64;
    let regex =
        Regex::new(r#"(?i)([0-9]+(?:\.[0-9]+)?)\s*(days?|d|hours?|h|minutes?|mins?|min|m)\b"#)
            .ok()?;
    for captures in regex.captures_iter(text) {
        let value = captures.get(1)?.as_str().parse::<f64>().ok()?;
        let unit = captures.get(2)?.as_str().to_ascii_lowercase();
        seconds += match unit.as_str() {
            "day" | "days" | "d" => (value * 86_400.0).round() as i64,
            "hour" | "hours" | "h" => (value * 3_600.0).round() as i64,
            "minute" | "minutes" | "min" | "mins" | "m" => (value * 60.0).round() as i64,
            _ => 0,
        };
    }
    (seconds > 0)
        .then(|| TimeDelta::try_seconds(seconds).map(|delta| Utc::now() + delta))
        .flatten()
}

fn html_text(text: &str) -> String {
    let without_comments = Regex::new(r#"(?is)<!--.*?-->"#)
        .ok()
        .map(|regex| regex.replace_all(text, " ").into_owned())
        .unwrap_or_else(|| text.to_string());
    let without_tags = Regex::new(r#"(?is)<[^>]+>"#)
        .ok()
        .map(|regex| regex.replace_all(&without_comments, " ").into_owned())
        .unwrap_or(without_comments);
    without_tags
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&#39;", "'")
        .replace("&quot;", "\"")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_zen_balance(text: &str) -> Option<f64> {
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        if let Some(balance) = find_zen_balance_json(&value) {
            return Some(balance);
        }
    }
    find_billing_balance_text(text).or_else(|| find_zen_balance_text(text))
}

fn find_zen_balance_json(value: &Value) -> Option<f64> {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                let normalized = key
                    .chars()
                    .filter(|ch| ch.is_ascii_alphanumeric())
                    .collect::<String>()
                    .to_ascii_lowercase();
                if matches!(
                    normalized.as_str(),
                    "zenbalance"
                        | "zencurrentbalance"
                        | "currentbalance"
                        | "currentbalanceusd"
                        | "balanceusd"
                        | "usdbalance"
                ) {
                    if let Some(balance) = number_from_json_value(value) {
                        return Some(balance);
                    }
                }
            }
            object.values().find_map(find_zen_balance_json)
        }
        Value::Array(values) => values.iter().find_map(find_zen_balance_json),
        _ => None,
    }
}

fn find_billing_balance_text(text: &str) -> Option<f64> {
    if !text.contains("customerID") && !text.contains("customerId") {
        return None;
    }
    let value = regex_number(
        text,
        r#""?balance"?\s*[:=]\s*(?:\$R\[\d+\]=)?(-?[0-9]+(?:\.[0-9]+)?)"#,
    )?;
    Some(value / 100_000_000.0)
}

fn find_zen_balance_text(text: &str) -> Option<f64> {
    let pattern = Regex::new(
        r#"(?is)(current\s*balance|zen\s*balance|balance|現在の残高|残高).{0,120}?\$?\s*([0-9]+(?:\.[0-9]{1,2})?)"#,
    )
    .ok()?;
    pattern
        .captures(text)
        .and_then(|captures| captures.get(2))
        .and_then(|value| value.as_str().parse::<f64>().ok())
}

fn account_email_from_text(text: &str) -> Option<String> {
    Regex::new(r#"(?i)[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}"#)
        .ok()?
        .find(text)
        .map(|match_| match_.as_str().to_string())
}

#[derive(Default)]
struct UsageHistoryCollection {
    report: Option<UsageHistoryReport>,
    raw_pages: Vec<String>,
    account_email: Option<String>,
}

#[derive(Clone, Debug)]
struct UsageHistoryRow {
    created_at: DateTime<Utc>,
    tokens: u64,
    cost_usd: f64,
}

#[derive(Clone, Default)]
struct UsageHistoryDay {
    tokens: u64,
    cost_usd: f64,
    rows: u64,
}

#[derive(Clone, Default)]
struct UsageHistoryReport {
    source: &'static str,
    estimate: bool,
    partial: bool,
    complete_lookback: bool,
    row_count: u64,
    total_tokens: u64,
    total_cost_usd: f64,
    latest_at: Option<DateTime<Utc>>,
    by_day: BTreeMap<String, UsageHistoryDay>,
}

impl UsageHistoryReport {
    fn metadata_value(&self) -> Value {
        json!({
            "source": self.source,
            "estimate": self.estimate,
            "partial": self.partial,
            "complete_lookback": self.complete_lookback,
            "row_count": self.row_count,
            "today_cost_usd": self.cost_on(local_date_key(Local::now())),
            "today_tokens": self.tokens_on(local_date_key(Local::now())),
            "lookback_days": COST_LOOKBACK_DAYS,
            "lookback_cost_usd": self.lookback_cost_usd(Local::now()),
            "lookback_tokens": self.lookback_tokens(Local::now()),
            "total_tokens": self.total_tokens,
            "total_cost_usd": self.total_cost_usd,
            "latest_usage_at": self.latest_at.map(|time| time.to_rfc3339()),
            "by_day": self.by_day
                .iter()
                .map(|(date, day)| json!({
                    "date": date,
                    "tokens": day.tokens,
                    "cost_usd": day.cost_usd,
                    "rows": day.rows,
                }))
                .collect::<Vec<_>>(),
        })
    }

    fn cost_on(&self, date_key: String) -> f64 {
        self.by_day
            .get(&date_key)
            .map(|day| day.cost_usd)
            .unwrap_or_default()
    }

    fn tokens_on(&self, date_key: String) -> u64 {
        self.by_day
            .get(&date_key)
            .map(|day| day.tokens)
            .unwrap_or_default()
    }

    fn lookback_cost_usd(&self, now: DateTime<Local>) -> f64 {
        self.by_day
            .iter()
            .filter(|(date, _)| date_in_lookback(date, now))
            .map(|(_, day)| day.cost_usd)
            .sum()
    }

    fn lookback_tokens(&self, now: DateTime<Local>) -> u64 {
        self.by_day
            .iter()
            .filter(|(date, _)| date_in_lookback(date, now))
            .map(|(_, day)| day.tokens)
            .sum()
    }
}

fn parse_usage_history_report(text: &str) -> Option<UsageHistoryReport> {
    if !text.contains("usage.list") {
        return None;
    }
    usage_history_report_from_rows(
        parse_usage_history_rows(text),
        "opencode_usage_page",
        true,
        false,
    )
}

fn parse_usage_history_rows(text: &str) -> Vec<UsageHistoryRow> {
    let row_regex = Regex::new(
        r#"(?is)timeCreated:\s*(?:\$R\[\d+\]\s*=\s*)?new Date\(["']([^"']+)["']\).*?inputTokens:\s*(null|[0-9]+).*?outputTokens:\s*(null|[0-9]+).*?reasoningTokens:\s*(null|[0-9]+).*?cacheReadTokens:\s*(null|[0-9]+).*?cost:\s*(null|[0-9]+)"#,
    );
    let Ok(row_regex) = row_regex else {
        return Vec::new();
    };
    row_regex
        .captures_iter(text)
        .filter_map(|captures| {
            let created_at = DateTime::parse_from_rfc3339(captures.get(1)?.as_str())
                .ok()?
                .with_timezone(&Utc);
            let input = optional_u64(captures.get(2)?.as_str());
            let output = optional_u64(captures.get(3)?.as_str());
            let reasoning = optional_u64(captures.get(4)?.as_str());
            let cache_read = optional_u64(captures.get(5)?.as_str());
            let cost_usd = optional_u64(captures.get(6)?.as_str()) as f64 / 100_000_000.0;
            let tokens = input
                .saturating_add(output)
                .saturating_add(reasoning)
                .saturating_add(cache_read);
            Some(UsageHistoryRow {
                created_at,
                tokens,
                cost_usd,
            })
        })
        .collect()
}

fn usage_history_report_from_rows(
    rows: Vec<UsageHistoryRow>,
    source: &'static str,
    partial: bool,
    complete_lookback: bool,
) -> Option<UsageHistoryReport> {
    if rows.is_empty() {
        return None;
    }
    let mut report = UsageHistoryReport {
        source,
        partial,
        complete_lookback,
        ..UsageHistoryReport::default()
    };

    for row in rows {
        let day = report
            .by_day
            .entry(local_date_key(row.created_at.with_timezone(&Local)))
            .or_default();
        day.tokens = day.tokens.saturating_add(row.tokens);
        day.cost_usd += row.cost_usd;
        day.rows = day.rows.saturating_add(1);
        report.total_tokens = report.total_tokens.saturating_add(row.tokens);
        report.total_cost_usd += row.cost_usd;
        report.row_count = report.row_count.saturating_add(1);
        report.latest_at = Some(
            report
                .latest_at
                .map_or(row.created_at, |current| current.max(row.created_at)),
        );
    }

    Some(report)
}

fn optional_u64(value: &str) -> u64 {
    if value.eq_ignore_ascii_case("null") {
        0
    } else {
        value.parse().unwrap_or(0)
    }
}

fn local_usage_history_report(
    rows: &[LocalUsageRow],
    now: DateTime<Utc>,
) -> Option<UsageHistoryReport> {
    if rows.is_empty() {
        return None;
    }
    let mut report = UsageHistoryReport {
        source: "opencode_local_sqlite",
        estimate: true,
        partial: false,
        complete_lookback: true,
        row_count: rows.len() as u64,
        ..UsageHistoryReport::default()
    };
    for row in rows {
        if row.created_at > now {
            continue;
        }
        let day = report
            .by_day
            .entry(local_date_key(row.created_at.with_timezone(&Local)))
            .or_default();
        day.cost_usd += row.cost;
        day.rows = day.rows.saturating_add(1);
        report.total_cost_usd += row.cost;
        report.latest_at = Some(
            report
                .latest_at
                .map_or(row.created_at, |current| current.max(row.created_at)),
        );
    }
    Some(report)
}

fn usage_history_windows(
    provider_id: &str,
    report: &UsageHistoryReport,
    now: DateTime<Utc>,
) -> Vec<UsageWindow> {
    let local_now = now.with_timezone(&Local);
    let today_cost = report.cost_on(local_date_key(local_now));
    let today_tokens = report.tokens_on(local_date_key(local_now));
    let lookback_cost = report.lookback_cost_usd(local_now);
    let lookback_tokens = report.lookback_tokens(local_now);
    let display_name = provider_display_name(provider_id);
    let mut windows = Vec::new();
    if today_cost > 0.0 {
        windows.push(spend_window(
            &format!("{provider_id}_spend_today"),
            &format!("{display_name} spend today"),
            today_cost,
        ));
    }
    if today_tokens > 0 {
        windows.push(token_usage_window(
            &format!("{provider_id}_tokens_today"),
            &format!("{display_name} tokens today"),
            today_tokens,
            UsageWindowKind::Daily,
        ));
    }
    if lookback_cost > 0.0 {
        windows.push(spend_window(
            &format!("{provider_id}_spend_30d"),
            &format!("{display_name} spend 30 days"),
            lookback_cost,
        ));
    }
    if lookback_tokens > 0 {
        windows.push(token_usage_window(
            &format!("{provider_id}_tokens_30d"),
            &format!("{display_name} tokens 30 days"),
            lookback_tokens,
            UsageWindowKind::Monthly,
        ));
    }
    windows
}

fn spend_window(window_id: &str, label: &str, value: f64) -> UsageWindow {
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

fn token_usage_window(
    window_id: &str,
    label: &str,
    value: u64,
    kind: UsageWindowKind,
) -> UsageWindow {
    UsageWindow {
        window_id: window_id.to_string(),
        label: label.to_string(),
        kind,
        used: Some(UsageAmount {
            value: value as f64,
            unit: UsageUnit::Tokens,
        }),
        limit: None,
        remaining: None,
        percent_used: None,
        percent_remaining: None,
        reset_at: None,
    }
}

fn local_date_key(time: DateTime<Local>) -> String {
    time.date_naive().to_string()
}

fn usage_history_lookback_start(now: DateTime<Local>) -> chrono::NaiveDate {
    now.date_naive() - TimeDelta::days(COST_LOOKBACK_DAYS - 1)
}

fn date_in_lookback(date: &str, now: DateTime<Local>) -> bool {
    let Ok(date) = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d") else {
        return false;
    };
    let today = now.date_naive();
    let start = usage_history_lookback_start(now);
    date >= start && date <= today
}

fn collect_go_local_usage() -> Result<ProviderCollectionResult, ProviderError> {
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
struct LocalUsageRow {
    created_at: DateTime<Utc>,
    cost: f64,
}

fn read_local_usage_rows(conn: &Connection) -> Result<Vec<LocalUsageRow>, ProviderError> {
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
            DateTime::from_timestamp_millis(created_ms)
                .map(|created_at| LocalUsageRow { created_at, cost })
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

fn usage_percent_window(
    window_id: &str,
    label: &str,
    kind: UsageWindowKind,
    parsed: &ParsedWindow,
) -> UsageWindow {
    UsageWindow {
        window_id: window_id.to_string(),
        label: label.to_string(),
        kind,
        used: None,
        limit: None,
        remaining: None,
        percent_used: Some(parsed.percent_used),
        percent_remaining: Some(MAX_PERCENT - parsed.percent_used),
        reset_at: parsed.reset_at,
    }
}

fn zen_balance_window(balance: f64) -> UsageWindow {
    UsageWindow {
        window_id: "opencode_go_zen_balance".to_string(),
        label: "OpenCode Go Zen balance".to_string(),
        kind: UsageWindowKind::Credits,
        used: None,
        limit: None,
        remaining: Some(UsageAmount {
            value: balance,
            unit: UsageUnit::Usd,
        }),
        percent_used: None,
        percent_remaining: None,
        reset_at: None,
    }
}

async fn response_text(response: reqwest::Response, label: &str) -> Result<String, ProviderError> {
    let status = response.status();
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(ProviderError::new(
            ProviderErrorKind::Unauthorized,
            format!("{label} rejected OpenCode credentials"),
        ));
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Err(ProviderError::new(
            ProviderErrorKind::RateLimited,
            format!("{label} was rate limited"),
        ));
    }
    if !status.is_success() {
        return Err(ProviderError::new(
            ProviderErrorKind::ProviderUnavailable,
            format!("{label} returned HTTP {}", status.as_u16()),
        ));
    }
    response.text().await.map_err(|err| {
        ProviderError::new(
            ProviderErrorKind::Parse,
            format!("{label} response body could not be read: {err}"),
        )
    })
}

fn local_go_auth_exists() -> bool {
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

fn read_cookie_file(provider_id: &str) -> Option<String> {
    let home = dirs::home_dir()?;
    let specific = home
        .join(".usagetracker")
        .join(format!("{}.cookie", provider_id));
    std::fs::read_to_string(&specific).ok()
}

fn load_cached_cookie_header(provider_id: &str) -> Option<String> {
    Entry::new(COOKIE_CACHE_SERVICE, provider_id)
        .ok()?
        .get_password()
        .ok()
        .and_then(|value| normalize_cookie_header(&value))
}

fn store_cached_cookie_header(provider_id: &str, cookie_header: &str) {
    if let Ok(entry) = Entry::new(COOKIE_CACHE_SERVICE, provider_id) {
        let _ = entry.set_password(cookie_header);
    }
}

fn clear_cached_cookie_header(provider_id: &str) {
    if let Ok(entry) = Entry::new(COOKIE_CACHE_SERVICE, provider_id) {
        let _ = entry.delete_credential();
    }
}

pub(crate) fn clear_cached_cookie_cache() {
    clear_cached_cookie_header(OPENCODE_GO_PROVIDER_ID);
}

fn import_browser_cookie_header(_provider_id: &str) -> Result<String, ProviderError> {
    let home = dirs::home_dir().ok_or_else(|| {
        ProviderError::new(
            ProviderErrorKind::ProviderUnavailable,
            "failed to resolve home directory for browser cookie import",
        )
    })?;
    let mut failures = Vec::new();
    for browser in browser_import_order() {
        let Ok(cookie_paths) = browser_cookie_paths(&home, browser) else {
            continue;
        };
        for cookie_path in cookie_paths {
            match import_cookie_db(&cookie_path, browser) {
                Ok(Some(header)) => return Ok(header),
                Ok(None) => {}
                Err(err) => failures.push(format!(
                    "{} at {}: {}",
                    browser.label,
                    cookie_path.display(),
                    err.short_message()
                )),
            }
        }
    }

    let detail = if failures.is_empty() {
        "no OpenCode auth cookies were found in supported browser stores".to_string()
    } else {
        format!(
            "no usable OpenCode auth cookies were found; import errors: {}",
            failures.join("; ")
        )
    };
    Err(ProviderError::new(
        ProviderErrorKind::CredentialsMissing,
        detail,
    ))
}

#[derive(Clone, Copy)]
struct BrowserCookieStore {
    label: &'static str,
    app_support_path: &'static str,
    keychain_service: &'static str,
    keychain_account: &'static str,
    kind: BrowserCookieStoreKind,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum BrowserCookieStoreKind {
    Chromium,
    Firefox,
}

fn browser_import_order() -> Vec<BrowserCookieStore> {
    let chrome = BrowserCookieStore {
        label: "Chrome",
        app_support_path: "Google/Chrome",
        keychain_service: "Chrome Safe Storage",
        keychain_account: "Chrome",
        kind: BrowserCookieStoreKind::Chromium,
    };
    let dia = BrowserCookieStore {
        label: "Dia",
        app_support_path: "Dia",
        keychain_service: "Dia Safe Storage",
        keychain_account: "Dia",
        kind: BrowserCookieStoreKind::Chromium,
    };
    let firefox = BrowserCookieStore {
        label: "Firefox",
        app_support_path: "Firefox",
        keychain_service: "",
        keychain_account: "",
        kind: BrowserCookieStoreKind::Firefox,
    };
    let common = vec![
        chrome,
        dia,
        firefox,
        BrowserCookieStore {
            label: "Brave",
            app_support_path: "BraveSoftware/Brave-Browser",
            keychain_service: "Brave Safe Storage",
            keychain_account: "Brave",
            kind: BrowserCookieStoreKind::Chromium,
        },
        BrowserCookieStore {
            label: "Edge",
            app_support_path: "Microsoft Edge",
            keychain_service: "Microsoft Edge Safe Storage",
            keychain_account: "Microsoft Edge",
            kind: BrowserCookieStoreKind::Chromium,
        },
        BrowserCookieStore {
            label: "Arc",
            app_support_path: "Arc/User Data",
            keychain_service: "Arc Safe Storage",
            keychain_account: "Arc",
            kind: BrowserCookieStoreKind::Chromium,
        },
        BrowserCookieStore {
            label: "Chromium",
            app_support_path: "Chromium",
            keychain_service: "Chromium Safe Storage",
            keychain_account: "Chromium",
            kind: BrowserCookieStoreKind::Chromium,
        },
        BrowserCookieStore {
            label: "Vivaldi",
            app_support_path: "Vivaldi",
            keychain_service: "Vivaldi Safe Storage",
            keychain_account: "Vivaldi",
            kind: BrowserCookieStoreKind::Chromium,
        },
    ];
    common
}

fn browser_cookie_paths(
    home: &Path,
    browser: BrowserCookieStore,
) -> Result<Vec<PathBuf>, ProviderError> {
    let root = home
        .join("Library")
        .join("Application Support")
        .join(browser.app_support_path);
    if browser.kind == BrowserCookieStoreKind::Firefox {
        let mut paths = Vec::new();
        let profile_root = root.join("Profiles");
        if let Ok(entries) = std::fs::read_dir(&profile_root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    paths.push(path.join("cookies.sqlite"));
                }
            }
        }
        paths.push(root.join("cookies.sqlite"));
        paths.sort();
        paths.dedup();
        return Ok(paths.into_iter().filter(|path| path.exists()).collect());
    }

    let mut paths = Vec::new();
    for profile in [
        "Default",
        "Profile 1",
        "Profile 2",
        "Profile 3",
        "Profile 4",
    ] {
        paths.push(root.join(profile).join("Network/Cookies"));
        paths.push(root.join(profile).join("Cookies"));
    }
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                paths.push(path.join("Network/Cookies"));
                paths.push(path.join("Cookies"));
            }
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths.into_iter().filter(|path| path.exists()).collect())
}

fn import_cookie_db(
    cookie_path: &Path,
    browser: BrowserCookieStore,
) -> Result<Option<String>, ProviderError> {
    let copy_path = std::env::temp_dir().join(format!(
        "usagetracker-{}-cookies-{}.sqlite",
        browser.label.replace(' ', "-").to_ascii_lowercase(),
        Uuid::new_v4()
    ));
    std::fs::copy(cookie_path, &copy_path).map_err(|err| {
        ProviderError::new(
            ProviderErrorKind::ProviderUnavailable,
            format!("failed to copy browser cookie database: {err}"),
        )
    })?;
    let result = import_cookie_db_copy(&copy_path, browser);
    let _ = std::fs::remove_file(copy_path);
    result
}

fn import_cookie_db_copy(
    cookie_path: &Path,
    browser: BrowserCookieStore,
) -> Result<Option<String>, ProviderError> {
    let conn = Connection::open(cookie_path).map_err(|err| {
        ProviderError::new(
            ProviderErrorKind::ProviderUnavailable,
            format!("failed to open browser cookie database copy: {err}"),
        )
    })?;
    if browser.kind == BrowserCookieStoreKind::Firefox {
        return import_firefox_cookie_db_copy(&conn);
    }

    let mut stmt = conn
        .prepare(
            r#"
            SELECT host_key, name, value, encrypted_value
            FROM cookies
            WHERE (host_key LIKE '%opencode.ai' OR host_key LIKE '%app.opencode.ai')
              AND name IN ('auth', '__Host-auth')
            ORDER BY expires_utc DESC, last_access_utc DESC, creation_utc DESC
            "#,
        )
        .map_err(browser_cookie_db_error)?;
    let rows = stmt
        .query_map([], |row| {
            Ok(BrowserCookieRow {
                host_key: row.get(0)?,
                name: row.get(1)?,
                value: row.get(2)?,
                encrypted_value: row.get(3)?,
            })
        })
        .map_err(browser_cookie_db_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(browser_cookie_db_error)?;

    let mut cookies = BTreeMap::new();
    for row in rows {
        if !COOKIE_NAMES.contains(&row.name.as_str()) {
            continue;
        }
        if cookies.contains_key(&row.name) {
            continue;
        }
        if let Some(value) = browser_cookie_value(&row, browser) {
            cookies.insert(row.name, value);
        }
    }

    if cookies.is_empty() {
        return Ok(None);
    }
    let header = COOKIE_NAMES
        .iter()
        .filter_map(|name| cookies.get(*name).map(|value| format!("{name}={value}")))
        .collect::<Vec<_>>()
        .join("; ");
    normalize_cookie_header(&header).map(Some).ok_or_else(|| {
        ProviderError::new(
            ProviderErrorKind::CredentialsInvalid,
            "browser cookie import produced an empty auth cookie header",
        )
    })
}

fn import_firefox_cookie_db_copy(conn: &Connection) -> Result<Option<String>, ProviderError> {
    let mut stmt = conn
        .prepare(
            r#"
            SELECT host, name, value
            FROM moz_cookies
            WHERE (host LIKE '%opencode.ai' OR host LIKE '%app.opencode.ai')
              AND name IN ('auth', '__Host-auth')
            ORDER BY expiry DESC, lastAccessed DESC, creationTime DESC
            "#,
        )
        .map_err(browser_cookie_db_error)?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(browser_cookie_db_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(browser_cookie_db_error)?;

    let mut cookies = BTreeMap::new();
    for (_host, name, value) in rows {
        if !COOKIE_NAMES.contains(&name.as_str()) || cookies.contains_key(&name) {
            continue;
        }
        let value = value.trim();
        if !value.is_empty() {
            cookies.insert(name, value.to_string());
        }
    }

    if cookies.is_empty() {
        return Ok(None);
    }
    let header = COOKIE_NAMES
        .iter()
        .filter_map(|name| cookies.get(*name).map(|value| format!("{name}={value}")))
        .collect::<Vec<_>>()
        .join("; ");
    Ok(normalize_cookie_header(&header))
}

struct BrowserCookieRow {
    host_key: String,
    name: String,
    value: String,
    encrypted_value: Vec<u8>,
}

fn browser_cookie_value(row: &BrowserCookieRow, browser: BrowserCookieStore) -> Option<String> {
    let value = row.value.trim();
    if !value.is_empty() {
        return Some(value.to_string());
    }
    decrypt_chromium_cookie(&row.encrypted_value, &row.host_key, browser)
}

fn decrypt_chromium_cookie(
    encrypted_value: &[u8],
    host_key: &str,
    browser: BrowserCookieStore,
) -> Option<String> {
    if encrypted_value.is_empty() {
        return None;
    }
    if !encrypted_value.starts_with(b"v10") && !encrypted_value.starts_with(b"v11") {
        return String::from_utf8(encrypted_value.to_vec()).ok();
    }

    let password = Entry::new(browser.keychain_service, browser.keychain_account)
        .ok()?
        .get_password()
        .ok()?;
    let mut key = [0_u8; 16];
    pbkdf2_hmac::<Sha1>(password.as_bytes(), b"saltysalt", 1003, &mut key);
    let iv = [b' '; 16];
    let ciphertext = &encrypted_value[3..];
    let mut buffer = ciphertext.to_vec();
    let plaintext = cbc::Decryptor::<Aes128>::new(&key.into(), &iv.into())
        .decrypt_padded_mut::<Pkcs7>(&mut buffer)
        .ok()?;

    if let Ok(value) = String::from_utf8(plaintext.to_vec()) {
        if is_plausible_cookie_value(&value) {
            return Some(value);
        }
    }

    if plaintext.len() > 32 {
        if let Ok(value) = String::from_utf8(plaintext[32..].to_vec()) {
            if is_plausible_cookie_value(&value) {
                return Some(value);
            }
        }
    }

    if let Ok(value) = String::from_utf8(plaintext.to_vec()) {
        return Some(value);
    }

    if plaintext.len() > 32 {
        if let Ok(value) = String::from_utf8(plaintext[32..].to_vec()) {
            return Some(value);
        }
    }

    let _ = host_key;
    None
}

fn is_plausible_cookie_value(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && !value
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
}

fn browser_cookie_db_error(err: rusqlite::Error) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::ProviderUnavailable,
        format!("browser cookie database query failed: {err}"),
    )
}

fn normalize_cookie_header(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let allowed = ["auth", "__Host-auth"];
    let filtered = raw
        .split(';')
        .filter_map(|part| {
            let part = part.trim();
            let (name, value) = part.split_once('=')?;
            allowed
                .iter()
                .any(|allowed_name| *allowed_name == name.trim())
                .then(|| format!("{}={}", name.trim(), value.trim()))
        })
        .collect::<Vec<_>>();
    if filtered.is_empty() {
        Some(raw.to_string())
    } else {
        Some(filtered.join("; "))
    }
}

fn is_auth_error(error: &ProviderError) -> bool {
    matches!(
        error.kind(),
        ProviderErrorKind::Unauthorized | ProviderErrorKind::CredentialsInvalid
    )
}

fn workspace_ids_from_text(text: &str) -> Vec<String> {
    let Ok(regex) = Regex::new(r#"wrk_[A-Za-z0-9_-]+"#) else {
        return Vec::new();
    };
    regex
        .find_iter(text)
        .map(|match_| match_.as_str().to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn regex_number(text: &str, pattern: &str) -> Option<f64> {
    let captures = Regex::new(pattern).ok()?.captures(text)?;
    captures
        .iter()
        .flatten()
        .last()
        .and_then(|value| value.as_str().parse::<f64>().ok())
}

fn number_from_json_value(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(value) => value.parse().ok(),
        _ => None,
    }
}

fn datetime_from_json_value(value: &Value) -> Option<DateTime<Utc>> {
    match value {
        Value::String(value) => DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|time| time.with_timezone(&Utc)),
        Value::Number(_) => {
            let number = number_from_json_value(value)?;
            if number > 1_000_000_000_000.0 {
                DateTime::from_timestamp_millis(number.round() as i64)
            } else {
                DateTime::from_timestamp(number.round() as i64, 0)
            }
        }
        _ => None,
    }
}

fn normalize_percent(value: f64) -> f64 {
    let percent = if (0.0..=1.0).contains(&value) {
        value * 100.0
    } else {
        value
    };
    percent.clamp(0.0, MAX_PERCENT)
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool, ProviderError> {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![table],
        |_| Ok(()),
    )
    .optional()
    .map(|value| value.is_some())
    .map_err(local_db_error)
}

fn local_db_error(err: rusqlite::Error) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::ProviderUnavailable,
        format!("OpenCode Go local database query failed: {err}"),
    )
}

fn utc_week_start(now: DateTime<Utc>) -> DateTime<Utc> {
    let days = now.weekday().num_days_from_monday() as i64;
    let date = now.date_naive() - TimeDelta::days(days);
    Utc.with_ymd_and_hms(date.year(), date.month(), date.day(), 0, 0, 0)
        .single()
        .unwrap_or(now)
}

fn monthly_window_start(anchor: DateTime<Utc>, now: DateTime<Utc>) -> DateTime<Utc> {
    let candidate = anchor_in_month(anchor, now.year(), now.month());
    if candidate <= now {
        candidate
    } else {
        let (year, month) = previous_month(now.year(), now.month());
        anchor_in_month(anchor, year, month)
    }
}

fn next_monthly_anchor(anchor: DateTime<Utc>, start: DateTime<Utc>) -> DateTime<Utc> {
    let (year, month) = next_month(start.year(), start.month());
    anchor_in_month(anchor, year, month)
}

fn anchor_in_month(anchor: DateTime<Utc>, year: i32, month: u32) -> DateTime<Utc> {
    let day = anchor.day().min(days_in_month(year, month));
    Utc.with_ymd_and_hms(
        year,
        month,
        day,
        anchor.hour(),
        anchor.minute(),
        anchor.second(),
    )
    .single()
    .unwrap_or(anchor)
}

fn previous_month(year: i32, month: u32) -> (i32, u32) {
    if month == 1 {
        (year - 1, 12)
    } else {
        (year, month - 1)
    }
}

fn next_month(year: i32, month: u32) -> (i32, u32) {
    if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    }
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let (next_year, next_month) = next_month(year, month);
    let next_first = Utc
        .with_ymd_and_hms(next_year, next_month, 1, 0, 0, 0)
        .single()
        .unwrap();
    (next_first - TimeDelta::days(1)).day()
}

fn provider_display_name(provider_id: &str) -> &'static str {
    match provider_id {
        OPENCODE_GO_PROVIDER_ID => "OpenCode Go",
        _ => "OpenCode",
    }
}

fn provider_cookie_env() -> &'static str {
    "USAGE_TRACKER_OPENCODE_GO_COOKIE"
}

fn provider_workspace_env() -> &'static str {
    "USAGE_TRACKER_OPENCODE_GO_WORKSPACE_ID"
}

fn url_encode_json_arg(workspace_id: &str) -> String {
    format!("%5B%22{}%22%5D", workspace_id.replace('"', ""))
}

#[allow(dead_code)]
fn stable_local_account_id(value: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    format!("local_{:x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn monthly_anchor_clamps_short_months() {
        let anchor = Utc.with_ymd_and_hms(2026, 1, 31, 14, 30, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 2, 15, 12, 0, 0).unwrap();
        let start = monthly_window_start(anchor, now);
        assert_eq!(start.day(), 31);
        assert_eq!(start.month(), 1);
        let next = next_monthly_anchor(anchor, start);
        assert_eq!(next.day(), 28);
        assert_eq!(next.month(), 2);
    }

    #[test]
    fn filters_auth_cookies_when_possible() {
        let header = normalize_cookie_header("foo=1; auth=a; __Host-auth=b; bar=2").unwrap();
        assert_eq!(header, "auth=a; __Host-auth=b");
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
        assert!(metadata["by_day"].as_array().unwrap().len() >= 1);
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
}
