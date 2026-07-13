//! OpenCode Go web collection orchestration and local fallback selection.

use async_trait::async_trait;
use chrono::Local;
use reqwest::{redirect::Policy, Url};
use serde_json::json;
use usage_core::ProviderId;
use uuid::Uuid;

use crate::{
    config::ProviderConfig,
    providers::{
        AccountDiscovery, DiscoveredAccount, ProviderCollectionResult, ProviderCollector,
        ProviderError, ProviderErrorKind, HTTP_CONNECT_TIMEOUT, HTTP_REQUEST_TIMEOUT,
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
    config: ProviderConfig,
    client: reqwest::Client,
}

impl OpenCodeCollector {
    pub fn new(config: ProviderConfig) -> anyhow::Result<Self> {
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
        Ok(Self { config, client })
    }

    async fn collect_web_usage(
        &self,
        workspace_hint: Option<&str>,
        allow_cached_cookie: bool,
    ) -> Result<ProviderCollectionResult, ProviderError> {
        let cookie_header = self.resolve_cookie_header(allow_cached_cookie).await?;
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
        let usage = parsed.to_provider_usage(UsageContext {
            provider_id: OPENCODE_GO_PROVIDER_ID,
            collection_mode: "opencode_go_web_console",
            zen_balance_usd: zen_balance,
            workspace_id: Some(&workspace_id),
            account_email: account_email.as_deref(),
            cookie_source,
            history: usage_history,
        });

        Ok(ProviderCollectionResult {
            usage,
            daily_usage: Vec::new(),
            collection_mode: "opencode_go_web_console".to_string(),
            account_email,
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
        Ok(UsageHistoryCollection {
            report,
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

    async fn resolve_cookie_header(
        &self,
        allow_cached: bool,
    ) -> Result<ResolvedCookieHeader, ProviderError> {
        let configured_cookie_header = self.configured_cookie_header();
        tokio::task::spawn_blocking(move || {
            resolve_cookie_header_blocking(configured_cookie_header, allow_cached)
        })
        .await
        .map_err(|err| {
            ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                format!("OpenCode cookie resolution task failed: {err}"),
            )
        })?
    }

    fn configured_cookie_header(&self) -> Option<String> {
        std::env::var(provider_cookie_env())
            .ok()
            .or_else(|| std::env::var("USAGE_TRACKER_OPENCODE_COOKIE").ok())
            .or_else(|| self.config.cookie_header.clone())
    }

    async fn has_manual_cookie_header(&self) -> Result<bool, ProviderError> {
        let configured_cookie_header = self.configured_cookie_header();
        tokio::task::spawn_blocking(move || {
            configured_cookie_header
                .or_else(|| read_cookie_file(OPENCODE_GO_PROVIDER_ID))
                .and_then(|value| normalize_cookie_header(&value))
                .is_some()
        })
        .await
        .map_err(|err| {
            ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                format!("OpenCode manual cookie check failed: {err}"),
            )
        })
    }

    pub(crate) async fn discover_workspace_options(&self) -> Result<Vec<String>, ProviderError> {
        let cookie_header = self.resolve_cookie_header(true).await?;
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
        ProviderId::new(OPENCODE_GO_PROVIDER_ID)
    }

    async fn discover_accounts(&self) -> Result<AccountDiscovery, ProviderError> {
        if let Some(workspace_id) = self
            .config
            .workspace_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Ok(vec![DiscoveredAccount {
                external_account_id: workspace_id.to_string(),
                display_name: None,
                email: None,
                profile_id: None,
            }]
            .into());
        }

        if let Ok(cookie_header) = self.resolve_cookie_header(true).await {
            if let Ok(workspace_id) = self.resolve_workspace_id(&cookie_header.value).await {
                return Ok(vec![DiscoveredAccount {
                    external_account_id: workspace_id.clone(),
                    display_name: None,
                    email: None,
                    profile_id: None,
                }]
                .into());
            }
        }

        if local_go_auth_exists() {
            return Ok(vec![DiscoveredAccount {
                external_account_id: "opencode_go_local".to_string(),
                display_name: None,
                email: None,
                profile_id: None,
            }]
            .into());
        }

        Err(ProviderError::new(
            ProviderErrorKind::CredentialsMissing,
            format!(
                "{} credentials are missing",
                provider_display_name(OPENCODE_GO_PROVIDER_ID)
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
            Err(error) if is_auth_error(&error) => {
                if self.has_manual_cookie_header().await? {
                    Err(error)
                } else {
                    tokio::task::spawn_blocking(|| {
                        clear_cached_cookie_header(OPENCODE_GO_PROVIDER_ID)
                    })
                    .await
                    .map_err(|err| {
                        ProviderError::new(
                            ProviderErrorKind::ProviderUnavailable,
                            format!("OpenCode cookie cache clear task failed: {err}"),
                        )
                    })?;
                    self.collect_web_usage(Some(&account.external_account_id), false)
                        .await
                }
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

pub(crate) mod cookies;
mod history;
mod http;
mod local;
mod usage;
mod utils;

pub(crate) use cookies::clear_cached_cookie_cache;
use cookies::{
    clear_cached_cookie_header, import_browser_cookie_header, is_auth_error,
    load_cached_cookie_header, normalize_cookie_header, read_cookie_file,
    store_cached_cookie_header,
};
use history::{
    parse_usage_history_report, parse_usage_history_rows, usage_history_lookback_start,
    usage_history_report_from_rows, UsageHistoryCollection,
};
use http::response_text;
use local::{collect_go_local_usage, local_go_auth_exists};
use usage::{account_email_from_text, parse_usage_text, parse_zen_balance, UsageContext};
use utils::{
    provider_cookie_env, provider_display_name, provider_workspace_env, url_encode_json_arg,
    workspace_ids_from_text,
};

fn resolve_cookie_header_blocking(
    configured_cookie_header: Option<String>,
    allow_cached: bool,
) -> Result<ResolvedCookieHeader, ProviderError> {
    if let Some(value) = configured_cookie_header
        .or_else(|| read_cookie_file(OPENCODE_GO_PROVIDER_ID))
        .and_then(|value| normalize_cookie_header(&value))
    {
        return Ok(ResolvedCookieHeader {
            value,
            source: CookieHeaderSource::Manual,
        });
    }

    if allow_cached {
        if let Some(value) = load_cached_cookie_header(OPENCODE_GO_PROVIDER_ID) {
            return Ok(ResolvedCookieHeader {
                value,
                source: CookieHeaderSource::Cache,
            });
        }
    }

    let value = import_browser_cookie_header(OPENCODE_GO_PROVIDER_ID)?;
    store_cached_cookie_header(OPENCODE_GO_PROVIDER_ID, &value);
    Ok(ResolvedCookieHeader {
        value,
        source: CookieHeaderSource::Browser,
    })
}

#[cfg(test)]
mod tests;
