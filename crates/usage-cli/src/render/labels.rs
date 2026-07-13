use std::collections::HashMap;

use serde_json::Value;
use usage_core::{Account, AccountDisplayNameSource, UsageSnapshot};

use crate::render::style::{format_provider_name, title_case};

#[derive(Clone, Debug, Default)]
pub(crate) struct IdentityLabels {
    pub identity: Option<String>,
    pub plan: Option<String>,
}

pub(crate) fn latest_snapshots_by_account(
    snapshots: &[UsageSnapshot],
) -> HashMap<String, &UsageSnapshot> {
    let mut latest = HashMap::new();
    for snapshot in snapshots {
        latest
            .entry(snapshot.account_id.as_str().to_string())
            .and_modify(|current: &mut &UsageSnapshot| {
                if snapshot.collected_at > current.collected_at {
                    *current = snapshot;
                }
            })
            .or_insert(snapshot);
    }
    latest
}

pub(crate) fn identity_labels(
    account: Option<&Account>,
    snapshot: Option<&UsageSnapshot>,
) -> IdentityLabels {
    let provider_id = snapshot
        .map(|snapshot| snapshot.provider_id.as_str())
        .or_else(|| account.map(|account| account.provider_id.as_str()));
    let metadata = snapshot.map(|snapshot| &snapshot.metadata);

    let account_label = account
        .and_then(|account| account.email.clone())
        .or_else(|| {
            account
                .filter(|account| account.display_name_source == AccountDisplayNameSource::User)
                .and_then(|account| account.display_name.clone())
        })
        .or_else(|| {
            metadata
                .and_then(|metadata| metadata_str(metadata, "email"))
                .or_else(|| metadata.and_then(|metadata| metadata_str(metadata, "account_email")))
                .or_else(|| {
                    metadata
                        .and_then(|metadata| metadata_str(metadata, "account_display_name"))
                        .filter(|value| is_account_label(provider_id, value))
                })
                .map(str::to_string)
        })
        .or_else(|| {
            account.and_then(|account| account_external_account_label(provider_id, account))
        });

    let profile = metadata
        .and_then(|metadata| metadata_str(metadata, "credential_profile"))
        .or_else(|| metadata.and_then(|metadata| metadata_str(metadata, "keychain_account")))
        .map(str::to_string)
        .or_else(|| {
            account.and_then(|account| match provider_id {
                Some("claude") if !account.external_account_id.trim().is_empty() => {
                    Some(account.external_account_id.clone())
                }
                _ => None,
            })
        });

    let plan = metadata
        .and_then(|metadata| metadata_str(metadata, "plan_type"))
        .or_else(|| metadata.and_then(|metadata| metadata_str(metadata, "subscription_type")))
        .map(plan_label);

    let identity = account_label.or_else(|| nonempty(profile));

    IdentityLabels {
        identity,
        plan: nonempty(plan),
    }
}

pub(crate) fn plan_label(plan: &str) -> String {
    match plan.trim().to_ascii_lowercase().as_str() {
        "prolite" => "Pro Lite".to_string(),
        "plus" => "Plus".to_string(),
        "pro" => "Pro".to_string(),
        "team" => "Team".to_string(),
        "max" => "Max".to_string(),
        _ => title_case(&plan.replace(['_', '-'], " ")),
    }
}

pub(crate) fn metadata_str<'a>(metadata: &'a Value, key: &str) -> Option<&'a str> {
    metadata
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn account_external_account_label(provider_id: Option<&str>, account: &Account) -> Option<String> {
    let value = account.external_account_id.trim();
    if value.is_empty() || looks_like_uuid(value) {
        return None;
    }
    match provider_id {
        Some("claude") => None,
        _ => Some(value.to_string()),
    }
}

fn is_account_label(provider_id: Option<&str>, value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() || looks_like_uuid(value) {
        return false;
    }
    if is_provider_placeholder(provider_id, value) {
        return false;
    }
    if display_name_plan(provider_id, value).is_some() {
        return false;
    }
    true
}

fn display_name_plan(provider_id: Option<&str>, value: &str) -> Option<String> {
    let provider_id = provider_id?;
    let provider_name = format_provider_name(provider_id);
    let value = value.trim();
    let lower = value.to_ascii_lowercase();
    let provider_lower = provider_name.to_ascii_lowercase();
    let suffix = lower.strip_prefix(&(provider_lower + " "))?;
    (!suffix.trim().is_empty()).then(|| plan_label(suffix))
}

fn is_provider_placeholder(provider_id: Option<&str>, value: &str) -> bool {
    let Some(provider_id) = provider_id else {
        return false;
    };
    value
        .trim()
        .eq_ignore_ascii_case(&format_provider_name(provider_id))
}

fn looks_like_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && [8, 13, 18, 23].iter().all(|index| bytes[*index] == b'-')
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| [8, 13, 18, 23].contains(&index) || byte.is_ascii_hexdigit())
}

fn nonempty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}
