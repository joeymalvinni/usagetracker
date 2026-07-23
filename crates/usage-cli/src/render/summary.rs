use std::borrow::Cow;
use std::fmt::Write;

use crate::render::style::{relative_time, truncate, visible_len, Theme};
use crate::views::{DataState, SummaryLimit, SummaryProvider, SummaryView};
use usage_core::ConnectivityStatus;

pub fn render_summary(view: &SummaryView, color: bool, width: usize) -> String {
    let theme = Theme::new(color);
    if view.providers.is_empty() {
        return "No providers are enabled.\n\nNext step   usage providers enable PROVIDER"
            .to_string();
    }

    let mut columns = vec![
        Column::Provider,
        Column::Accounts,
        Column::Limits,
        Column::Today,
        Column::Lookback,
        Column::Cost,
        Column::Updated,
    ];
    for removable in [Column::Accounts, Column::Cost, Column::Today] {
        if table_width(view, &columns) <= width {
            break;
        }
        columns.retain(|column| *column != removable);
    }
    let table = render_table(view, &columns, theme, width);
    if view.connectivity == ConnectivityStatus::Offline {
        format!("Offline — showing last known usage.\n\n{table}")
    } else {
        table
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Column {
    Provider,
    Accounts,
    Limits,
    Today,
    Lookback,
    Cost,
    Updated,
}

impl Column {
    fn header(self) -> &'static str {
        match self {
            Self::Provider => "Provider",
            Self::Accounts => "Accounts",
            Self::Limits => "Limits",
            Self::Today => "Today",
            Self::Lookback => "30d",
            Self::Cost => "Cost",
            Self::Updated => "Updated",
        }
    }
}

fn table_width(view: &SummaryView, columns: &[Column]) -> usize {
    column_widths(view, columns).into_iter().sum::<usize>() + columns.len().saturating_sub(1) * 2
}

fn column_widths(view: &SummaryView, columns: &[Column]) -> Vec<usize> {
    columns
        .iter()
        .map(|column| {
            view.providers
                .iter()
                .map(|provider| visible_len(&cell(provider, *column)))
                .chain([visible_len(column.header())])
                .max()
                .unwrap_or_default()
        })
        .collect()
}

fn render_table(view: &SummaryView, columns: &[Column], theme: Theme, width: usize) -> String {
    let mut widths = column_widths(view, columns);
    for (column, minimum) in [
        (Column::Limits, 8),
        (Column::Provider, 8),
        (Column::Updated, 7),
    ] {
        let total = widths.iter().sum::<usize>() + columns.len().saturating_sub(1) * 2;
        if total <= width {
            break;
        }
        if let Some(index) = columns.iter().position(|candidate| *candidate == column) {
            let reducible = widths[index].saturating_sub(minimum);
            widths[index] -= reducible.min(total - width);
        }
    }
    let mut output = String::new();
    push_row(
        &mut output,
        &columns
            .iter()
            .map(|column| column.header().to_string())
            .collect::<Vec<_>>(),
        &widths,
        theme,
        true,
    );
    push_row(
        &mut output,
        &widths
            .iter()
            .map(|width| "─".repeat(*width))
            .collect::<Vec<_>>(),
        &widths,
        theme,
        true,
    );
    for provider in &view.providers {
        let cells = columns
            .iter()
            .enumerate()
            .map(|(index, column)| {
                let value = if *column == Column::Limits {
                    limits_cell(provider, Some(widths[index]))
                } else {
                    cell(provider, *column)
                };
                if visible_len(&value) <= widths[index] {
                    value
                } else if *column == Column::Updated && provider.stale {
                    "stale".to_string()
                } else {
                    value
                }
            })
            .collect::<Vec<_>>();
        push_row(&mut output, &cells, &widths, theme, false);
    }
    output.trim_end().to_string()
}

fn push_row(output: &mut String, cells: &[String], widths: &[usize], theme: Theme, header: bool) {
    for (index, width) in widths.iter().enumerate() {
        if index > 0 {
            output.push_str("  ");
        }
        let cell = cells.get(index).map(String::as_str).unwrap_or_default();
        let cell_len = visible_len(cell);
        let (cell, cell_len) = if cell_len <= *width {
            (Cow::Borrowed(cell), cell_len)
        } else {
            let cell = truncate(cell, *width);
            let cell_len = visible_len(&cell);
            (Cow::Owned(cell), cell_len)
        };
        let padding = width.saturating_sub(cell_len);
        if header {
            let _ = write!(output, "{}{}", theme.label(&cell), " ".repeat(padding));
        } else {
            let _ = write!(output, "{cell}{}", " ".repeat(padding));
        }
    }
    output.push('\n');
}

fn cell(provider: &SummaryProvider, column: Column) -> String {
    match column {
        Column::Provider => match provider.data_state {
            DataState::Disabled => format!("{} (disabled)", provider.display_name),
            DataState::NoData => format!("{} (no data)", provider.display_name),
            DataState::Available => provider.display_name.clone(),
        },
        Column::Accounts => provider.account_count.to_string(),
        Column::Limits => limits_cell(provider, None),
        Column::Today => provider
            .today_tokens
            .map_or_else(|| "—".to_string(), format_tokens),
        Column::Lookback => provider
            .lookback_tokens
            .map_or_else(|| "—".to_string(), format_tokens),
        Column::Cost => provider
            .lookback_cost_usd
            .map_or_else(|| "—".to_string(), |cost| format!("${cost:.2}")),
        Column::Updated => provider.oldest_snapshot_at.map_or_else(
            || "—".to_string(),
            |updated| {
                format!(
                    "{}{}",
                    relative_time(updated),
                    if provider.stale { " (stale)" } else { "" }
                )
            },
        ),
    }
}

fn limits_cell(provider: &SummaryProvider, max_width: Option<usize>) -> String {
    if provider.limits.is_empty() {
        return "—".to_string();
    }
    let parts = provider.limits.iter().map(format_limit).collect::<Vec<_>>();
    let Some(max_width) = max_width else {
        return parts.join(" · ");
    };
    let mut output = String::new();
    for (index, part) in parts.iter().enumerate() {
        let separator = (!output.is_empty()).then_some(" · ").unwrap_or_default();
        if visible_len(&output) + separator.len() + visible_len(part) <= max_width {
            output.push_str(separator);
            output.push_str(part);
            continue;
        }
        let suffix = if index < parts.len() { " · …" } else { "" };
        if !output.is_empty() && visible_len(&output) + visible_len(suffix) <= max_width {
            output.push_str(suffix);
        }
        break;
    }
    if output.is_empty() {
        "…".to_string()
    } else {
        output
    }
}

fn format_limit(limit: &SummaryLimit) -> String {
    if let (Some(minimum), Some(maximum)) = (
        limit.minimum_percent_remaining,
        limit.maximum_percent_remaining,
    ) {
        if (minimum - maximum).abs() < 0.05 {
            format!("{} {:.0}% left", limit.label, minimum)
        } else {
            format!("{} {:.0}–{:.0}% left", limit.label, minimum, maximum)
        }
    } else if let (Some(minimum), Some(maximum)) =
        (limit.minimum_remaining, limit.maximum_remaining)
    {
        let value = if (minimum - maximum).abs() < f64::EPSILON {
            compact_number(minimum)
        } else {
            format!("{}–{}", compact_number(minimum), compact_number(maximum))
        };
        format!("{} {value}", limit.label)
    } else {
        limit.label.clone()
    }
}

pub(crate) fn format_tokens(tokens: u64) -> String {
    if tokens == 0 {
        return "0".to_string();
    }
    compact_number(tokens as f64)
}

fn compact_number(value: f64) -> String {
    if value >= 1_000_000_000.0 {
        format_compact(value / 1_000_000_000.0, "B")
    } else if value >= 1_000_000.0 {
        format_compact(value / 1_000_000.0, "M")
    } else if value >= 1_000.0 {
        format_compact(value / 1_000.0, "K")
    } else if value.fract().abs() < f64::EPSILON {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

fn format_compact(value: f64, suffix: &str) -> String {
    if value >= 100.0 || value.fract().abs() < 0.05 {
        format!("{value:.0}{suffix}")
    } else {
        format!("{value:.1}{suffix}")
    }
}
