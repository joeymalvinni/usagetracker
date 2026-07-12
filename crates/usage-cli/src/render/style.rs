use std::io::{self, IsTerminal};

use chrono::{DateTime, Local, Utc};

/// Lower bound for the rendered box/table width.
const MIN_WIDTH: usize = 60;
/// Width used when stdout is not a terminal (pipes, redirects, tests).
const DEFAULT_WIDTH: usize = 80;

/// Effective rendering width: the terminal's column count when stdout is a
/// TTY, otherwise a stable default. The result never exceeds `max_width`.
pub(crate) fn output_width(max_width: usize) -> usize {
    let width = if io::stdout().is_terminal() {
        terminal_size::terminal_size()
            .map(|(terminal_size::Width(cols), _)| cols as usize)
            .unwrap_or(DEFAULT_WIDTH)
    } else {
        DEFAULT_WIDTH
    };
    width.clamp(MIN_WIDTH, max_width)
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Theme {
    color: bool,
}

impl Theme {
    pub(crate) fn new(color: bool) -> Self {
        Self { color }
    }

    pub(crate) fn paint(self, code: &str, value: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{value}\x1b[0m")
        } else {
            value.to_string()
        }
    }

    pub(crate) fn border(self, value: &str) -> String {
        self.paint("2;37", value)
    }

    pub(crate) fn title(self, value: &str) -> String {
        self.paint("1;97", value)
    }

    pub(crate) fn label(self, value: &str) -> String {
        self.paint("2;37", value)
    }

    pub(crate) fn value(self, value: &str) -> String {
        self.paint("1;37", value)
    }

    pub(crate) fn muted(self, value: &str) -> String {
        self.paint("2;37", value)
    }

    pub(crate) fn accent(self, value: &str) -> String {
        self.paint("36", value)
    }

    pub(crate) fn good(self, value: &str) -> String {
        self.paint("32", value)
    }

    pub(crate) fn warn(self, value: &str) -> String {
        self.paint("33", value)
    }

    pub(crate) fn danger(self, value: &str) -> String {
        self.paint("31", value)
    }

    pub(crate) fn quota(self, percent_remaining: f64, value: &str) -> String {
        if percent_remaining >= 50.0 {
            self.good(value)
        } else if percent_remaining >= 20.0 {
            self.warn(value)
        } else {
            self.danger(value)
        }
    }

    pub(crate) fn pace(self, status: &str) -> String {
        match status {
            "over" => self.warn(status),
            "under" | "on track" => self.good(status),
            _ => self.value(status),
        }
    }

    pub(crate) fn status(self, status: &str) -> String {
        match status {
            "ok" => self.good(status),
            "disabled" => self.muted(status),
            "credentials_missing" | "auth_failed" | "rate_limited" | "backing_off" => {
                self.warn(status)
            }
            _ => self.danger(status),
        }
    }
}

pub(crate) fn format_local_time(time: Option<DateTime<Utc>>) -> String {
    time.map(|time| {
        time.with_timezone(&Local)
            .format("%Y-%m-%d %-I:%M %p")
            .to_string()
    })
    .unwrap_or_else(|| "-".to_string())
}

/// Compact, human-friendly "time since" label ("just now", "3m ago", "2d
/// ago"), falling back to an absolute date for anything older than a week.
pub(crate) fn relative_time(time: DateTime<Utc>) -> String {
    let delta = Utc::now() - time;
    if delta.num_minutes() < 1 {
        return "just now".to_string();
    }
    if delta.num_hours() < 1 {
        return format!("{}m ago", delta.num_minutes());
    }
    if delta.num_days() < 1 {
        return format!("{}h ago", delta.num_hours());
    }
    if delta.num_days() < 7 {
        return format!("{}d ago", delta.num_days());
    }
    time.with_timezone(&Local).format("%b %-d").to_string()
}

/// `relative_time` for an optional timestamp, rendering `-` when absent.
pub(crate) fn relative_time_opt(time: Option<DateTime<Utc>>) -> String {
    time.map(relative_time).unwrap_or_else(|| "-".to_string())
}

pub(crate) fn format_provider_name(provider_id: &str) -> String {
    match provider_id {
        "codex" => "Codex".to_string(),
        "claude" => "Claude".to_string(),
        "opencode_go" => "OpenCode Go".to_string(),
        "grok" => "Grok".to_string(),
        value => title_case(&value.replace(['_', '-'], " ")),
    }
}

pub(crate) fn format_collection_mode(provider_id: &str, mode: &str) -> String {
    match (provider_id, mode) {
        ("codex", "wham_usage_api") => "openai-web".to_string(),
        ("codex", "codex_app_server_rate_limits") => "app-server".to_string(),
        ("claude", "claude_cli_usage") => "terminal".to_string(),
        ("claude", "oauth_usage_api") => "web".to_string(),
        ("opencode_go", "opencode_go_web_console") => "web".to_string(),
        ("opencode_go", "opencode_go_local_sqlite") => "local".to_string(),
        ("grok", "grok_cli_billing_rpc") => "cli-rpc".to_string(),
        ("grok", "grok_web_billing_rpc") => "web".to_string(),
        _ => mode.replace('_', "-"),
    }
}

pub(crate) fn title_case(value: &str) -> String {
    value
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn collapse_spaces(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn truncate(value: &str, max_chars: usize) -> String {
    if visible_len(value) <= max_chars {
        return value.to_string();
    }
    strip_ansi(value)
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>()
        + "…"
}

pub(crate) fn visible_len(value: &str) -> usize {
    let mut len = 0;
    let mut chars = value.chars().peekable();
    while let Some(char) = chars.next() {
        if char == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
            }
            for char in chars.by_ref() {
                if ('@'..='~').contains(&char) {
                    break;
                }
            }
        } else {
            len += 1;
        }
    }
    len
}

#[cfg(test)]
pub(crate) fn strip_ansi(value: &str) -> String {
    strip_ansi_inner(value)
}

#[cfg(not(test))]
fn strip_ansi(value: &str) -> String {
    strip_ansi_inner(value)
}

fn strip_ansi_inner(value: &str) -> String {
    let mut stripped = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(char) = chars.next() {
        if char == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
            }
            for char in chars.by_ref() {
                if ('@'..='~').contains(&char) {
                    break;
                }
            }
        } else {
            stripped.push(char);
        }
    }
    stripped
}
