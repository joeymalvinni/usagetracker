use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{json, Value};
use usage_core::{ProviderId, UsageAmount, UsageUnit, UsageWindow, UsageWindowKind};

use crate::providers::{ProviderError, ProviderErrorKind, ProviderUsage};

use super::PROVIDER_ID;

#[derive(Clone, Debug)]
pub(super) struct BillingData {
    pub(super) used_percent: f64,
    pub(super) period_start: Option<DateTime<Utc>>,
    pub(super) resets_at: Option<DateTime<Utc>>,
    pub(super) used_usd: Option<f64>,
    pub(super) limit_usd: Option<f64>,
    pub(super) on_demand_used_usd: Option<f64>,
    pub(super) on_demand_limit_usd: Option<f64>,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum BillingSource {
    CliRpc,
    GrokWeb,
}

impl BillingSource {
    pub(super) fn collection_mode(self) -> &'static str {
        match self {
            Self::CliRpc => "grok_cli_billing_rpc",
            Self::GrokWeb => "grok_web_billing_rpc",
        }
    }
}

pub(super) fn from_rpc(value: &Value) -> Result<BillingData, ProviderError> {
    let value = value.get("billing").unwrap_or(value);
    let limit_cents = money(value.get("monthlyLimit"));
    let usage = value.get("usage").unwrap_or(&Value::Null);
    let included_cents = money(usage.get("includedUsed")).or_else(|| money(usage.get("totalUsed")));
    let (Some(limit_cents), Some(included_cents)) = (limit_cents, included_cents) else {
        return Err(ProviderError::new(
            ProviderErrorKind::Parse,
            "Grok CLI billing response omitted included usage or its limit",
        ));
    };
    if !limit_cents.is_finite()
        || limit_cents <= 0.0
        || !included_cents.is_finite()
        || included_cents < 0.0
    {
        return Err(ProviderError::new(
            ProviderErrorKind::Parse,
            "Grok CLI billing response contained invalid monetary values",
        ));
    }
    let cycle = value.get("billingCycle").unwrap_or(&Value::Null);
    Ok(BillingData {
        used_percent: (included_cents / limit_cents * 100.0).clamp(0.0, 100.0),
        period_start: timestamp(cycle.get("billingPeriodStart")),
        resets_at: timestamp(cycle.get("billingPeriodEnd")),
        used_usd: Some(included_cents / 100.0),
        limit_usd: Some(limit_cents / 100.0),
        on_demand_used_usd: money(usage.get("onDemandUsed")).map(|value| value / 100.0),
        on_demand_limit_usd: money(value.get("onDemandCap")).map(|value| value / 100.0),
    })
}

pub(super) fn to_provider_usage(data: &BillingData, source: BillingSource) -> ProviderUsage {
    let collected_at = Utc::now();
    let detected_cycle = detect_cycle(data, collected_at);
    let kind = detected_cycle
        .map(Cycle::kind)
        .unwrap_or_else(|| match source {
            BillingSource::GrokWeb => UsageWindowKind::Weekly,
            BillingSource::CliRpc => UsageWindowKind::Monthly,
        });
    let cycle_name = detected_cycle.map(Cycle::label).unwrap_or("credits");
    let mut windows = vec![UsageWindow {
        window_id: "grok_included_usage".to_string(),
        label: format!("Grok {cycle_name}"),
        kind: kind.clone(),
        used: data.used_usd.map(usd),
        limit: data.limit_usd.map(usd),
        remaining: data
            .limit_usd
            .zip(data.used_usd)
            .map(|(limit, used)| usd((limit - used).max(0.0))),
        percent_used: Some(data.used_percent.clamp(0.0, 100.0)),
        percent_remaining: Some((100.0 - data.used_percent).clamp(0.0, 100.0)),
        reset_at: data.resets_at,
    }];
    if let Some(limit) = data.on_demand_limit_usd.filter(|value| *value > 0.0) {
        let used = data.on_demand_used_usd.unwrap_or_default().max(0.0);
        let percent = (used / limit * 100.0).clamp(0.0, 100.0);
        windows.push(UsageWindow {
            window_id: "grok_on_demand".to_string(),
            label: "Grok on-demand cap".to_string(),
            kind,
            used: Some(usd(used)),
            limit: Some(usd(limit)),
            remaining: Some(usd((limit - used).max(0.0))),
            percent_used: Some(percent),
            percent_remaining: Some(100.0 - percent),
            reset_at: data.resets_at,
        });
    }
    ProviderUsage {
        provider_id: ProviderId::new(PROVIDER_ID),
        collected_at,
        windows,
        metadata: json!({
            "source": source.collection_mode(),
            "web_authoritative": true,
        }),
    }
}

#[derive(Clone, Copy)]
enum Cycle {
    Weekly,
    Monthly,
}

impl Cycle {
    fn kind(self) -> UsageWindowKind {
        match self {
            Self::Weekly => UsageWindowKind::Weekly,
            Self::Monthly => UsageWindowKind::Monthly,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
        }
    }
}

fn detect_cycle(data: &BillingData, now: DateTime<Utc>) -> Option<Cycle> {
    if let (Some(start), Some(end)) = (data.period_start, data.resets_at) {
        let days = (end - start).num_hours() as f64 / 24.0;
        if (4.0..=12.0).contains(&days) {
            return Some(Cycle::Weekly);
        }
        if (20.0..=45.0).contains(&days) {
            return Some(Cycle::Monthly);
        }
    }
    if let Some(until_reset) = data.resets_at.map(|end| end - now) {
        if until_reset >= TimeDelta::days(4) && until_reset <= TimeDelta::days(12) {
            return Some(Cycle::Weekly);
        }
        if until_reset >= TimeDelta::days(20) && until_reset <= TimeDelta::days(45) {
            return Some(Cycle::Monthly);
        }
    }
    None
}

fn money(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    let value = value.get("val").unwrap_or(value);
    value.as_f64().or_else(|| value.as_str()?.parse().ok())
}

fn timestamp(value: Option<&Value>) -> Option<DateTime<Utc>> {
    let value = value?.as_str()?;
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

fn usd(value: f64) -> UsageAmount {
    UsageAmount {
        value,
        unit: UsageUnit::Usd,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_included_and_on_demand_cli_billing() {
        let value = json!({
            "billingCycle": {"billingPeriodStart":"2026-07-06T00:00:00Z","billingPeriodEnd":"2026-07-13T00:00:00Z"},
            "monthlyLimit":{"val":10000}, "onDemandCap":{"val":5000},
            "usage":{"includedUsed":{"val":2500},"onDemandUsed":{"val":1000},"totalUsed":{"val":3500}}
        });
        let data = from_rpc(&value).unwrap();
        assert_eq!(data.used_percent, 25.0);
        let usage = to_provider_usage(&data, BillingSource::CliRpc);
        assert_eq!(usage.windows.len(), 2);
        assert!(matches!(usage.windows[0].kind, UsageWindowKind::Weekly));
        assert_eq!(usage.windows[0].used.as_ref().unwrap().value, 25.0);
        assert_eq!(usage.windows[1].percent_used, Some(20.0));
    }

    #[test]
    fn clamps_overage_and_rejects_missing_or_invalid_limits() {
        let overage = json!({
            "monthlyLimit": 100,
            "usage": {"totalUsed": 125}
        });
        assert_eq!(from_rpc(&overage).unwrap().used_percent, 100.0);

        assert_eq!(
            from_rpc(&json!({"usage":{"totalUsed":25}}))
                .unwrap_err()
                .kind(),
            ProviderErrorKind::Parse
        );
        assert_eq!(
            from_rpc(&json!({"monthlyLimit":0,"usage":{"totalUsed":25}}))
                .unwrap_err()
                .kind(),
            ProviderErrorKind::Parse
        );
    }

    #[test]
    fn derives_dynamic_cycle_kind_from_reset_distance() {
        let now = DateTime::parse_from_rfc3339("2026-07-11T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut data = BillingData {
            used_percent: 10.0,
            period_start: None,
            resets_at: Some(now + TimeDelta::days(7)),
            used_usd: None,
            limit_usd: None,
            on_demand_used_usd: None,
            on_demand_limit_usd: None,
        };
        assert!(matches!(detect_cycle(&data, now), Some(Cycle::Weekly)));
        data.resets_at = Some(now + TimeDelta::days(30));
        assert!(matches!(detect_cycle(&data, now), Some(Cycle::Monthly)));
    }

    #[test]
    fn uses_credits_label_when_cycle_timing_is_ambiguous() {
        let data = BillingData {
            used_percent: 10.0,
            period_start: None,
            resets_at: None,
            used_usd: None,
            limit_usd: None,
            on_demand_used_usd: None,
            on_demand_limit_usd: None,
        };
        let usage = to_provider_usage(&data, BillingSource::GrokWeb);
        assert_eq!(usage.windows[0].label, "Grok credits");
    }
}
