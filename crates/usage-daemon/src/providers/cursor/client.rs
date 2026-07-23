use futures_util::{stream, StreamExt, TryStreamExt};
use reqwest::{StatusCode, Url};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::providers::{
    read_response_body, retry_after_deadline, ProviderError, ProviderErrorKind,
};

use super::{
    events::CursorUsageEventsPage,
    model::{CursorUsageResponse, CursorUsageSummary, CursorUserInfo},
};

const EVENT_PAGE_SIZE: u64 = 100;
const MAX_EVENT_PAGES: u64 = 500;
const MAX_EVENT_REQUESTS_IN_FLIGHT: usize = 4;

#[derive(Clone)]
pub(super) struct CursorClient {
    client: reqwest::Client,
    base_url: Url,
}

pub(super) struct CursorFetch {
    pub(super) summary: CursorUsageSummary,
    pub(super) identity: Option<CursorUserInfo>,
    pub(super) legacy: Option<CursorUsageResponse>,
    pub(super) event_pages: Option<Vec<CursorUsageEventsPage>>,
    pub(super) event_warning: Option<String>,
}

impl CursorClient {
    pub(super) fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            base_url: Url::parse("https://cursor.com/").expect("Cursor base URL is valid"),
        }
    }

    #[cfg(test)]
    pub(super) fn with_base_url(client: reqwest::Client, base_url: Url) -> Self {
        Self { client, base_url }
    }

    pub(super) async fn fetch_identity(
        &self,
        cookie_header: &str,
    ) -> Result<CursorUserInfo, ProviderError> {
        self.get_json("api/auth/me", cookie_header, "Cursor account identity")
            .await
    }

    pub(super) async fn fetch(
        &self,
        cookie_header: &str,
        account_id_fallback: Option<&str>,
    ) -> Result<CursorFetch, ProviderError> {
        let summary = self.get_json::<CursorUsageSummary>(
            "api/usage-summary",
            cookie_header,
            "Cursor usage summary",
        );
        let identity = self.fetch_identity(cookie_header);
        let (summary, identity) = tokio::join!(summary, identity);
        let summary = summary?;
        let identity = identity.ok();
        let account_id = identity
            .as_ref()
            .and_then(CursorUserInfo::stable_id)
            .or(account_id_fallback);
        let legacy = match account_id {
            Some(account_id) => self
                .get_json_with_query(
                    "api/usage",
                    &[("user", account_id)],
                    cookie_header,
                    "Cursor legacy usage",
                )
                .await
                .ok(),
            None => None,
        };
        let (event_pages, event_warning) = match summary.billing_period() {
            Some((period_start, period_end)) => {
                match self
                    .fetch_usage_events(cookie_header, period_start, period_end)
                    .await
                {
                    Ok(pages) => (Some(pages), None),
                    Err(error) if error.kind() == ProviderErrorKind::Unauthorized => {
                        return Err(error)
                    }
                    Err(error) => (
                        None,
                        Some(format!(
                            "Cursor usage event history was unavailable: {}",
                            error.short_message()
                        )),
                    ),
                }
            }
            None => (
                None,
                Some(
                    "Cursor usage event history was unavailable: billing cycle was missing"
                        .to_string(),
                ),
            ),
        };
        Ok(CursorFetch {
            summary,
            identity,
            legacy,
            event_pages,
            event_warning,
        })
    }

    pub(super) async fn fetch_usage_events(
        &self,
        cookie_header: &str,
        period_start: chrono::DateTime<chrono::Utc>,
        period_end: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<CursorUsageEventsPage>, ProviderError> {
        let request = |page| CursorEventRequest {
            start_date: period_start.timestamp_millis().to_string(),
            end_date: period_end.timestamp_millis().to_string(),
            page,
            page_size: EVENT_PAGE_SIZE,
        };
        let first = self
            .post_json::<CursorUsageEventsPage>(
                "api/dashboard/get-filtered-usage-events",
                cookie_header,
                "Cursor usage event history",
                &request(1),
            )
            .await?;
        let total = first.total_usage_events_count;
        let page_count = total.div_ceil(EVENT_PAGE_SIZE);
        if page_count > MAX_EVENT_PAGES {
            return Err(ProviderError::new(
                ProviderErrorKind::Parse,
                format!(
                    "Cursor usage event history exceeded the {}-event collection limit",
                    MAX_EVENT_PAGES * EVENT_PAGE_SIZE
                ),
            ));
        }

        let mut pages = vec![first.clone()];
        let remaining = stream::iter(2..=page_count)
            .map(|page| {
                let client = self.clone();
                let request = request(page);
                async move {
                    client
                        .post_json::<CursorUsageEventsPage>(
                            "api/dashboard/get-filtered-usage-events",
                            cookie_header,
                            "Cursor usage event history",
                            &request,
                        )
                        .await
                }
            })
            .buffer_unordered(MAX_EVENT_REQUESTS_IN_FLIGHT)
            .try_collect::<Vec<_>>()
            .await?;
        pages.extend(remaining);

        let verified = self
            .post_json::<CursorUsageEventsPage>(
                "api/dashboard/get-filtered-usage-events",
                cookie_header,
                "Cursor usage event history",
                &request(1),
            )
            .await?;
        if serde_json::to_value(&first).ok() != serde_json::to_value(verified).ok() {
            return Err(ProviderError::new(
                ProviderErrorKind::Parse,
                "Cursor usage event pagination changed during collection",
            ));
        }
        Ok(pages)
    }

    async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        cookie_header: &str,
        label: &str,
    ) -> Result<T, ProviderError> {
        self.get_json_with_query(path, &[], cookie_header, label)
            .await
    }

    async fn get_json_with_query<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
        cookie_header: &str,
        label: &str,
    ) -> Result<T, ProviderError> {
        let mut url = self.base_url.join(path).map_err(|err| {
            ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                format!("failed to construct {label} URL: {err}"),
            )
        })?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query.iter().copied());
        }
        let response = self
            .client
            .get(url)
            .header("Accept", "application/json")
            .header("Cookie", cookie_header)
            .send()
            .await
            .map_err(|err| {
                ProviderError::new(
                    ProviderErrorKind::Network,
                    format!("{label} request failed: {err}"),
                )
            })?;
        response_json(response, label).await
    }

    async fn post_json<T: DeserializeOwned>(
        &self,
        path: &str,
        cookie_header: &str,
        label: &str,
        body: &impl Serialize,
    ) -> Result<T, ProviderError> {
        let url = self.base_url.join(path).map_err(|err| {
            ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                format!("failed to construct {label} URL: {err}"),
            )
        })?;
        let origin = self.base_url.origin().ascii_serialization();
        let response = self
            .client
            .post(url)
            .header("Accept", "application/json")
            .header("Cookie", cookie_header)
            .header("Origin", origin)
            .json(body)
            .send()
            .await
            .map_err(|err| {
                ProviderError::new(
                    ProviderErrorKind::Network,
                    format!("{label} request failed: {err}"),
                )
            })?;
        response_json(response, label).await
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CursorEventRequest {
    start_date: String,
    end_date: String,
    page: u64,
    page_size: u64,
}

async fn response_json<T: DeserializeOwned>(
    response: reqwest::Response,
    label: &str,
) -> Result<T, ProviderError> {
    let status = response.status();
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return Err(ProviderError::new(
            ProviderErrorKind::Unauthorized,
            format!("{label} rejected the current Cursor session"),
        ));
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Err(ProviderError::new(
            ProviderErrorKind::RateLimited,
            format!("{label} was rate limited"),
        )
        .with_retry_at(retry_after_deadline(response.headers())));
    }
    if !status.is_success() {
        return Err(ProviderError::new(
            ProviderErrorKind::ProviderUnavailable,
            format!("{label} returned HTTP {}", status.as_u16()),
        ));
    }
    let body = read_response_body(response, label).await?;
    serde_json::from_slice(&body).map_err(|err| {
        ProviderError::new(
            ProviderErrorKind::Parse,
            format!("{label} response shape was invalid: {err}"),
        )
    })
}
