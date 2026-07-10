//! HTTP response validation shared by OpenCode web endpoints.

use reqwest::StatusCode;

use crate::providers::{read_response_body, ProviderError, ProviderErrorKind};

pub(super) async fn response_text(
    response: reqwest::Response,
    label: &str,
) -> Result<String, ProviderError> {
    let status = response.status();
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
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
    let body = read_response_body(response, label).await?;
    String::from_utf8(body).map_err(|err| {
        ProviderError::new(
            ProviderErrorKind::Parse,
            format!("{label} response body was not UTF-8: {err}"),
        )
    })
}
