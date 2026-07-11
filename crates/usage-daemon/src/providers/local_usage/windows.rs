use usage_core::{UsageAmount, UsageUnit, UsageWindow, UsageWindowKind};

/// Builds an observed USD total. It deliberately carries no entitlement,
/// percentage, or reset: those fields must only come from an authoritative
/// provider quota response.
pub(crate) fn cost_window(
    window_id: impl Into<String>,
    label: impl Into<String>,
    value: f64,
) -> UsageWindow {
    UsageWindow {
        window_id: window_id.into(),
        label: label.into(),
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

/// Builds an observed token total without implying a quota or reset cycle.
pub(crate) fn token_window(
    window_id: impl Into<String>,
    label: impl Into<String>,
    tokens: u64,
    kind: UsageWindowKind,
) -> UsageWindow {
    UsageWindow {
        window_id: window_id.into(),
        label: label.into(),
        kind,
        used: Some(UsageAmount {
            value: tokens as f64,
            unit: UsageUnit::Tokens,
        }),
        limit: None,
        remaining: None,
        percent_used: None,
        percent_remaining: None,
        reset_at: None,
    }
}

pub(crate) fn usage_kind_from_name(name: &str) -> UsageWindowKind {
    let normalized = name.to_ascii_lowercase();
    if normalized.contains("session") || normalized.contains("hour") {
        UsageWindowKind::Session
    } else if normalized.contains("daily") || normalized.contains("day") {
        UsageWindowKind::Daily
    } else if normalized.contains("weekly") || normalized.contains("week") {
        UsageWindowKind::Weekly
    } else if normalized.contains("monthly") || normalized.contains("month") {
        UsageWindowKind::Monthly
    } else {
        UsageWindowKind::Other(normalized)
    }
}

pub(crate) fn stable_window_fragment(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observed_windows_never_claim_a_quota() {
        for window in [
            cost_window("cost", "Cost", 3.5),
            token_window("tokens", "Tokens", 42, UsageWindowKind::Daily),
        ] {
            assert!(window.limit.is_none());
            assert!(window.remaining.is_none());
            assert!(window.percent_used.is_none());
            assert!(window.percent_remaining.is_none());
            assert!(window.reset_at.is_none());
        }
    }

    #[test]
    fn shared_window_identity_helpers_are_stable() {
        assert_eq!(
            stable_window_fragment("Current week (all models)"),
            "current_week__all_models_"
        );
        assert!(matches!(
            usage_kind_from_name("five hour"),
            UsageWindowKind::Session
        ));
        assert!(matches!(
            usage_kind_from_name("seven day"),
            UsageWindowKind::Daily
        ));
    }
}
