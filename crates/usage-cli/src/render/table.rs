use std::borrow::Cow;
use std::fmt::Write;

use super::style::{truncate, visible_len, Theme};

#[derive(Debug)]
pub(crate) struct Table {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Table {
    pub(crate) fn new(headers: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            headers: headers.into_iter().map(Into::into).collect(),
            rows: Vec::new(),
        }
    }

    pub(crate) fn row(&mut self, values: impl IntoIterator<Item = impl Into<String>>) {
        self.rows.push(values.into_iter().map(Into::into).collect());
    }

    pub(crate) fn render(&self, theme: Theme) -> String {
        self.render_with_width(theme, usize::MAX)
    }

    pub(crate) fn render_with_width(&self, theme: Theme, max_width: usize) -> String {
        if self.headers.is_empty() {
            return String::new();
        }

        let mut widths = self.widths();
        shrink_widths(&mut widths, &self.headers, max_width);
        let mut output = String::new();
        push_row(&mut output, &self.headers, &widths, |value| {
            theme.label(value)
        });
        let rule = widths
            .iter()
            .map(|width| "─".repeat(*width))
            .collect::<Vec<_>>();
        push_row(&mut output, &rule, &widths, |value| theme.border(value));
        for row in &self.rows {
            push_row(&mut output, row, &widths, |value| value.to_string());
        }
        output.trim_end().to_string()
    }

    fn widths(&self) -> Vec<usize> {
        let mut widths = self
            .headers
            .iter()
            .map(|header| visible_len(header))
            .collect::<Vec<_>>();

        for row in &self.rows {
            for (index, value) in row.iter().enumerate() {
                if index == widths.len() {
                    widths.push(0);
                }
                widths[index] = widths[index].max(visible_len(value));
            }
        }

        widths
    }
}

fn shrink_widths(widths: &mut [usize], headers: &[String], max_width: usize) {
    let spacing = widths.len().saturating_sub(1) * 2;
    while widths.iter().sum::<usize>().saturating_add(spacing) > max_width {
        let Some((index, _)) = widths
            .iter()
            .enumerate()
            .filter(|(index, width)| {
                **width
                    > headers
                        .get(*index)
                        .map_or(3, |header| visible_len(header).max(3))
            })
            .max_by_key(|(_, width)| **width)
        else {
            break;
        };
        widths[index] -= 1;
    }
}

fn push_row(output: &mut String, row: &[String], widths: &[usize], paint: impl Fn(&str) -> String) {
    for (index, width) in widths.iter().enumerate() {
        if index > 0 {
            output.push_str("  ");
        }
        let value = row.get(index).map(String::as_str).unwrap_or_default();
        let value_len = visible_len(value);
        let (value, value_len) = if value_len <= *width {
            (Cow::Borrowed(value), value_len)
        } else {
            let value = truncate(value, *width);
            let value_len = visible_len(&value);
            (Cow::Owned(value), value_len)
        };
        let padding = width.saturating_sub(value_len);
        let _ = write!(output, "{}{}", paint(&value), " ".repeat(padding));
    }
    output.push('\n');
}
