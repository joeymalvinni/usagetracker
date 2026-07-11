use chrono::{DateTime, TimeDelta, Utc};
use usage_core::{
    ForecastConfidence, ForecastStatus, UsageForecast, UsageSnapshot, UsageWindow, UsageWindowKind,
};

use crate::storage::StoredForecastHistory;

const MIN_SAMPLES: usize = 3;
const MIN_SPAN: TimeDelta = TimeDelta::minutes(15);
const RESET_DROP_PERCENT: f64 = 20.0;
const RESET_TOLERANCE: TimeDelta = TimeDelta::minutes(5);
const MAX_REGRESSION_SAMPLES: usize = 96;

#[derive(Clone, Copy)]
struct Observation {
    at: DateTime<Utc>,
    percent: f64,
}

struct RateEstimate {
    rate: f64,
    residual: f64,
    span: TimeDelta,
}

pub fn forecast_snapshot(
    current: &UsageSnapshot,
    history: &StoredForecastHistory,
    generated_at: DateTime<Utc>,
) -> Vec<UsageForecast> {
    current
        .windows
        .iter()
        .filter(|window| {
            !matches!(
                window.kind,
                UsageWindowKind::Credits | UsageWindowKind::Tokens
            )
        })
        .filter_map(|window| forecast_window(current, window, history, generated_at))
        .collect()
}

fn forecast_window(
    current: &UsageSnapshot,
    window: &UsageWindow,
    history: &StoredForecastHistory,
    generated_at: DateTime<Utc>,
) -> Option<UsageForecast> {
    let current_percent = window.percent_used.filter(|value| value.is_finite())?;
    let expected = expected_percent_used(window, generated_at);
    let pace_delta = expected.map(|value| current_percent - value);
    let samples = current_cycle(current, window, history);
    let estimate = window
        .reset_at
        .filter(|reset| *reset > generated_at)
        .and_then(|_| estimate_rate(&samples));

    let rate = estimate.as_ref().map(|value| value.rate);
    let projected = estimate.as_ref().and_then(|value| {
        let reset = window.reset_at?;
        let hours = (reset - generated_at).num_seconds() as f64 / 3600.0;
        Some(current_percent + value.rate * hours)
    });
    let projected_remaining = if current_percent >= 100.0 {
        Some(0.0)
    } else {
        projected
            .filter(|value| value.is_finite())
            .map(|value| (100.0 - value).clamp(0.0, 100.0))
    };
    let exhaustion = estimate.as_ref().and_then(|value| {
        let reset = window.reset_at?;
        if current_percent >= 100.0 {
            return Some(generated_at);
        }
        let seconds = (100.0 - current_percent) / value.rate * 3600.0;
        if !seconds.is_finite() || seconds <= 0.0 {
            return None;
        }
        let at = generated_at + TimeDelta::seconds(seconds.round() as i64);
        (at < reset).then_some(at)
    });

    let status = if current_percent >= 100.0 {
        ForecastStatus::Exhausted
    } else if exhaustion.is_some() || projected.is_some_and(|value| value >= 100.0) {
        ForecastStatus::AtRisk
    } else if let Some(delta) = pace_delta {
        if delta > 5.0 {
            ForecastStatus::AtRisk
        } else if delta < -5.0 {
            ForecastStatus::Safe
        } else {
            ForecastStatus::OnPace
        }
    } else if estimate.is_some() {
        ForecastStatus::Safe
    } else {
        ForecastStatus::InsufficientData
    };

    Some(UsageForecast {
        provider_id: current.provider_id.clone(),
        account_id: current.account_id.clone(),
        window_id: window.window_id.clone(),
        generated_at,
        reset_at: window.reset_at,
        current_percent_used: current_percent,
        expected_percent_used: expected,
        pace_delta_percent: pace_delta,
        rate_percent_per_hour: rate,
        projected_percent_at_reset: projected,
        projected_percent_remaining_at_reset: projected_remaining,
        predicted_exhaustion_at: exhaustion,
        status,
        sample_count: samples.len(),
        confidence: confidence(estimate.as_ref(), samples.len()),
    })
}

fn current_cycle(
    current: &UsageSnapshot,
    window: &UsageWindow,
    history: &StoredForecastHistory,
) -> Vec<Observation> {
    let mut observations = history
        .by_window
        .get(&window.window_id)
        .into_iter()
        .flatten()
        .filter(|observation| observation.collected_at <= current.collected_at)
        .filter_map(|observation| {
            if !compatible_reset(window.reset_at, observation.reset_at) {
                return None;
            }
            Some(Observation {
                at: observation.collected_at,
                percent: observation.percent_used,
            })
        })
        .collect::<Vec<_>>();
    observations.push(Observation {
        at: current.collected_at,
        percent: window.percent_used.unwrap_or_default(),
    });
    observations.sort_by_key(|sample| sample.at);
    observations.dedup_by_key(|sample| sample.at);

    if let Some(boundary) = observations
        .windows(2)
        .rposition(|pair| pair[0].percent - pair[1].percent >= RESET_DROP_PERCENT)
    {
        observations.drain(..=boundary);
    }
    observations
}

fn compatible_reset(current: Option<DateTime<Utc>>, historical: Option<DateTime<Utc>>) -> bool {
    match (current, historical) {
        (Some(current), Some(historical)) => (current - historical).abs() <= RESET_TOLERANCE,
        _ => true,
    }
}

fn estimate_rate(samples: &[Observation]) -> Option<RateEstimate> {
    let span = samples.last()?.at - samples.first()?.at;
    if samples.len() < MIN_SAMPLES || span < MIN_SPAN {
        return None;
    }
    let samples = regression_samples(samples);
    let origin = samples.first()?.at;
    let mut slopes = Vec::with_capacity(samples.len() * samples.len() / 2);
    for (index, left) in samples.iter().enumerate() {
        for right in &samples[index + 1..] {
            let hours = (right.at - left.at).num_milliseconds() as f64 / 3_600_000.0;
            if hours > 0.0 {
                slopes.push((right.percent - left.percent) / hours);
            }
        }
    }
    let rate = median(&mut slopes)?;
    if rate < 0.01 || !rate.is_finite() {
        return None;
    }

    let mut intercepts = samples
        .iter()
        .map(|sample| {
            let hours = (sample.at - origin).num_milliseconds() as f64 / 3_600_000.0;
            sample.percent - rate * hours
        })
        .collect::<Vec<_>>();
    let intercept = median(&mut intercepts)?;
    let mut residuals = samples
        .iter()
        .map(|sample| {
            let hours = (sample.at - origin).num_milliseconds() as f64 / 3_600_000.0;
            (sample.percent - (intercept + rate * hours)).abs()
        })
        .collect::<Vec<_>>();
    let residual = median(&mut residuals)?;
    if residual > 8.0 {
        return None;
    }
    Some(RateEstimate {
        rate,
        residual,
        span,
    })
}

fn regression_samples(samples: &[Observation]) -> Vec<Observation> {
    if samples.len() <= MAX_REGRESSION_SAMPLES {
        return samples.to_vec();
    }
    (0..MAX_REGRESSION_SAMPLES)
        .map(|index| samples[index * (samples.len() - 1) / (MAX_REGRESSION_SAMPLES - 1)])
        .collect()
}

fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    Some(if values.len().is_multiple_of(2) {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    })
}

fn confidence(estimate: Option<&RateEstimate>, sample_count: usize) -> ForecastConfidence {
    let Some(estimate) = estimate else {
        return ForecastConfidence::Low;
    };
    if sample_count >= 12 && estimate.span >= TimeDelta::hours(6) && estimate.residual <= 2.0 {
        ForecastConfidence::High
    } else if sample_count >= 5 && estimate.span >= TimeDelta::hours(1) {
        ForecastConfidence::Medium
    } else {
        ForecastConfidence::Low
    }
}

fn expected_percent_used(window: &UsageWindow, now: DateTime<Utc>) -> Option<f64> {
    let reset_at = window.reset_at.filter(|reset| *reset > now)?;
    let duration = expected_window_duration(window)?;
    let elapsed = (now - (reset_at - duration)).num_seconds().max(0) as f64;
    let total = duration.num_seconds() as f64;
    Some((elapsed / total * 100.0).clamp(0.0, 100.0))
}

fn expected_window_duration(window: &UsageWindow) -> Option<TimeDelta> {
    let name = format!(
        "{} {}",
        window.window_id.to_ascii_lowercase(),
        window.label.to_ascii_lowercase()
    );
    if name.contains("five_hour") || name.contains("five hour") || name.contains("session") {
        Some(TimeDelta::hours(5))
    } else if name.contains("seven_day") || name.contains("seven day") || name.contains("weekly") {
        Some(TimeDelta::days(7))
    } else if name.contains("daily") || name.contains("today") {
        Some(TimeDelta::days(1))
    } else if name.contains("monthly") || name.contains("30d") || name.contains("30 days") {
        Some(TimeDelta::days(30))
    } else {
        match window.kind {
            UsageWindowKind::Session => Some(TimeDelta::hours(5)),
            UsageWindowKind::Daily => Some(TimeDelta::days(1)),
            UsageWindowKind::Weekly => Some(TimeDelta::days(7)),
            UsageWindowKind::Monthly => Some(TimeDelta::days(30)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;
    use std::collections::HashMap;
    use usage_core::{AccountId, ProviderId};

    use crate::storage::{StoredForecastHistory, StoredWindowObservation};

    #[test]
    fn schedule_pace_is_available_from_the_first_snapshot() {
        let now = time(2026, 7, 10, 12, 0);
        let current = snapshot(now, now + TimeDelta::hours(2), 50.0);

        let forecast = &forecast_from_snapshots(&current, &[], now)[0];

        assert_eq!(forecast.sample_count, 1);
        assert_eq!(forecast.expected_percent_used, Some(60.0));
        assert_eq!(forecast.pace_delta_percent, Some(-10.0));
        assert_eq!(forecast.status, ForecastStatus::Safe);
        assert_eq!(forecast.confidence, ForecastConfidence::Low);
        assert!(forecast.rate_percent_per_hour.is_none());
    }

    #[test]
    fn median_slope_predicts_exhaustion_before_reset() {
        let now = time(2026, 7, 10, 12, 0);
        let reset = now + TimeDelta::hours(1);
        let current = snapshot(now, reset, 90.0);
        let history = vec![
            snapshot(now - TimeDelta::minutes(15), reset, 75.0),
            snapshot(now - TimeDelta::minutes(30), reset, 60.0),
        ];

        let forecast = &forecast_from_snapshots(&current, &history, now)[0];

        assert_eq!(forecast.sample_count, 3);
        assert_eq!(forecast.rate_percent_per_hour, Some(60.0));
        assert_eq!(forecast.projected_percent_at_reset, Some(150.0));
        assert_eq!(forecast.projected_percent_remaining_at_reset, Some(0.0));
        assert_eq!(forecast.status, ForecastStatus::AtRisk);
        assert_eq!(
            forecast.predicted_exhaustion_at,
            Some(now + TimeDelta::minutes(10))
        );
    }

    #[test]
    fn percentage_drop_discards_the_previous_cycle() {
        let now = time(2026, 7, 10, 12, 0);
        let reset = now + TimeDelta::hours(2);
        let current = snapshot(now, reset, 20.0);
        let history = vec![
            snapshot(now - TimeDelta::minutes(10), reset, 10.0),
            snapshot(now - TimeDelta::minutes(20), reset, 5.0),
            snapshot(now - TimeDelta::minutes(30), reset, 90.0),
        ];

        let forecast = &forecast_from_snapshots(&current, &history, now)[0];

        assert_eq!(forecast.sample_count, 3);
        assert!(forecast.rate_percent_per_hour.is_some());
    }

    #[test]
    fn flat_history_does_not_invent_an_observed_rate() {
        let now = time(2026, 7, 10, 12, 0);
        let reset = now + TimeDelta::hours(2);
        let current = snapshot(now, reset, 20.0);
        let history = vec![
            snapshot(now - TimeDelta::minutes(15), reset, 20.0),
            snapshot(now - TimeDelta::minutes(30), reset, 20.0),
        ];

        let forecast = &forecast_from_snapshots(&current, &history, now)[0];

        assert!(forecast.rate_percent_per_hour.is_none());
        assert!(forecast.projected_percent_at_reset.is_none());
        assert!(forecast.projected_percent_remaining_at_reset.is_none());
        assert!(forecast.predicted_exhaustion_at.is_none());
    }

    #[test]
    fn projection_exposes_percent_remaining_at_reset() {
        let now = time(2026, 7, 10, 12, 0);
        let reset = now + TimeDelta::hours(2);
        let current = snapshot(now, reset, 40.0);
        let history = vec![
            snapshot(now - TimeDelta::minutes(15), reset, 37.5),
            snapshot(now - TimeDelta::minutes(30), reset, 35.0),
        ];

        let forecast = &forecast_from_snapshots(&current, &history, now)[0];

        assert_eq!(forecast.rate_percent_per_hour, Some(10.0));
        assert_eq!(forecast.projected_percent_at_reset, Some(60.0));
        assert_eq!(forecast.projected_percent_remaining_at_reset, Some(40.0));
    }

    #[test]
    fn exhausted_window_projects_no_remaining_capacity_without_rate_history() {
        let now = time(2026, 7, 10, 12, 0);
        let current = snapshot(now, now + TimeDelta::hours(2), 100.0);

        let forecast = &forecast_from_snapshots(&current, &[], now)[0];

        assert_eq!(forecast.projected_percent_remaining_at_reset, Some(0.0));
        assert_eq!(forecast.status, ForecastStatus::Exhausted);
    }

    #[test]
    fn long_consistent_history_has_high_confidence() {
        let now = time(2026, 7, 10, 12, 0);
        let reset = now + TimeDelta::hours(1);
        let current = snapshot(now, reset, 30.0);
        let history = (1..=12)
            .map(|offset| {
                snapshot(
                    now - TimeDelta::minutes(offset * 30),
                    reset,
                    30.0 - offset as f64,
                )
            })
            .collect::<Vec<_>>();

        let forecast = &forecast_from_snapshots(&current, &history, now)[0];

        assert_eq!(forecast.sample_count, 13);
        assert_eq!(forecast.rate_percent_per_hour, Some(2.0));
        assert_eq!(forecast.confidence, ForecastConfidence::High);
    }

    #[test]
    fn missing_reset_is_insufficient_data() {
        let now = time(2026, 7, 10, 12, 0);
        let mut current = snapshot(now, now, 20.0);
        current.windows[0].reset_at = None;

        let forecast = &forecast_from_snapshots(&current, &[], now)[0];

        assert_eq!(forecast.status, ForecastStatus::InsufficientData);
        assert!(forecast.expected_percent_used.is_none());
    }

    fn forecast_from_snapshots(
        current: &UsageSnapshot,
        snapshots: &[UsageSnapshot],
        generated_at: DateTime<Utc>,
    ) -> Vec<UsageForecast> {
        let mut by_window = HashMap::<String, Vec<StoredWindowObservation>>::new();
        for snapshot in snapshots.iter().filter(|snapshot| {
            snapshot.provider_id == current.provider_id
                && snapshot.account_id == current.account_id
                && snapshot.collected_at <= current.collected_at
        }) {
            for window in &snapshot.windows {
                let Some(percent_used) = window.percent_used.filter(|value| value.is_finite())
                else {
                    continue;
                };
                by_window.entry(window.window_id.clone()).or_default().push(
                    StoredWindowObservation {
                        collected_at: snapshot.collected_at,
                        percent_used,
                        reset_at: window.reset_at,
                    },
                );
            }
        }
        forecast_snapshot(current, &StoredForecastHistory { by_window }, generated_at)
    }

    fn snapshot(at: DateTime<Utc>, reset_at: DateTime<Utc>, percent: f64) -> UsageSnapshot {
        UsageSnapshot {
            provider_id: ProviderId::new("codex"),
            account_id: AccountId::new("account"),
            collected_at: at,
            windows: vec![UsageWindow {
                window_id: "codex_session".to_string(),
                label: "Session".to_string(),
                kind: UsageWindowKind::Session,
                used: None,
                limit: None,
                remaining: None,
                percent_used: Some(percent),
                percent_remaining: Some(100.0 - percent),
                reset_at: Some(reset_at),
            }],
            metadata: json!({}),
        }
    }

    fn time(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
            .unwrap()
    }
}
