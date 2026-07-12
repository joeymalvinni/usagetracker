use std::{
    fs::OpenOptions,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
};

use tracing::{info, warn};
use usage_core::{default_app_dir, AccountId, ProviderId};

use crate::polling::RefreshCoordinator;

pub(crate) fn launch_codex_login(codex_home: &Path) -> anyhow::Result<std::process::Child> {
    let mut direct = Command::new("codex");
    direct.arg("login");
    configure_codex_stdio(&mut direct, codex_home);
    match direct.spawn() {
        Ok(child) => Ok(child),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/zsh".into());
            let mut fallback = Command::new(shell);
            fallback.args(["-lic", "exec codex login"]);
            configure_codex_stdio(&mut fallback, codex_home);
            fallback.spawn().map_err(|fallback_err| {
                anyhow::anyhow!("failed to start Codex login: {fallback_err}")
            })
        }
        Err(err) => Err(anyhow::anyhow!("failed to start Codex login: {err}")),
    }
}

fn configure_codex_stdio(command: &mut Command, codex_home: &Path) {
    command
        .env("CODEX_HOME", codex_home)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
}

pub(crate) fn launch_claude_login(
    config_dir: Option<&Path>,
) -> anyhow::Result<std::process::Child> {
    let mut direct = Command::new("claude");
    direct.args(["auth", "login"]);
    configure_claude_environment(&mut direct, config_dir);
    configure_browser_stdio(&mut direct);
    match direct.spawn() {
        Ok(child) => Ok(child),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/zsh".into());
            let mut fallback = Command::new(shell);
            fallback.args(["-lic", "exec claude auth login"]);
            configure_claude_environment(&mut fallback, config_dir);
            configure_browser_stdio(&mut fallback);
            fallback.spawn().map_err(|fallback_err| {
                anyhow::anyhow!("failed to start Claude login: {fallback_err}")
            })
        }
        Err(err) => Err(anyhow::anyhow!("failed to start Claude login: {err}")),
    }
}

pub(crate) fn launch_grok_login(binary: &Path) -> anyhow::Result<std::process::Child> {
    let mut command = Command::new(binary);
    command.arg("login");
    configure_browser_stdio(&mut command);
    command
        .spawn()
        .map_err(|err| anyhow::anyhow!("failed to start Grok login: {err}"))
}

fn configure_claude_environment(command: &mut Command, config_dir: Option<&Path>) {
    if let Some(config_dir) = config_dir {
        command
            .env("CLAUDE_CONFIG_DIR", config_dir)
            .env_remove("CLAUDE_SECURESTORAGE_CONFIG_DIR");
    }
}

fn configure_browser_stdio(command: &mut Command) {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
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
}
