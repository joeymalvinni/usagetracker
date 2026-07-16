use std::collections::BTreeMap;

use chrono::{Days, NaiveDate};

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

#[derive(Debug, serde::Serialize)]
pub(crate) struct DailyCostRow {
    date: String,
    cost_usd: f64,
    tokens: u64,
    activity_tokens: u64,
    cached_input_tokens: u64,
    priced_tokens: u64,
    unpriced_tokens: u64,
    unpriced_models: Vec<UnpricedModelRow>,
    rows: u64,
}

#[derive(Debug, serde::Serialize)]
struct UnpricedModelRow {
    model: String,
    tokens: u64,
}

pub(crate) fn daily_cost_rows(by_day: &BTreeMap<NaiveDate, DailyCostSummary>) -> Vec<DailyCostRow> {
    by_day
        .iter()
        .map(|(date, summary)| DailyCostRow {
            date: date.to_string(),
            cost_usd: summary.cost_usd,
            tokens: summary.tokens,
            activity_tokens: summary.tokens,
            cached_input_tokens: summary.cached_input_tokens,
            priced_tokens: summary.priced_tokens,
            unpriced_tokens: summary.unpriced_tokens,
            unpriced_models: summary
                .unpriced_models
                .iter()
                .map(|(model, tokens)| UnpricedModelRow {
                    model: model.clone(),
                    tokens: *tokens,
                })
                .collect(),
            rows: summary.rows,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_rows_publish_processed_activity_with_cached_input() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 11).unwrap();
        let rows = daily_cost_rows(&BTreeMap::from([(
            date,
            DailyCostSummary {
                tokens: 1_100,
                cached_input_tokens: 800,
                ..Default::default()
            },
        )]));
        let value = serde_json::to_value(rows).unwrap();

        assert_eq!(value[0]["tokens"], 1_100);
        assert_eq!(value[0]["cached_input_tokens"], 800);
        assert_eq!(value[0]["activity_tokens"], 1_100);
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
