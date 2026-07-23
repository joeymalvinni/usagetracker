use std::fmt::Write;

use chrono::Datelike;

use crate::render::style::Theme;
use crate::render::summary::format_tokens;
use crate::views::ActivityView;
use usage_core::UsageDataScope;

pub fn render_activity(view: &ActivityView, color: bool) -> String {
    let theme = Theme::new(color);
    let provider_label = if view.filters.providers.is_empty() {
        "All providers".to_string()
    } else {
        view.title_providers.join(", ")
    };
    let mut output = String::new();
    let _ = writeln!(
        output,
        "{}",
        theme.title(&format!(
            "Activity · {} · {provider_label}",
            format_range(view.range.start_date, view.range.end_date)
        ))
    );
    output.push('\n');
    let _ = writeln!(
        output,
        "{}",
        theme.label("Date         Tokens      Cost  Coverage")
    );
    let _ = writeln!(
        output,
        "{}",
        theme.border("──────────  ─────────  ───────  ────────")
    );
    for day in &view.days {
        let coverage = coverage(day.priced_tokens, day.unpriced_tokens);
        let _ = writeln!(
            output,
            "{:<10}  {:>9}  {:>7}  {:>8}",
            day.date.format("%a %b %d"),
            format_tokens(day.tokens),
            day.cost_usd
                .map_or_else(|| "—".to_string(), |cost| format!("${cost:.2}")),
            coverage.map_or_else(|| "—".to_string(), |value| format!("{value:.0}%")),
        );
    }
    let total_tokens = view.days.iter().map(|day| day.tokens).sum();
    let costs = view
        .days
        .iter()
        .filter_map(|day| day.cost_usd)
        .collect::<Vec<_>>();
    let total_cost = (!costs.is_empty()).then(|| costs.into_iter().sum::<f64>());
    let _ = writeln!(output);
    let _ = writeln!(
        output,
        "{:<10}  {:>9}  {:>7}  {:>8}",
        "Total",
        format_tokens(total_tokens),
        total_cost.map_or_else(|| "—".to_string(), |cost| format!("${cost:.2}")),
        coverage(view.pricing.priced_tokens, view.pricing.unpriced_tokens)
            .map_or_else(|| "—".to_string(), |value| format!("{value:.0}%")),
    );
    let mut scope = view
        .provenance
        .scopes
        .iter()
        .map(scope_name)
        .collect::<Vec<_>>();
    if view.provenance.estimated {
        scope.push("estimated".to_string());
    }
    if view.provenance.partial {
        scope.push("partial".to_string());
    }
    if !scope.is_empty() {
        let _ = writeln!(output, "{:<10}  {}", "Scope", scope.join(" · "));
    }
    if total_tokens == 0 {
        let _ = writeln!(output, "\nNo observed activity in this period.");
    }
    output.trim_end().to_string()
}

fn format_range(start: chrono::NaiveDate, end: chrono::NaiveDate) -> String {
    if start.year() == end.year() && start.month() == end.month() {
        format!("{}–{}", start.format("%b %-d"), end.format("%-d"))
    } else if start.year() == end.year() {
        format!("{}–{}", start.format("%b %-d"), end.format("%b %-d"))
    } else {
        format!(
            "{}–{}",
            start.format("%b %-d, %Y"),
            end.format("%b %-d, %Y")
        )
    }
}

fn scope_name(scope: &UsageDataScope) -> String {
    match scope {
        UsageDataScope::AccountWide => "account wide",
        UsageDataScope::Organization => "organization wide",
        UsageDataScope::ThisDevice => "this Mac",
        UsageDataScope::SelectedLocalRoots => "selected local roots",
        UsageDataScope::Workspace => "workspace",
    }
    .to_string()
}

fn coverage(priced: u64, unpriced: u64) -> Option<f64> {
    let total = priced.saturating_add(unpriced);
    (total > 0).then(|| priced as f64 / total as f64 * 100.0)
}
