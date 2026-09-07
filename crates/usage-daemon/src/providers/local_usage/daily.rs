use std::collections::BTreeMap;

use chrono::{Days, NaiveDate};
use usage_core::DailyUsagePoint;

/// Canonical per-day rows shared by the cost collectors and the dashboard.
/// Providers own the semantics of `priced`/`unpriced` here so the read side no
/// longer has to infer them from a partially-populated JSON row.
pub(crate) fn daily_usage_points(
    by_day: &BTreeMap<NaiveDate, DailyCostSummary>,
) -> Vec<DailyUsagePoint> {
    by_day
        .iter()
        .map(|(date, summary)| DailyUsagePoint {
            date: *date,
            tokens: summary.tokens,
            cost_usd: Some(summary.cost_usd),
            priced_tokens: summary.priced_tokens,
            unpriced_tokens: summary.unpriced_tokens,
        })
        .collect()
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct DailyCostSummary {
    pub(crate) cost_usd: f64,
    /// Total processed tokens, including cached input.
    pub(crate) tokens: u64,
    /// Cached input included in `tokens`. Providers without this detail leave it at zero.
    pub(crate) cached_input_tokens: u64,
    pub(crate) priced_tokens: u64,
    pub(crate) unpriced_tokens: u64,
    pub(crate) unpriced_models: BTreeMap<String, u64>,
    pub(crate) rows: u64,
}

impl DailyCostSummary {
    pub(crate) fn add(&mut self, source: &Self) {
        self.cost_usd += source.cost_usd;
        self.tokens = self.tokens.saturating_add(source.tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(source.cached_input_tokens);
        self.priced_tokens = self.priced_tokens.saturating_add(source.priced_tokens);
        self.unpriced_tokens = self.unpriced_tokens.saturating_add(source.unpriced_tokens);
        self.rows = self.rows.saturating_add(source.rows);
        for (model, tokens) in &source.unpriced_models {
            let total = self.unpriced_models.entry(model.clone()).or_default();
            *total = total.saturating_add(*tokens);
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct DailyRollup {
    pub(crate) today: DailyCostSummary,
    pub(crate) lookback: DailyCostSummary,
    pub(crate) by_day: BTreeMap<NaiveDate, DailyCostSummary>,
}

impl DailyRollup {
    pub(crate) fn from_days(
        days: &BTreeMap<NaiveDate, DailyCostSummary>,
        today: NaiveDate,
        lookback_days: u64,
    ) -> Self {
        Self::from_range(days, today, lookback_start(today, lookback_days))
    }

    pub(crate) fn from_range(
        days: &BTreeMap<NaiveDate, DailyCostSummary>,
        today: NaiveDate,
        start: NaiveDate,
    ) -> Self {
        let mut rollup = Self {
            today: days.get(&today).cloned().unwrap_or_default(),
            ..Self::default()
        };
        for (date, summary) in days.range(start..=today) {
            rollup.lookback.add(summary);
            rollup.by_day.insert(*date, summary.clone());
        }
        rollup
    }
}

pub(crate) fn lookback_start(today: NaiveDate, lookback_days: u64) -> NaiveDate {
    today
        .checked_sub_days(Days::new(lookback_days.saturating_sub(1)))
        .unwrap_or(today)
}

pub(crate) fn merge_daily_summary(
    target: &mut BTreeMap<NaiveDate, DailyCostSummary>,
    source: &BTreeMap<NaiveDate, DailyCostSummary>,
) {
    for (date, summary) in source {
        target.entry(*date).or_default().add(summary);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_points_carry_priced_and_unpriced_tokens() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 11).unwrap();
        let points = daily_usage_points(&BTreeMap::from([(
            date,
            DailyCostSummary {
                cost_usd: 1.5,
                tokens: 1_100,
                cached_input_tokens: 800,
                priced_tokens: 900,
                unpriced_tokens: 200,
                ..Default::default()
            },
        )]));

        assert_eq!(points.len(), 1);
        assert_eq!(points[0].date, date);
        assert_eq!(points[0].tokens, 1_100);
        assert_eq!(points[0].cost_usd, Some(1.5));
        assert_eq!(points[0].priced_tokens, 900);
        assert_eq!(points[0].unpriced_tokens, 200);
    }

    #[test]
    fn rollup_includes_today_and_exact_lookback() {
        let today = NaiveDate::from_ymd_opt(2026, 7, 11).unwrap();
        let mut days = BTreeMap::new();
        for (offset, tokens) in [(0, 1), (29, 2), (30, 4)] {
            days.insert(
                today.checked_sub_days(Days::new(offset)).unwrap(),
                DailyCostSummary {
                    tokens,
                    ..Default::default()
                },
            );
        }

        let rollup = DailyRollup::from_days(&days, today, 30);
        assert_eq!(rollup.today.tokens, 1);
        assert_eq!(rollup.lookback.tokens, 3);
        assert_eq!(rollup.by_day.len(), 2);
    }
}
