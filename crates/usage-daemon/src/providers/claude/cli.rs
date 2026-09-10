use std::{
    collections::BTreeMap,
    io::Read,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use chrono::{DateTime, Datelike, Days, TimeZone, Utc};
use chrono_tz::Tz;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tracing::{debug, warn};
use usage_core::{
    ProviderId, SnapshotDetail, UsageAmount, UsageUnit, UsageWindow, UsageWindowKind,
};
use wait_timeout::ChildExt;

use crate::providers::{
    json_map,
    local_usage::{stable_window_fragment, usage_kind_from_name},
    ProviderError, ProviderErrorKind, ProviderUsage,
};

use super::{CLAUDE_CLI_COLLECTION_MODE, PROVIDER_ID};

const CLAUDE_CLI_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_CLAUDE_CLI_STDOUT_BYTES: u64 = 1024 * 1024;
const MAX_CLAUDE_CLI_STDERR_BYTES: u64 = 64 * 1024;
const MAX_PERCENT: f64 = 100.0;

pub(super) struct ClaudeCliUsage {
    pub usage: ProviderUsage,
}

pub(super) fn collect_usage_from_cli(
    config_dir: Option<&Path>,
    profile_id: &str,
    access_token: Option<&str>,
) -> Result<ClaudeCliUsage, ProviderError> {
    match collect_usage_from_tui(config_dir, profile_id, access_token) {
        Ok(usage) => Ok(ClaudeCliUsage { usage }),
        Err(error)
            if matches!(
                error.kind(),
                ProviderErrorKind::RateLimited | ProviderErrorKind::Unauthorized
            ) =>
        {
            Err(error)
        }
        Err(tui_error) => collect_usage_from_json_cli(config_dir, profile_id, access_token)
            .map_err(|json_error| {
                ProviderError::new(
                    json_error.kind(),
                    format!(
                        "{}; JSON fallback: {}",
                        tui_error.short_message(),
                        json_error.short_message()
                    ),
                )
            }),
    }
}

// A real terminal is required for /usage on Claude versions whose print mode
// returns help text. Never answer trust/login prompts or send an inference prompt.
fn collect_usage_from_tui(
    config_dir: Option<&Path>,
    profile_id: &str,
    access_token: Option<&str>,
) -> Result<ProviderUsage, ProviderError> {
    let workspace = prepare_usage_workspace(config_dir, profile_id, access_token)?;
    let mut command = claude_command(config_dir, access_token);
    command.current_dir(workspace);
    command
        .args([
            "/usage",
            "--ax-screen-reader",
            "--settings",
            r#"{"disableAllHooks":true}"#,
            "--strict-mcp-config",
            "--mcp-config",
            r#"{"mcpServers":{}}"#,
        ])
        .env("TERM", "xterm-256color");
    read_usage_screen(command, CLAUDE_CLI_TIMEOUT)
}

// The usage probe has no reason to open a user's project. Prepare only our
// own empty directory, after Claude confirms this profile already has a login.
pub(super) fn prepare_usage_workspace(
    config_dir: Option<&Path>,
    profile_id: &str,
    access_token: Option<&str>,
) -> Result<std::path::PathBuf, ProviderError> {
    let failure = || {
        ProviderError::new(
            ProviderErrorKind::ProviderUnavailable,
            "Could not prepare Claude's usage screen",
        )
    };
    let home = dirs::home_dir().ok_or_else(failure)?;
    let profile_file = config_dir
        .map(|root| root.join(".claude.json"))
        .unwrap_or_else(|| home.join(".claude.json"));
    let workspace = crate::runtime::managed_profiles::profile_home(PROVIDER_ID, profile_id)
        .map_err(|_| failure())?
        .join("usage-probe");
    // Reject symlinks before creating or granting trust to a probe directory.
    let mut ancestor = Some(workspace.as_path());
    while let Some(path) = ancestor {
        if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err(failure());
        }
        ancestor = path.parent();
    }
    if std::fs::symlink_metadata(&profile_file).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(failure());
    }
    let original = std::fs::read(&profile_file).map_err(|_| failure())?;
    let value: serde_json::Value = serde_json::from_slice(&original).map_err(|_| failure())?;
    if usage_workspace_ready(&value, &workspace) {
        return create_probe_directory(&workspace)
            .map(|()| workspace)
            .map_err(|_| failure());
    }
    create_probe_directory(&workspace).map_err(|_| failure())?;
    let mut command = claude_command(config_dir, access_token);
    command
        .current_dir(&workspace)
        .args(["auth", "status", "--json"]);
    let status =
        run_claude_command(command, profile_id, Duration::from_secs(5)).map_err(|_| failure())?;
    let status: serde_json::Value = serde_json::from_str(&status).map_err(|_| failure())?;
    if !has_existing_cli_login(&status, access_token.is_some()) {
        return Err(ProviderError::new(
            ProviderErrorKind::Unauthorized,
            "Claude CLI is not signed in to the selected account",
        ));
    }
    let updated = initialized_usage_workspace(value, &workspace).map_err(|_| failure())?;
    create_probe_directory(&workspace).map_err(|_| failure())?;
    write_cli_config_if_unchanged(&profile_file, &original, &updated).map_err(|_| failure())?;
    Ok(workspace)
}

fn has_existing_cli_login(status: &serde_json::Value, supplied_token: bool) -> bool {
    status.get("loggedIn").and_then(serde_json::Value::as_bool) == Some(true)
        && match status.get("authMethod").and_then(serde_json::Value::as_str) {
            Some("claude.ai") => true,
            Some("oauth_token") => supplied_token,
            _ => false,
        }
}

fn create_probe_directory(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700).create(path)
}

fn usage_workspace_ready(value: &serde_json::Value, workspace: &Path) -> bool {
    value
        .get("hasCompletedOnboarding")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
        && value
            .get("projects")
            .and_then(|v| v.get(workspace.to_string_lossy().as_ref()))
            .and_then(|v| v.get("hasTrustDialogAccepted"))
            .and_then(serde_json::Value::as_bool)
            == Some(true)
}

fn initialized_usage_workspace(
    mut value: serde_json::Value,
    workspace: &Path,
) -> anyhow::Result<serde_json::Value> {
    // A cached identity alone is insufficient: caller must first verify login
    // using the selected CLI's auth status. This never creates credentials.
    super::client::parse_cached_profile_identity(&serde_json::to_vec(&value)?)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("invalid Claude config"))?;
    object.insert("hasCompletedOnboarding".into(), json!(true));
    let projects = object
        .entry("projects")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("invalid Claude projects"))?;
    let project = projects
        .entry(workspace.to_string_lossy().into_owned())
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("invalid probe project"))?;
    project.insert("hasTrustDialogAccepted".into(), json!(true));
    Ok(value)
}

fn write_cli_config_if_unchanged(
    path: &Path,
    expected: &[u8],
    value: &serde_json::Value,
) -> anyhow::Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let temporary = path.with_file_name(format!(".usage-setup-{}.json", uuid::Uuid::new_v4()));
    let result = (|| -> anyhow::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        serde_json::to_writer(&mut file, value)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        anyhow::ensure!(
            std::fs::read(path)? == expected,
            "Claude config changed during initialization"
        );
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    let _ = std::fs::remove_file(temporary);
    result
}

fn read_usage_screen(
    mut command: Command,
    timeout: Duration,
) -> Result<ProviderUsage, ProviderError> {
    use std::{
        fs::File,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::process::CommandExt,
        },
    };
    let failure = || {
        ProviderError::new(
            ProviderErrorKind::ProviderUnavailable,
            "Claude usage screen could not be read",
        )
    };
    let mut master = -1;
    let mut slave = -1;
    let mut size = libc::winsize {
        ws_row: 60,
        ws_col: 160,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    } != 0
    {
        return Err(failure());
    }
    let mut terminal = unsafe { File::from_raw_fd(master) };
    let slave = unsafe { File::from_raw_fd(slave) };
    command
        .stdin(slave.try_clone().map_err(|_| failure())?)
        .stdout(slave.try_clone().map_err(|_| failure())?)
        .stderr(slave);
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
        if libc::fcntl(terminal.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) < 0
            || libc::fcntl(terminal.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) < 0
        {
            return Err(failure());
        }
    }
    let child = command.spawn().map_err(|_| failure())?;
    drop(command);
    struct TerminalChild(std::process::Child);
    impl Drop for TerminalChild {
        fn drop(&mut self) {
            unsafe {
                libc::kill(-(self.0.id() as i32), libc::SIGKILL);
            }
            let _ = self.0.kill();
            let _ = self.0.wait_timeout(Duration::from_secs(1));
        }
    }
    let mut child = TerminalChild(child);
    let started = Instant::now();
    let mut last_output = Instant::now();
    let mut output = Vec::new();
    let mut buffer = [0; 8192];
    while started.elapsed() < timeout {
        match terminal.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                output.extend_from_slice(&buffer[..count]);
                last_output = Instant::now();
                if output.len() as u64 > MAX_CLAUDE_CLI_STDOUT_BYTES {
                    return Err(failure());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => break,
        }
        if last_output.elapsed() >= Duration::from_millis(350) {
            let text = terminal_text(&output);
            let lower = text.to_ascii_lowercase();
            if lower.contains("rate limited") || lower.contains("too many requests") {
                return Err(ProviderError::new(
                    ProviderErrorKind::RateLimited,
                    "Claude CLI usage is rate limited",
                ));
            }
            if lower.contains("let's get started") || lower.contains("trust this folder") {
                return Err(ProviderError::new(
                    ProviderErrorKind::ProviderUnavailable,
                    "Claude CLI could not initialize its usage screen for the selected account",
                ));
            }
            if let Ok(mut usage) = parse_usage_text(&text, Utc::now()) {
                usage
                    .detail
                    .extra
                    .insert("command".into(), json!("claude /usage (TUI)"));
                return Ok(usage);
            }
            if lower.contains("please log in") || lower.contains("not logged in") {
                return Err(ProviderError::new(
                    ProviderErrorKind::Unauthorized,
                    "Claude CLI requires sign-in",
                ));
            }
        }
        if child.0.try_wait().map_err(|_| failure())?.is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(failure())
}

fn terminal_text(bytes: &[u8]) -> String {
    static ESCAPES: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07\x1b]*(?:\x07|\x1b\\))").unwrap()
    });
    ESCAPES
        .replace_all(&String::from_utf8_lossy(bytes), "")
        .replace('\r', "\n")
}

fn collect_usage_from_json_cli(
    config_dir: Option<&Path>,
    profile_id: &str,
    access_token: Option<&str>,
) -> Result<ClaudeCliUsage, ProviderError> {
    let raw_output = run_claude_usage_cli(config_dir, profile_id, access_token).map_err(|err| {
        ProviderError::new(
            ProviderErrorKind::ProviderUnavailable,
            format!("Claude CLI usage fallback failed: {err}"),
        )
    })?;
    let decoded = serde_json::from_str::<serde_json::Value>(&raw_output)
        .and_then(|value| ClaudePrintResponse::deserialize(&value));
    let response = match decoded {
        Ok(decoded) => decoded,
        Err(err) => {
            warn!(
                provider_id = PROVIDER_ID,
                profile_id,
                collection_mode = CLAUDE_CLI_COLLECTION_MODE,
                failure_stage = "cli_json_decode",
                stdout_bytes = raw_output.len(),
                stdout_fingerprint = output_fingerprint(&raw_output),
                parse_error_category = ?err.classify(),
                "Claude CLI usage returned invalid JSON"
            );
            return Err(ProviderError::new(
                ProviderErrorKind::Parse,
                "Claude CLI usage fallback returned invalid JSON",
            ));
        }
    };

    if response.is_error {
        let text = response.result.to_ascii_lowercase();
        let kind = if text.contains("rate limit") || text.contains("too many requests") {
            ProviderErrorKind::RateLimited
        } else if text.contains("not logged in") || text.contains("please log in") {
            ProviderErrorKind::Unauthorized
        } else {
            ProviderErrorKind::ProviderUnavailable
        };
        return Err(ProviderError::new(
            kind,
            "Claude CLI reported a usage command error",
        ));
    }
    let usage = match parse_usage_text(&response.result, Utc::now()) {
        Ok(usage) => usage,
        Err(err) => {
            let diagnostics = usage_text_diagnostics(&response.result);
            warn!(
                provider_id = PROVIDER_ID,
                profile_id,
                collection_mode = CLAUDE_CLI_COLLECTION_MODE,
                failure_stage = "cli_usage_text_parse",
                error_code = err.kind().as_str(),
                error = %err,
                result_bytes = diagnostics.bytes,
                result_lines = diagnostics.lines,
                non_empty_lines = diagnostics.non_empty_lines,
                current_heading_candidates = diagnostics.current_heading_candidates,
                percent_used_markers = diagnostics.percent_used_markers,
                reset_markers = diagnostics.reset_markers,
                output_category = diagnostics.category,
                result_fingerprint = diagnostics.fingerprint,
                "Claude CLI usage result contained no parseable usage windows"
            );
            return Err(err);
        }
    };
    Ok(ClaudeCliUsage { usage })
}

fn usage_command(config_dir: Option<&Path>, access_token: Option<&str>) -> Command {
    let mut command = claude_command(config_dir, access_token);
    command
        .arg("-p")
        .arg("/usage")
        .arg("--output-format")
        .arg("json")
        .arg("--no-session-persistence")
        .args([
            "--settings",
            r#"{"disableAllHooks":true}"#,
            "--strict-mcp-config",
            "--mcp-config",
            r#"{"mcpServers":{}}"#,
        ]);
    command
}

fn claude_command(config_dir: Option<&Path>, access_token: Option<&str>) -> Command {
    let mut command = Command::new("claude");
    command.args(["--setting-sources", "user"]);
    command
        .env_remove("CLAUDE_CODE_OAUTH_TOKEN")
        .env_remove("CLAUDE_CODE_OAUTH_REFRESH_TOKEN")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env_remove("ANTHROPIC_BASE_URL")
        .env_remove("CLAUDE_CODE_USE_BEDROCK")
        .env_remove("CLAUDE_CODE_USE_VERTEX")
        .env_remove("CLAUDE_CODE_USE_FOUNDRY")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CLAUDE_SECURESTORAGE_CONFIG_DIR")
        .stdin(Stdio::null())
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .env_remove("all_proxy")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(access_token) = access_token {
        command.env("CLAUDE_CODE_OAUTH_TOKEN", access_token);
    }
    if let Some(config_dir) = config_dir {
        command
            .env("CLAUDE_CONFIG_DIR", config_dir)
            .env_remove("CLAUDE_SECURESTORAGE_CONFIG_DIR");
    }
    command
}

fn run_claude_usage_cli(
    config_dir: Option<&Path>,
    profile_id: &str,
    access_token: Option<&str>,
) -> anyhow::Result<String> {
    run_claude_command(
        usage_command(config_dir, access_token),
        profile_id,
        CLAUDE_CLI_TIMEOUT,
    )
}

fn run_claude_command(
    mut command: Command,
    profile_id: &str,
    timeout: Duration,
) -> anyhow::Result<String> {
    let started = Instant::now();
    let mut child = command.spawn()?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to open Claude CLI stdout"))?;
    let stdout_thread = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(MAX_CLAUDE_CLI_STDOUT_BYTES + 1)
            .read_to_end(&mut bytes)?;
        Ok::<_, std::io::Error>(bytes)
    });
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to open Claude CLI stderr"))?;
    let stderr_thread = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr
            .take(MAX_CLAUDE_CLI_STDERR_BYTES + 1)
            .read_to_end(&mut bytes)?;
        Ok::<_, std::io::Error>(bytes)
    });

    let status = match child.wait_timeout(timeout)? {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("Claude command timed out after {timeout:?}");
        }
    };

    let stdout = stdout_thread
        .join()
        .map_err(|_| anyhow::anyhow!("Claude CLI stdout reader panicked"))??;
    let stderr = stderr_thread
        .join()
        .map_err(|_| anyhow::anyhow!("Claude CLI stderr reader panicked"))??;
    if stdout.len() > MAX_CLAUDE_CLI_STDOUT_BYTES as usize {
        anyhow::bail!(
            "Claude command exceeded the {MAX_CLAUDE_CLI_STDOUT_BYTES}-byte stdout limit"
        );
    }
    if stderr.len() > MAX_CLAUDE_CLI_STDERR_BYTES as usize {
        anyhow::bail!(
            "Claude command exceeded the {MAX_CLAUDE_CLI_STDERR_BYTES}-byte stderr limit"
        );
    }
    let stdout = String::from_utf8(stdout)?;
    let stderr = String::from_utf8_lossy(&stderr);

    debug!(
        provider_id = PROVIDER_ID,
        profile_id,
        collection_mode = CLAUDE_CLI_COLLECTION_MODE,
        status = status.code(),
        success = status.success(),
        elapsed_ms = started.elapsed().as_millis(),
        stdout_bytes = stdout.len(),
        stderr_bytes = stderr.len(),
        "Claude CLI usage command completed"
    );

    if !status.success() {
        anyhow::bail!(
            "Claude command exited with status {status} ({} stderr bytes)",
            stderr.len()
        );
    }

    Ok(stdout)
}

#[derive(Debug, Deserialize)]
struct ClaudePrintResponse {
    result: String,
    #[serde(default)]
    is_error: bool,
}

#[derive(Debug, Eq, PartialEq)]
struct UsageTextDiagnostics {
    bytes: usize,
    lines: usize,
    non_empty_lines: usize,
    current_heading_candidates: usize,
    percent_used_markers: usize,
    reset_markers: usize,
    category: &'static str,
    fingerprint: String,
}

fn usage_text_diagnostics(text: &str) -> UsageTextDiagnostics {
    let lines = text.lines().collect::<Vec<_>>();
    let lowercase = text.to_ascii_lowercase();
    UsageTextDiagnostics {
        bytes: text.len(),
        lines: lines.len(),
        non_empty_lines: lines.iter().filter(|line| !line.trim().is_empty()).count(),
        current_heading_candidates: lines
            .iter()
            .filter(|line| usage_heading(line).is_some())
            .count(),
        percent_used_markers: lowercase.matches("% used").count(),
        reset_markers: lowercase.matches("reset").count(),
        category: usage_output_category(&lowercase),
        fingerprint: output_fingerprint(text),
    }
}

fn usage_output_category(lowercase: &str) -> &'static str {
    if lowercase.trim().is_empty() {
        "empty"
    } else if lowercase.contains("login")
        || lowercase.contains("log in")
        || lowercase.contains("authenticate")
    {
        "authentication_prompt"
    } else if lowercase.contains("error") || lowercase.contains("failed") {
        "error_text"
    } else if lowercase.contains("usage") || lowercase.contains("current session") {
        "usage_text_without_windows"
    } else {
        "unrecognized_text"
    }
}

fn output_fingerprint(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    format!("{digest:x}")[..12].to_string()
}

fn parse_usage_text(
    text: &str,
    collected_at: DateTime<Utc>,
) -> Result<ProviderUsage, ProviderError> {
    let mut windows = Vec::new();
    let mut reset_text_by_window = BTreeMap::new();
    let mut pending: Option<ParsedUsageWindow> = None;

    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if let Some(window) = single_line_usage_window(line) {
            push_pending_window(
                pending.take(),
                collected_at,
                &mut windows,
                &mut reset_text_by_window,
            );
            push_pending_window(
                Some(window),
                collected_at,
                &mut windows,
                &mut reset_text_by_window,
            );
            continue;
        }

        if let Some(heading) = usage_heading(line) {
            push_pending_window(
                pending.take(),
                collected_at,
                &mut windows,
                &mut reset_text_by_window,
            );
            pending = Some(ParsedUsageWindow {
                heading,
                percent_used: None,
                reset_text: None,
            });
            continue;
        }

        let Some(window) = pending.as_mut() else {
            continue;
        };

        if window.percent_used.is_none() {
            if let Some(percent_used) = percent_used_from_line(line) {
                window.percent_used = Some(percent_used);
            }
            continue;
        }

        if let Some(reset_text) = reset_text_from_line(line) {
            window.reset_text = Some(reset_text.to_string());
            push_pending_window(
                pending.take(),
                collected_at,
                &mut windows,
                &mut reset_text_by_window,
            );
        }
    }

    push_pending_window(
        pending,
        collected_at,
        &mut windows,
        &mut reset_text_by_window,
    );

    if windows.is_empty() {
        return Err(ProviderError::new(
            ProviderErrorKind::Parse,
            "Claude CLI usage output did not contain usage windows",
        ));
    }

    Ok(ProviderUsage {
        provider_id: ProviderId::new(PROVIDER_ID),
        collected_at,
        windows,
        detail: SnapshotDetail {
            collection_mode: Some(CLAUDE_CLI_COLLECTION_MODE.to_string()),
            extra: json_map(json!({
                "command": "claude -p /usage --output-format json --no-session-persistence",
                "reset_text_by_window": reset_text_by_window,
            })),
            ..SnapshotDetail::default()
        },
    })
}

#[derive(Debug)]
struct ParsedUsageWindow {
    heading: String,
    percent_used: Option<f64>,
    reset_text: Option<String>,
}

fn single_line_usage_window(line: &str) -> Option<ParsedUsageWindow> {
    let (heading, detail) = line.split_once(':')?;
    let heading = usage_heading(heading)?;
    let percent_used = percent_used_from_line(detail)?;
    let reset_text = reset_text_from_line(detail).map(str::to_string);

    Some(ParsedUsageWindow {
        heading,
        percent_used: Some(percent_used),
        reset_text,
    })
}

fn push_pending_window(
    pending: Option<ParsedUsageWindow>,
    collected_at: DateTime<Utc>,
    windows: &mut Vec<UsageWindow>,
    reset_text_by_window: &mut BTreeMap<String, String>,
) {
    let Some(pending) = pending else {
        return;
    };
    let Some(percent_used) = pending.percent_used else {
        return;
    };

    let heading = pending.heading.to_ascii_lowercase();
    let name = if heading == "current session" {
        "five_hour".to_string()
    } else if heading == "current week" || heading == "current week (all models)" {
        "seven_day".to_string()
    } else if let Some(scope) = heading
        .strip_prefix("current week (")
        .and_then(|s| s.strip_suffix(')'))
    {
        format!("seven_day_{scope}")
    } else {
        heading
    };
    let window_id = format!("claude_usage_utilization_{}", stable_window_fragment(&name));
    if let Some(reset_text) = pending.reset_text.as_ref() {
        reset_text_by_window.insert(window_id.clone(), reset_text.clone());
    }

    windows.retain(|window| window.window_id != window_id);
    windows.push(percent_window(
        window_id,
        claude_label(&pending.heading),
        usage_kind_from_name(&pending.heading),
        percent_used,
        pending
            .reset_text
            .as_deref()
            .and_then(|value| parse_reset_at(value, collected_at)),
    ));
}

fn usage_heading(line: &str) -> Option<String> {
    let value = line.trim();
    let lower = value.to_ascii_lowercase();
    if lower.starts_with("current ") && !lower.contains("% used") {
        Some(value.to_string())
    } else {
        None
    }
}

fn percent_window(
    window_id: String,
    label: String,
    kind: UsageWindowKind,
    percent_used: f64,
    reset_at: Option<DateTime<Utc>>,
) -> UsageWindow {
    let percent_used = percent_used.clamp(0.0, MAX_PERCENT);
    let percent_remaining = MAX_PERCENT - percent_used;

    UsageWindow {
        window_id,
        label,
        kind,
        used: Some(UsageAmount {
            value: percent_used,
            unit: UsageUnit::Percent,
        }),
        limit: Some(UsageAmount {
            value: MAX_PERCENT,
            unit: UsageUnit::Percent,
        }),
        remaining: Some(UsageAmount {
            value: percent_remaining,
            unit: UsageUnit::Percent,
        }),
        percent_used: Some(percent_used),
        percent_remaining: Some(percent_remaining),
        reset_at,
    }
}

fn percent_used_from_line(line: &str) -> Option<f64> {
    let marker = line.find("% used")?;
    let prefix = &line[..marker];
    let prefix = prefix.trim_end();
    let start = prefix
        .char_indices()
        .rev()
        .find(|(_, char)| !char.is_ascii_digit() && *char != '.')
        .map(|(index, char)| index + char.len_utf8())
        .unwrap_or(0);

    prefix[start..].parse().ok()
}

fn reset_text_from_line(line: &str) -> Option<&str> {
    let lower = line.to_ascii_lowercase();
    let index = lower.find("resets ")?;
    Some(line[index + "resets ".len()..].trim())
}

fn parse_reset_at(value: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let (body, tz) = split_timezone(value);
    let tz = tz
        .and_then(|value| value.parse::<Tz>().ok())
        .unwrap_or(chrono_tz::UTC);
    let local_now = now.with_timezone(&tz);

    if let Some((date, time)) = body.split_once(" at ") {
        let (month, day) = parse_month_day(date)?;
        let (hour, minute) = parse_time_of_day(time)?;
        let mut reset = local_datetime(tz, local_now.year(), month, day, hour, minute)?;
        if reset.with_timezone(&Utc) + chrono::Duration::hours(24) < now {
            reset = local_datetime(tz, local_now.year() + 1, month, day, hour, minute)?;
        }
        return Some(reset.with_timezone(&Utc));
    }

    let (hour, minute) = parse_time_of_day(body)?;
    let date = local_now.date_naive();
    let mut reset = local_datetime(tz, date.year(), date.month(), date.day(), hour, minute)?;
    if reset.with_timezone(&Utc) <= now {
        let tomorrow = date.checked_add_days(Days::new(1))?;
        reset = local_datetime(
            tz,
            tomorrow.year(),
            tomorrow.month(),
            tomorrow.day(),
            hour,
            minute,
        )?;
    }
    Some(reset.with_timezone(&Utc))
}

fn split_timezone(value: &str) -> (&str, Option<&str>) {
    let value = value.trim();
    let Some(open) = value.rfind('(') else {
        return (value, None);
    };
    let Some(close) = value[open..].find(')').map(|offset| open + offset) else {
        return (value, None);
    };

    (value[..open].trim(), Some(value[open + 1..close].trim()))
}

fn parse_month_day(value: &str) -> Option<(u32, u32)> {
    let mut parts = value.split_whitespace();
    let month = month_number(parts.next()?)?;
    let day = parts.next()?.trim_end_matches(',').parse().ok()?;
    Some((month, day))
}

fn month_number(value: &str) -> Option<u32> {
    match value.to_ascii_lowercase().as_str() {
        "jan" | "january" => Some(1),
        "feb" | "february" => Some(2),
        "mar" | "march" => Some(3),
        "apr" | "april" => Some(4),
        "may" => Some(5),
        "jun" | "june" => Some(6),
        "jul" | "july" => Some(7),
        "aug" | "august" => Some(8),
        "sep" | "sept" | "september" => Some(9),
        "oct" | "october" => Some(10),
        "nov" | "november" => Some(11),
        "dec" | "december" => Some(12),
        _ => None,
    }
}

fn parse_time_of_day(value: &str) -> Option<(u32, u32)> {
    let compact = value
        .trim()
        .to_ascii_lowercase()
        .replace(char::is_whitespace, "");
    let (time, is_pm) = compact
        .strip_suffix("am")
        .map(|time| (time, false))
        .or_else(|| compact.strip_suffix("pm").map(|time| (time, true)))?;
    let (hour, minute) = match time.split_once(':') {
        Some((hour, minute)) => (hour.parse::<u32>().ok()?, minute.parse::<u32>().ok()?),
        None => (time.parse::<u32>().ok()?, 0),
    };
    if hour == 0 || hour > 12 || minute > 59 {
        return None;
    }

    let hour = match (hour, is_pm) {
        (12, false) => 0,
        (12, true) => 12,
        (_, true) => hour + 12,
        _ => hour,
    };
    Some((hour, minute))
}

fn local_datetime(
    tz: Tz,
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
) -> Option<DateTime<Tz>> {
    tz.with_ymd_and_hms(year, month, day, hour, minute, 0)
        .earliest()
}

fn claude_label(value: &str) -> String {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return "Claude".to_string();
    };
    format!("Claude {}{}", first.to_ascii_lowercase(), chars.as_str())
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "uses the selected local Claude sign-in"]
    fn live_managed_usage_probe() {
        let directory =
            std::env::var("USAGE_TEST_CLAUDE_CONFIG_DIR").expect("set test profile directory");
        let id = std::env::var("USAGE_TEST_CLAUDE_PROFILE_ID").expect("set test profile id");
        let usage = collect_usage_from_tui(Some(Path::new(&directory)), &id, None).unwrap();
        assert!(usage.windows.len() >= 2);
        for window in usage.windows {
            println!("{}: {:?}% used", window.label, window.percent_used);
        }
    }

    #[test]
    fn setup_requires_the_selected_subscription_login_or_supplied_oauth_token() {
        assert!(has_existing_cli_login(
            &json!({"loggedIn":true,"authMethod":"claude.ai"}),
            false
        ));
        let injected = json!({"loggedIn":true,"authMethod":"oauth_token"});
        assert!(has_existing_cli_login(&injected, true));
        assert!(!has_existing_cli_login(&injected, false));
        assert!(!has_existing_cli_login(
            &json!({"loggedIn":false,"authMethod":"claude.ai"}),
            true
        ));
        assert!(!has_existing_cli_login(
            &json!({"loggedIn":true,"authMethod":"api_key"}),
            true
        ));
    }

    #[test]
    fn initialization_preserves_identity_and_only_trusts_the_probe() {
        let value = json!({
            "oauthAccount":{"accountUuid":"986efbc1-2be6-407a-9bcc-2e429b8e358d"},
            "projects":{"/user/project":{"hasTrustDialogAccepted":false,"custom":"keep"}},
            "custom":"keep", "mcpServers":{"configured":"keep"}
        });
        let probe = Path::new("/tracker/profiles/claude/work/usage-probe");
        let updated = initialized_usage_workspace(value.clone(), probe).unwrap();
        assert!(usage_workspace_ready(&updated, probe));
        assert_eq!(updated["oauthAccount"], value["oauthAccount"]);
        assert_eq!(
            updated["projects"]["/user/project"],
            value["projects"]["/user/project"]
        );
        assert_eq!(updated["mcpServers"], value["mcpServers"]);
        assert_eq!(updated["custom"], "keep");
        assert!(initialized_usage_workspace(json!({}), probe).is_err());
    }

    #[test]
    fn initialization_refuses_to_overwrite_a_concurrent_cli_change() {
        let directory =
            std::env::temp_dir().join(format!("usage-cli-config-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join(".claude.json");
        std::fs::write(&path, br#"{"new":"login"}"#).unwrap();
        assert!(write_cli_config_if_unchanged(
            &path,
            b"{}",
            &json!({"hasCompletedOnboarding":true})
        )
        .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), br#"{"new":"login"}"#);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn tui_reader_reads_meters_and_terminates_a_waiting_child() {
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "printf 'Current session\\n12%% used\\nResets 10pm (UTC)\\n'; sleep 5",
        ]);
        let started = Instant::now();
        let usage = read_usage_screen(command, Duration::from_secs(2)).unwrap();
        assert_eq!(usage.windows[0].percent_used, Some(12.0));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn tui_reader_bounds_a_child_that_never_produces_usage() {
        let mut command = Command::new("/bin/sleep");
        command.arg("5");
        let started = Instant::now();
        assert!(read_usage_screen(command, Duration::from_millis(100)).is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn native_cli_auth_is_profile_scoped_and_removes_inherited_tokens() {
        let command = usage_command(Some(Path::new("/profiles/work")), None);
        let env: BTreeMap<_, _> = command.get_envs().collect();
        assert_eq!(env[std::ffi::OsStr::new("CLAUDE_CODE_OAUTH_TOKEN")], None);
        assert_eq!(
            env[std::ffi::OsStr::new("CLAUDE_CONFIG_DIR")],
            Some(std::ffi::OsStr::new("/profiles/work"))
        );
    }

    #[test]
    fn terminal_redraws_keep_only_the_latest_meter() {
        let text = terminal_text(
            b"\x1b[32mCurrent session\x1b[0m\r\n12% used\r\nCurrent session\r\n13% used\r\n",
        );
        let usage = parse_usage_text(&text, Utc::now()).unwrap();
        assert_eq!(usage.windows.len(), 1);
        assert_eq!(usage.windows[0].percent_used, Some(13.0));
    }

    #[test]
    fn fallback_uses_the_selected_token_without_inheriting_another_auth_source() {
        let command = super::usage_command(
            Some(std::path::Path::new("/profiles/work")),
            Some("test-token"),
        );
        let env: std::collections::BTreeMap<_, _> = command.get_envs().collect();
        assert_eq!(
            env[std::ffi::OsStr::new("CLAUDE_CODE_OAUTH_TOKEN")],
            Some(std::ffi::OsStr::new("test-token"))
        );
        assert_eq!(
            env[std::ffi::OsStr::new("CLAUDE_CONFIG_DIR")],
            Some(std::ffi::OsStr::new("/profiles/work"))
        );
        for key in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_REFRESH_TOKEN",
            "CLAUDE_SECURESTORAGE_CONFIG_DIR",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
        ] {
            assert_eq!(env[std::ffi::OsStr::new(key)], None);
        }
        assert!(!command.get_args().any(|arg| arg == "test-token"));
    }

    use super::*;

    #[test]
    fn summarizes_unparseable_usage_without_logging_raw_text() {
        let text =
            "Please log in to Claude Code\nCurrent session\nUsage is temporarily unavailable";
        let diagnostics = usage_text_diagnostics(text);

        assert_eq!(diagnostics.bytes, text.len());
        assert_eq!(diagnostics.lines, 3);
        assert_eq!(diagnostics.non_empty_lines, 3);
        assert_eq!(diagnostics.current_heading_candidates, 1);
        assert_eq!(diagnostics.percent_used_markers, 0);
        assert_eq!(diagnostics.reset_markers, 0);
        assert_eq!(diagnostics.category, "authentication_prompt");
        assert_eq!(diagnostics.fingerprint.len(), 12);
    }

    #[test]
    fn parses_claude_usage_print_windows() {
        let now = Utc.with_ymd_and_hms(2026, 7, 7, 20, 0, 0).unwrap();
        let usage = parse_usage_text(
            r#"
You are currently using your subscription to power your Claude Code usage

Current session: 20% used · resets Jul 7 at 9:39pm (America/Los_Angeles)
Current week (all models): 25% used · resets Jul 7 at 6pm (America/Los_Angeles)
Current week (Fable): 17% used
"#,
            now,
        )
        .unwrap();

        assert_eq!(usage.windows.len(), 3);

        let session = find_window(&usage.windows, "claude_usage_utilization_five_hour");
        assert!(matches!(session.kind, UsageWindowKind::Session));
        assert_eq!(session.label, "Claude current session");
        assert_eq!(session.percent_used, Some(20.0));
        assert_eq!(session.percent_remaining, Some(80.0));
        assert_eq!(
            session.reset_at.unwrap(),
            Utc.with_ymd_and_hms(2026, 7, 8, 4, 39, 0).unwrap()
        );

        let all_models = find_window(&usage.windows, "claude_usage_utilization_seven_day");
        assert!(matches!(all_models.kind, UsageWindowKind::Weekly));
        assert_eq!(all_models.percent_used, Some(25.0));
    }

    #[test]
    fn still_parses_multiline_usage_windows() {
        let now = Utc.with_ymd_and_hms(2026, 7, 7, 20, 0, 0).unwrap();
        let usage = parse_usage_text(
            r#"
Current session
██████████                                         20% used
Resets 9:40pm (America/Los_Angeles)
"#,
            now,
        )
        .unwrap();

        let session = find_window(&usage.windows, "claude_usage_utilization_five_hour");
        assert_eq!(session.percent_used, Some(20.0));
        assert_eq!(
            session.reset_at.unwrap(),
            Utc.with_ymd_and_hms(2026, 7, 8, 4, 40, 0).unwrap()
        );
    }

    fn find_window<'a>(windows: &'a [UsageWindow], window_id: &str) -> &'a UsageWindow {
        windows
            .iter()
            .find(|window| window.window_id == window_id)
            .unwrap_or_else(|| panic!("missing window {window_id}"))
    }
}
