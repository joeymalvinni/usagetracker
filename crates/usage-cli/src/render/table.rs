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
        if self.headers.is_empty() {
            return String::new();
        }

        let widths = self.widths();
        let mut output = String::new();
        push_row(&mut output, &self.headers, &widths, |value| {
            theme.label(value)
        });
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

fn push_row(output: &mut String, row: &[String], widths: &[usize], paint: impl Fn(&str) -> String) {
    for (index, width) in widths.iter().enumerate() {
        if index > 0 {
            output.push_str("  ");
        }
        let value = row.get(index).map(String::as_str).unwrap_or_default();
        let value = truncate(value, *width);
        let padding = width.saturating_sub(visible_len(&value));
        let _ = write!(output, "{}{}", paint(&value), " ".repeat(padding));
    }
    output.push('\n');
}
