use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use usage_core::UsageEvent;

use crate::providers::{
    DailyUsageBucket, ProviderError, ProviderErrorKind, ProviderUsageEventBatch,
};

use super::number::NumberLike;

const NANODOLLARS_PER_CENT: f64 = 10_000_000.0;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CursorUsageEventsPage {
    pub(super) total_usage_events_count: u64,
    #[serde(default, alias = "usageEvents")]
    pub(super) usage_events_display: Vec<CursorUsageEvent>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CursorUsageEvent {
    timestamp: NumberLike,
    model: String,
    kind: String,
    #[serde(default)]
    requests_costs: Option<NumberLike>,
    #[serde(default)]
    is_token_based_call: bool,
    #[serde(default)]
    token_usage: Option<CursorTokenUsage>,
    #[serde(default)]
    cursor_token_fee: Option<NumberLike>,
    #[serde(default)]
    is_chargeable: bool,
    #[serde(default)]
    is_headless: bool,
    #[serde(default)]
    charged_cents: Option<NumberLike>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CursorTokenUsage {
    #[serde(default)]
    input_tokens: Option<NumberLike>,
    #[serde(default)]
    output_tokens: Option<NumberLike>,
    #[serde(default)]
    cache_read_tokens: Option<NumberLike>,
    #[serde(default)]
    cache_write_tokens: Option<NumberLike>,
    #[serde(default)]
    total_cents: Option<NumberLike>,
}

pub(super) struct CursorEventReport {
    pub(super) batch: ProviderUsageEventBatch,
    pub(super) daily_usage: Vec<DailyUsageBucket>,
    pub(super) metadata: serde_json::Value,
}

#[derive(Default)]
struct Aggregate {
    events: u64,
    tokens: u64,
    vendor_nanos: u64,
    metered_nanos: u64,
    chargeable_nanos: u64,
    fee_nanos: u64,
}

impl Aggregate {
    fn add(&mut self, event: &UsageEvent) -> Result<(), ProviderError> {
        self.events = checked_add(self.events, 1, "event count")?;
        self.tokens = checked_add(self.tokens, event_tokens(event)?, "token total")?;
        self.vendor_nanos = checked_add(
            self.vendor_nanos,
            usd_to_nanos(event.vendor_cost_usd)?,
            "vendor cost",
        )?;
        self.metered_nanos = checked_add(
            self.metered_nanos,
            usd_to_nanos(event.metered_cost_usd)?,
            "metered cost",
        )?;
        if event.chargeable {
            self.chargeable_nanos = checked_add(
                self.chargeable_nanos,
                usd_to_nanos(event.metered_cost_usd)?,
                "chargeable cost",
            )?;
        }
        self.fee_nanos = checked_add(
            self.fee_nanos,
            usd_to_nanos(event.provider_fee_usd)?,
            "provider fee",
        )?;
        Ok(())
    }

    fn json(&self) -> serde_json::Value {
        json!({
            "event_count": self.events,
            "tokens": self.tokens,
            "vendor_cost_usd": nanos_to_usd(self.vendor_nanos),
            "metered_cost_usd": nanos_to_usd(self.metered_nanos),
            "chargeable_cost_usd": nanos_to_usd(self.chargeable_nanos),
            "provider_fee_usd": nanos_to_usd(self.fee_nanos),
        })
    }
}

pub(super) fn normalize_usage_events(
    pages: Vec<CursorUsageEventsPage>,
    period_start: DateTime<Utc>,
    period_end: DateTime<Utc>,
) -> Result<CursorEventReport, ProviderError> {
    let expected = pages
        .first()
        .map(|page| page.total_usage_events_count)
        .unwrap_or(0);
    if pages
        .iter()
        .any(|page| page.total_usage_events_count != expected)
    {
        return Err(ProviderError::new(
            ProviderErrorKind::Parse,
            "Cursor usage event pagination changed during collection",
        ));
    }

    let raw_events = pages
        .into_iter()
        .flat_map(|page| page.usage_events_display)
        .collect::<Vec<_>>();
    if raw_events.len() as u64 != expected {
        return Err(ProviderError::new(
            ProviderErrorKind::Parse,
            "Cursor usage event pagination returned an inconsistent event count",
        ));
    }

    let mut keyed = raw_events
        .into_iter()
        .map(normalize_event)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|event| {
            let mut hashable = event.clone();
            hashable.event_id.clear();
            let encoded = serde_json::to_vec(&hashable).expect("usage event is serializable");
            (hex_digest(&Sha256::digest(encoded)), event)
        })
        .collect::<Vec<_>>();
    if keyed
        .iter()
        .any(|(_, event)| event.occurred_at < period_start || event.occurred_at > period_end)
    {
        return Err(ProviderError::new(
            ProviderErrorKind::Parse,
            "Cursor usage event fell outside the requested billing period",
        ));
    }
    keyed.sort_by(|left, right| {
        left.1
            .occurred_at
            .cmp(&right.1.occurred_at)
            .then_with(|| left.0.cmp(&right.0))
    });

    let mut occurrences = HashMap::<String, u64>::new();
    let mut events = Vec::with_capacity(keyed.len());
    let mut total = Aggregate::default();
    let mut by_day = BTreeMap::new();
    let mut by_model = BTreeMap::new();
    for (digest, mut event) in keyed {
        let occurrence = occurrences.entry(digest.clone()).or_default();
        event.event_id = format!("{digest}:{occurrence}");
        *occurrence = checked_add(*occurrence, 1, "duplicate occurrence")?;

        total.add(&event)?;
        by_day
            .entry(event.occurred_at.with_timezone(&Local).date_naive())
            .or_insert_with(Aggregate::default)
            .add(&event)?;
        by_model
            .entry(event.model.clone())
            .or_insert_with(Aggregate::default)
            .add(&event)?;
        events.push(event);
    }
    events.sort_by(|left, right| {
        right
            .occurred_at
            .cmp(&left.occurred_at)
            .then_with(|| right.event_id.cmp(&left.event_id))
    });

    let daily_usage = by_day
        .iter()
        .map(|(date, aggregate)| DailyUsageBucket {
            date: *date,
            tokens: aggregate.tokens,
            cost_usd: Some(nanos_to_usd(aggregate.metered_nanos)),
            source: "cursor_usage_events".to_string(),
        })
        .collect();
    let by_day_json = by_day
        .iter()
        .map(|(date, aggregate)| {
            let mut value = aggregate.json();
            value["date"] = json!(date.to_string());
            value
        })
        .collect::<Vec<_>>();
    let by_model_json = by_model
        .iter()
        .map(|(model, aggregate)| {
            let mut value = aggregate.json();
            value["model"] = json!(model);
            value
        })
        .collect::<Vec<_>>();

    Ok(CursorEventReport {
        batch: ProviderUsageEventBatch {
            period_start,
            period_end,
            daily_source: "cursor_usage_events".to_string(),
            events,
        },
        daily_usage,
        metadata: json!({
            "source": "cursor_usage_events",
            "estimate": false,
            "partial": false,
            "period_start": period_start,
            "period_end": period_end,
            "total": total.json(),
            "by_day": by_day_json,
            "by_model": by_model_json,
        }),
    })
}

fn normalize_event(raw: CursorUsageEvent) -> Result<UsageEvent, ProviderError> {
    let timestamp = raw.timestamp.nonnegative_integer("timestamp")?;
    let timestamp = i64::try_from(timestamp)
        .ok()
        .and_then(DateTime::from_timestamp_millis)
        .ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::Parse,
                "Cursor usage event timestamp was invalid",
            )
        })?;
    let model = raw.model.trim();
    if model.is_empty() {
        return Err(ProviderError::new(
            ProviderErrorKind::Parse,
            "Cursor usage event model was empty",
        ));
    }
    let token_usage = raw.token_usage.unwrap_or_default();
    Ok(UsageEvent {
        event_id: String::new(),
        occurred_at: timestamp,
        model: model.to_string(),
        kind: raw.kind.trim().to_string(),
        input_tokens: optional_integer(token_usage.input_tokens.as_ref(), "inputTokens")?,
        output_tokens: optional_integer(token_usage.output_tokens.as_ref(), "outputTokens")?,
        cache_read_tokens: optional_integer(
            token_usage.cache_read_tokens.as_ref(),
            "cacheReadTokens",
        )?,
        cache_write_tokens: optional_integer(
            token_usage.cache_write_tokens.as_ref(),
            "cacheWriteTokens",
        )?,
        request_units: optional_value(raw.requests_costs.as_ref(), "requestsCosts")?,
        vendor_cost_usd: nanos_to_usd(optional_cents(
            token_usage.total_cents.as_ref(),
            "tokenUsage.totalCents",
        )?),
        metered_cost_usd: nanos_to_usd(optional_cents(raw.charged_cents.as_ref(), "chargedCents")?),
        provider_fee_usd: nanos_to_usd(optional_cents(
            raw.cursor_token_fee.as_ref(),
            "cursorTokenFee",
        )?),
        chargeable: raw.is_chargeable,
        token_based: raw.is_token_based_call,
        headless: raw.is_headless,
    })
}

fn optional_integer(value: Option<&NumberLike>, field: &str) -> Result<u64, ProviderError> {
    value.map_or(Ok(0), |value| value.nonnegative_integer(field))
}

fn optional_value(value: Option<&NumberLike>, field: &str) -> Result<f64, ProviderError> {
    value.map_or(Ok(0.0), |value| value.nonnegative_value(field))
}

fn optional_cents(value: Option<&NumberLike>, field: &str) -> Result<u64, ProviderError> {
    let cents = optional_value(value, field)?;
    let nanos = cents * NANODOLLARS_PER_CENT;
    if !nanos.is_finite() || nanos > u64::MAX as f64 {
        return Err(ProviderError::new(
            ProviderErrorKind::Parse,
            format!("Cursor usage event {field} exceeded the supported monetary range"),
        ));
    }
    Ok(nanos.round() as u64)
}

fn event_tokens(event: &UsageEvent) -> Result<u64, ProviderError> {
    [
        event.input_tokens,
        event.output_tokens,
        event.cache_read_tokens,
        event.cache_write_tokens,
    ]
    .into_iter()
    .try_fold(0, |total, value| checked_add(total, value, "token total"))
}

fn usd_to_nanos(value: f64) -> Result<u64, ProviderError> {
    let nanos = value * 1_000_000_000.0;
    if !nanos.is_finite() || nanos < 0.0 || nanos > u64::MAX as f64 {
        return Err(ProviderError::new(
            ProviderErrorKind::Parse,
            "Cursor usage event cost exceeded the supported monetary range",
        ));
    }
    Ok(nanos.round() as u64)
}

fn nanos_to_usd(value: u64) -> f64 {
    value as f64 / 1_000_000_000.0
}

fn checked_add(left: u64, right: u64, field: &str) -> Result<u64, ProviderError> {
    left.checked_add(right).ok_or_else(|| {
        ProviderError::new(
            ProviderErrorKind::Parse,
            format!("Cursor usage event {field} overflowed"),
        )
    })
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}
