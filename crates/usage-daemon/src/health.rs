use chrono::Utc;
use usage_core::{AccountId, ProviderHealth, ProviderHealthStatus, ProviderId};

use crate::providers::{ProviderError, ProviderErrorKind};

pub fn disabled(provider_id: ProviderId) -> ProviderHealth {
    ProviderHealth {
        provider_id,
        account_id: None,
        status: ProviderHealthStatus::Disabled,
        collection_mode: None,
        last_success_at: None,
        last_failure_at: None,
        last_error_code: None,
        last_error_message: None,
        updated_at: Utc::now(),
    }
}

pub fn ok(
    provider_id: ProviderId,
    account_id: AccountId,
    collection_mode: String,
) -> ProviderHealth {
    let now = Utc::now();
    ProviderHealth {
        provider_id,
        account_id: Some(account_id),
        status: ProviderHealthStatus::Ok,
        collection_mode: Some(collection_mode),
        last_success_at: Some(now),
        last_failure_at: None,
        last_error_code: None,
        last_error_message: None,
        updated_at: now,
    }
}

pub fn from_provider_error(
    provider_id: ProviderId,
    account_id: Option<AccountId>,
    error: &ProviderError,
) -> ProviderHealth {
    let now = Utc::now();
    ProviderHealth {
        provider_id,
        account_id,
        status: status_for_kind(error.kind()),
        collection_mode: None,
        last_success_at: None,
        last_failure_at: Some(now),
        last_error_code: Some(error.kind().as_str().to_string()),
        last_error_message: Some(error.short_message().to_string()),
        updated_at: now,
    }
}

pub fn backing_off(
    provider_id: ProviderId,
    account_id: AccountId,
    last_failure_at: chrono::DateTime<Utc>,
    message: String,
) -> ProviderHealth {
    ProviderHealth {
        provider_id,
        account_id: Some(account_id),
        status: ProviderHealthStatus::BackingOff,
        collection_mode: None,
        last_success_at: None,
        last_failure_at: Some(last_failure_at),
        last_error_code: Some(ProviderErrorKind::RateLimited.as_str().to_string()),
        last_error_message: Some(message),
        updated_at: Utc::now(),
    }
}

fn status_for_kind(kind: ProviderErrorKind) -> ProviderHealthStatus {
    match kind {
        ProviderErrorKind::CredentialsMissing => ProviderHealthStatus::CredentialsMissing,
        ProviderErrorKind::CredentialsInvalid | ProviderErrorKind::Unauthorized => {
            ProviderHealthStatus::AuthFailed
        }
        ProviderErrorKind::KeychainAccessFailed => ProviderHealthStatus::KeychainAccessFailed,
        ProviderErrorKind::RateLimited => ProviderHealthStatus::RateLimited,
        ProviderErrorKind::Parse => ProviderHealthStatus::ParseError,
        ProviderErrorKind::Network | ProviderErrorKind::ProviderUnavailable => {
            ProviderHealthStatus::ProviderError
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keychain_access_is_not_reported_as_provider_authentication() {
        let error = ProviderError::new(
            ProviderErrorKind::KeychainAccessFailed,
            "macOS Keychain authentication failed after 3 attempts",
        );

        let health = from_provider_error(ProviderId::new("claude"), None, &error);

        assert!(matches!(
            health.status,
            ProviderHealthStatus::KeychainAccessFailed
        ));
        assert_eq!(
            health.last_error_code.as_deref(),
            Some("keychain_access_failed")
        );
    }
}
