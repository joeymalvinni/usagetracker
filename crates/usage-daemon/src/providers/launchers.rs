use std::{
    ffi::OsStr,
    fs::OpenOptions,
    io::{Read, Write},
    os::unix::{
        fs::{OpenOptionsExt, PermissionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{mpsc, Arc, LazyLock},
    thread,
    time::{Duration, Instant},
};

use regex::Regex;
use tracing::{info, warn};
use usage_core::{default_app_dir, AccountId, ProviderId, ProviderSignInAction};

use crate::polling::RefreshCoordinator;

const AUTH_URL_CAPTURE_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_AUTH_OUTPUT_BYTES: usize = 128 * 1024;
const NO_BROWSER_SANDBOX_PROFILE: &str =
    "(version 1) (allow default) (deny process-exec (literal \"/usr/bin/open\"))";
static HTTPS_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"https://[^\s\x1b<>\"']+"#).expect("valid auth URL regex"));

pub(crate) struct LoginProcess {
    pub(crate) child: std::process::Child,
    pub(crate) authentication_url: Option<String>,
}

pub(crate) fn launch_codex_login(
    codex_home: &Path,
    action: ProviderSignInAction,
) -> anyhow::Result<LoginProcess> {
    let mut direct = login_command("codex", action);
    direct.arg("login");
    configure_codex_command(&mut direct, codex_home, action)?;
    let child = match direct.spawn() {
        Ok(child) => child,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/zsh".into());
            let mut fallback = login_command(shell, action);
            fallback.args(["-lic", "exec codex login"]);
            configure_codex_command(&mut fallback, codex_home, action)?;
            fallback.spawn().map_err(|fallback_err| {
                anyhow::anyhow!("failed to start Codex login: {fallback_err}")
            })?
        }
        Err(err) => return Err(anyhow::anyhow!("failed to start Codex login: {err}")),
    };
    finish_login_launch(child, action, &["openai.com", "chatgpt.com"])
}

fn configure_codex_command(
    command: &mut Command,
    codex_home: &Path,
    action: ProviderSignInAction,
) -> anyhow::Result<()> {
    command.env("CODEX_HOME", codex_home);
    configure_login_stdio(command, action)
}

pub(crate) fn launch_claude_login(
    config_dir: Option<&Path>,
    action: ProviderSignInAction,
) -> anyhow::Result<LoginProcess> {
    let mut direct = login_command("claude", action);
    direct.args(["auth", "login"]);
    configure_claude_environment(&mut direct, config_dir);
    configure_login_stdio(&mut direct, action)?;
    let child = match direct.spawn() {
        Ok(child) => child,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/zsh".into());
            let mut fallback = login_command(shell, action);
            fallback.args(["-lic", "exec claude auth login"]);
            configure_claude_environment(&mut fallback, config_dir);
            configure_login_stdio(&mut fallback, action)?;
            fallback.spawn().map_err(|fallback_err| {
                anyhow::anyhow!("failed to start Claude login: {fallback_err}")
            })?
        }
        Err(err) => return Err(anyhow::anyhow!("failed to start Claude login: {err}")),
    };
    finish_login_launch(child, action, &["anthropic.com", "claude.ai", "claude.com"])
}

pub(crate) fn launch_grok_login(
    binary: &Path,
    grok_home: &Path,
    action: ProviderSignInAction,
) -> anyhow::Result<LoginProcess> {
    let mut command = login_command(binary, action);
    command.arg("login");
    command.env("GROK_HOME", grok_home);
    configure_login_stdio(&mut command, action)?;
    let child = command
        .spawn()
        .map_err(|err| anyhow::anyhow!("failed to start Grok login: {err}"))?;
    finish_login_launch(child, action, &["x.ai", "grok.com"])
}

fn login_command(program: impl AsRef<OsStr>, action: ProviderSignInAction) -> Command {
    let program = program.as_ref();
    if action == ProviderSignInAction::CopyLink && Path::new("/usr/bin/sandbox-exec").is_file() {
        if let Some(program) = resolve_executable(program) {
            let mut command = Command::new("/usr/bin/sandbox-exec");
            command
                .args(["-p", NO_BROWSER_SANDBOX_PROFILE])
                .arg(program);
            return command;
        }
    }
    Command::new(program)
}

fn resolve_executable(program: &OsStr) -> Option<PathBuf> {
    let program_path = Path::new(program);
    if program_path.components().count() > 1 {
        return is_executable(program_path).then(|| program_path.to_path_buf());
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|directory| directory.join(program))
            .find(|candidate| is_executable(candidate))
    })
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .ok()
        .is_some_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn configure_claude_environment(command: &mut Command, config_dir: Option<&Path>) {
    if let Some(config_dir) = config_dir {
        command
            .env("CLAUDE_CONFIG_DIR", config_dir)
            .env_remove("CLAUDE_SECURESTORAGE_CONFIG_DIR");
    }
}

fn configure_login_stdio(
    command: &mut Command,
    action: ProviderSignInAction,
) -> anyhow::Result<()> {
    command
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match action {
        ProviderSignInAction::Open => {}
        ProviderSignInAction::CopyLink => configure_no_browser(command)?,
    }
    Ok(())
}

fn configure_no_browser(command: &mut Command) -> anyhow::Result<()> {
    let app_dir = default_app_dir()
        .ok_or_else(|| anyhow::anyhow!("failed to resolve ~/.usagetracker directory"))?;
    let shim_dir = app_dir.join("launchers").join("no-browser");
    let shim = ensure_no_browser_shim(&shim_dir)?;
    configure_no_browser_with_shim(command, &shim)
}

fn ensure_no_browser_shim(shim_dir: &Path) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(shim_dir)?;
    let shim = shim_dir.join("open");
    let contents = b"#!/bin/sh\nprintf '%s\\n' \"$@\"\n";
    let existing_is_valid = std::fs::metadata(&shim)
        .ok()
        .filter(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .and_then(|_| std::fs::read(&shim).ok())
        .is_some_and(|existing| existing == contents);
    if !existing_is_valid {
        let temporary = shim_dir.join(format!(".open.{}.tmp", uuid::Uuid::new_v4()));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o700)
            .open(&temporary)?;
        let result = (|| -> anyhow::Result<()> {
            file.write_all(contents)?;
            file.sync_all()?;
            std::fs::rename(&temporary, &shim)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result?;
    }
    Ok(shim)
}

fn configure_no_browser_with_shim(command: &mut Command, shim: &Path) -> anyhow::Result<()> {
    let shim_dir = shim
        .parent()
        .ok_or_else(|| anyhow::anyhow!("browser suppression helper has no parent directory"))?;
    let mut paths = vec![shim_dir.to_path_buf()];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    command
        .env("BROWSER", shim)
        .env("PATH", std::env::join_paths(paths)?);
    Ok(())
}

fn finish_login_launch(
    mut child: std::process::Child,
    action: ProviderSignInAction,
    allowed_domains: &'static [&'static str],
) -> anyhow::Result<LoginProcess> {
    let authentication_url = match action {
        ProviderSignInAction::Open => capture_authentication_url(&mut child, allowed_domains),
        ProviderSignInAction::CopyLink => {
            match capture_authentication_url(&mut child, allowed_domains) {
                Some(url) => Some(url),
                None => {
                    terminate_login_process(&mut child);
                    anyhow::bail!("provider CLI did not produce a sign-in link");
                }
            }
        }
    };
    Ok(LoginProcess {
        child,
        authentication_url,
    })
}

fn terminate_login_process(child: &mut std::process::Child) {
    let process_group = i32::try_from(child.id()).ok();
    let killed_group = process_group.is_some_and(|process_group| {
        // Login commands are placed in a dedicated process group before spawn,
        // so this also terminates wrapper-owned children such as npm Codex.
        unsafe { libc::kill(-process_group, libc::SIGKILL) == 0 }
    });
    if !killed_group {
        let _ = child.kill();
    }
    let _ = child.wait();
}

fn capture_authentication_url(
    child: &mut std::process::Child,
    allowed_domains: &'static [&'static str],
) -> Option<String> {
    let (sender, receiver) = mpsc::channel();
    if let Some(stdout) = child.stdout.take() {
        drain_login_output(stdout, sender.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        drain_login_output(stderr, sender.clone());
    }
    drop(sender);

    let deadline = Instant::now() + AUTH_URL_CAPTURE_TIMEOUT;
    let mut output = Vec::new();
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        match receiver.recv_timeout(remaining) {
            Ok(chunk) => {
                if output.len() < MAX_AUTH_OUTPUT_BYTES {
                    let remaining_capacity = MAX_AUTH_OUTPUT_BYTES - output.len();
                    output.extend_from_slice(&chunk[..chunk.len().min(remaining_capacity)]);
                }
                if let Some(url) = extract_authentication_url(&output, allowed_domains, false) {
                    return Some(url);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    extract_authentication_url(&output, allowed_domains, true)
}

fn drain_login_output(mut stream: impl Read + Send + 'static, sender: mpsc::Sender<Vec<u8>>) {
    thread::spawn(move || {
        let mut buffer = [0_u8; 2048];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    // Keep draining even after the short URL-capture window closes so the
                    // provider process cannot block on a full stdout or stderr pipe.
                    if sender.send(buffer[..count].to_vec()).is_err() {
                        let _ = std::io::copy(&mut stream, &mut std::io::sink());
                        break;
                    }
                }
            }
        }
    });
}

fn extract_authentication_url(
    output: &[u8],
    allowed_domains: &[&str],
    allow_at_end: bool,
) -> Option<String> {
    let text = String::from_utf8_lossy(output);
    HTTPS_URL.find_iter(&text).find_map(|candidate| {
        if !allow_at_end && candidate.end() == text.len() {
            return None;
        }
        let value = candidate
            .as_str()
            .trim_end_matches(['.', ',', ')', ']', '}']);
        let parsed = reqwest::Url::parse(value).ok()?;
        let host = parsed.host_str()?;
        allowed_domains
            .iter()
            .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
            .then(|| value.to_string())
    })
}

pub(crate) fn monitor_login(
    mut child: std::process::Child,
    refresh: Arc<RefreshCoordinator>,
    provider_id: &'static str,
    profile_id: Option<String>,
) {
    let runtime = tokio::runtime::Handle::current();
    std::thread::spawn(move || match child.wait() {
        Ok(status) if status.success() => {
            info!(
                provider_id,
                profile_id, "provider login completed; refreshing account"
            );
            runtime.spawn(async move {
                let provider = ProviderId::new(provider_id);
                if let Err(error) = refresh
                    .invalidate_cached_credentials(&provider, profile_id.as_deref())
                    .await
                {
                    warn!(
                        provider_id,
                        profile_id,
                        error = %error,
                        "failed to invalidate credentials after provider login"
                    );
                }
                let report = refresh.refresh(Some(std::slice::from_ref(&provider))).await;
                info!(
                    provider_id,
                    profile_id,
                    results = report.provider_results.len(),
                    "post-login provider refresh completed"
                );
            });
        }
        Ok(status) => {
            warn!(provider_id, profile_id, %status, "provider login process exited unsuccessfully");
        }
        Err(err) => {
            warn!(provider_id, profile_id, error = %err, "failed to wait for provider login process");
        }
    });
}

pub(crate) fn write_claude_profile_launcher(
    account_id: &AccountId,
    config_dir: Option<&Path>,
) -> anyhow::Result<PathBuf> {
    let app_dir = default_app_dir()
        .ok_or_else(|| anyhow::anyhow!("failed to resolve ~/.usagetracker directory"))?;
    let launcher_dir = app_dir.join("launchers");
    std::fs::create_dir_all(&launcher_dir)?;
    let launcher = launcher_dir.join(format!("claude-{}.command", account_id.as_str()));
    let temporary = launcher_dir.join(format!(
        ".claude-{}.{}.tmp",
        account_id.as_str(),
        uuid::Uuid::new_v4()
    ));
    let contents = claude_launcher_contents(config_dir);
    let result = (|| -> anyhow::Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o700)
            .open(&temporary)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, &launcher)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result?;
    Ok(launcher)
}

pub(crate) fn claude_launcher_contents(config_dir: Option<&Path>) -> String {
    let profile_setup = match config_dir {
        Some(path) => format!(
            "export CLAUDE_CONFIG_DIR={}\n",
            shell_single_quote(&path.display().to_string())
        ),
        None => "unset CLAUDE_CONFIG_DIR\n".to_string(),
    };
    format!("#!/bin/zsh -l\nunset CLAUDE_SECURESTORAGE_CONFIG_DIR\n{profile_setup}exec claude\n")
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub(crate) fn open_terminal(path: &Path) -> anyhow::Result<()> {
    let status = Command::new("open")
        .args(["-a", "Terminal"])
        .arg(path)
        .status()
        .map_err(|err| anyhow::anyhow!("failed to open Claude profile terminal: {err}"))?;
    anyhow::ensure!(status.success(), "failed to open Claude profile terminal");
    Ok(())
}

pub(crate) fn open_url(url: &str) -> anyhow::Result<()> {
    let status = Command::new("open")
        .arg(url)
        .status()
        .map_err(|err| anyhow::anyhow!("failed to open {url}: {err}"))?;
    anyhow::ensure!(status.success(), "failed to open {url}");
    Ok(())
}

pub(crate) fn handle_sign_in_url(
    url: &str,
    action: ProviderSignInAction,
) -> anyhow::Result<String> {
    if action == ProviderSignInAction::Open {
        open_url(url)?;
    }
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_quotes_profile_paths_and_clears_legacy_overrides() {
        let contents = claude_launcher_contents(Some(Path::new("/tmp/Claude's Work")));
        assert!(contents.contains("CLAUDE_CONFIG_DIR='/tmp/Claude'\"'\"'s Work'"));
        assert!(contents.contains("unset CLAUDE_SECURESTORAGE_CONFIG_DIR"));

        let legacy = claude_launcher_contents(None);
        assert!(legacy.contains("unset CLAUDE_CONFIG_DIR"));
    }

    #[test]
    fn extracts_only_complete_urls_from_allowed_authentication_domains() {
        let output = b"Update docs: https://example.com/help\nSign in: https://auth.openai.com/oauth/authorize?state=secret\n";
        assert_eq!(
            extract_authentication_url(output, &["openai.com"], false).as_deref(),
            Some("https://auth.openai.com/oauth/authorize?state=secret")
        );

        let partial = b"Sign in: https://auth.openai.com/oauth/authorize?state=part";
        assert_eq!(
            extract_authentication_url(partial, &["openai.com"], false),
            None
        );
        assert_eq!(
            extract_authentication_url(partial, &["openai.com"], true).as_deref(),
            Some("https://auth.openai.com/oauth/authorize?state=part")
        );
    }

    #[test]
    fn capture_mode_intercepts_open_and_returns_the_sign_in_url() {
        let root =
            std::env::temp_dir().join(format!("usage-login-capture-test-{}", uuid::Uuid::new_v4()));
        let shim = ensure_no_browser_shim(&root.join("bin")).unwrap();
        let mut command = login_command("/bin/sh", ProviderSignInAction::CopyLink);
        command
            .args([
                "-c",
                "open 'https://auth.openai.com/oauth/authorize?state=captured'; exec sleep 30",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_no_browser_with_shim(&mut command, &shim).unwrap();

        let child = command.spawn().unwrap();
        let mut login =
            finish_login_launch(child, ProviderSignInAction::CopyLink, &["openai.com"]).unwrap();
        assert_eq!(
            login.authentication_url.as_deref(),
            Some("https://auth.openai.com/oauth/authorize?state=captured")
        );
        let _ = login.child.kill();
        let _ = login.child.wait();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn copy_link_mode_sandboxes_the_provider_command_but_open_mode_does_not() {
        let copy = login_command("/bin/sh", ProviderSignInAction::CopyLink);
        assert_eq!(copy.get_program(), OsStr::new("/usr/bin/sandbox-exec"));
        assert_eq!(
            copy.get_args().collect::<Vec<_>>(),
            [
                OsStr::new("-p"),
                OsStr::new(NO_BROWSER_SANDBOX_PROFILE),
                OsStr::new("/bin/sh"),
            ]
        );

        let open = login_command("/bin/sh", ProviderSignInAction::Open);
        assert_eq!(open.get_program(), OsStr::new("/bin/sh"));
        assert_eq!(open.get_args().count(), 0);
    }

    #[test]
    fn copy_link_mode_blocks_the_absolute_macos_opener() {
        let unsandboxed_status = Command::new("/usr/bin/open")
            .arg("-h")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .code()
            .unwrap();
        let mut command = login_command("/bin/sh", ProviderSignInAction::CopyLink);
        command
            .args([
                "-c",
                "/usr/bin/open -h >/dev/null 2>&1; \
                 printf 'https://auth.openai.com/oauth/authorize?state=%s\\n' \"$?\"; \
                 exec sleep 30",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let child = command.spawn().unwrap();
        let mut login =
            finish_login_launch(child, ProviderSignInAction::CopyLink, &["openai.com"]).unwrap();
        let unsandboxed_url =
            format!("https://auth.openai.com/oauth/authorize?state={unsandboxed_status}");
        assert_ne!(
            login.authentication_url.as_deref(),
            Some(unsandboxed_url.as_str())
        );
        let _ = login.child.kill();
        let _ = login.child.wait();
    }

    #[test]
    fn missing_copy_link_command_preserves_not_found_for_shell_fallback() {
        let command = login_command(
            "usage-provider-cli-that-does-not-exist",
            ProviderSignInAction::CopyLink,
        );
        assert_eq!(
            command.get_program(),
            OsStr::new("usage-provider-cli-that-does-not-exist")
        );
    }

    #[test]
    fn open_mode_preserves_the_sign_in_url_for_v3_clients() {
        let child = Command::new("/bin/sh")
            .args([
                "-c",
                "printf '%s\\n' 'https://auth.openai.com/oauth/authorize?state=open'; exec sleep 30",
            ])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let started = Instant::now();
        let mut login =
            finish_login_launch(child, ProviderSignInAction::Open, &["openai.com"]).unwrap();

        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            login.authentication_url.as_deref(),
            Some("https://auth.openai.com/oauth/authorize?state=open")
        );
        let _ = login.child.kill();
        let _ = login.child.wait();
    }

    #[test]
    fn no_browser_shim_installation_is_atomic_under_concurrency() {
        let root =
            std::env::temp_dir().join(format!("usage-login-shim-test-{}", uuid::Uuid::new_v4()));
        let shim_dir = root.join("bin");
        let workers = (0..8)
            .map(|_| {
                let shim_dir = shim_dir.clone();
                thread::spawn(move || ensure_no_browser_shim(&shim_dir).unwrap())
            })
            .collect::<Vec<_>>();

        let paths = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        assert!(paths.iter().all(|path| path == &shim_dir.join("open")));
        assert_eq!(
            std::fs::read(shim_dir.join("open")).unwrap(),
            b"#!/bin/sh\nprintf '%s\\n' \"$@\"\n"
        );
        assert!(
            std::fs::metadata(shim_dir.join("open"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111
                != 0
        );
        assert_eq!(std::fs::read_dir(&shim_dir).unwrap().count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }
}
