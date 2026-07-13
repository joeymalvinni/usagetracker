mod config;
mod daemon;
mod dashboard;
mod fixtures;
mod forecast;
mod health;
mod keychain;
mod local_logs;
mod notifications;
mod polling;
mod providers;
mod runtime;
mod server;
mod storage;

use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
use usage_core::{default_app_dir, APP_HOME_ENV, CONFIG_FILE, DB_FILE, SOCKET_FILE};

use crate::{config::Config, daemon::Daemon};

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, default_value_t = true)]
    foreground: bool,
    #[arg(long, env = "USAGE_TRACKER_LOG_LEVEL", default_value = "info")]
    log_level: String,
    #[arg(long, env = "USAGE_TRACKER_CONFIG")]
    config: Option<PathBuf>,
    #[arg(long, env = "USAGE_TRACKER_DB")]
    db_path: Option<PathBuf>,
    #[arg(long, env = "USAGE_TRACKER_SOCKET")]
    socket_path: Option<PathBuf>,
    /// Run against a reset, synthetic development database.
    #[arg(long, env = "USAGE_TRACKER_FIXTURE", value_enum)]
    fixture: Option<fixtures::FixtureScenario>,
    /// Internal one-operation Keychain subprocess.
    #[arg(long, hide = true)]
    keychain_helper: bool,
}

#[tokio::main(worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if args.keychain_helper {
        return keychain::run_helper();
    }
    init_tracing(&args.log_level)?;

    let (config_path, db_path, socket_path) = fixture_path_overrides(&args)?;
    let mut config = match args.fixture {
        Some(_) => Config::load_fixture(config_path, db_path, socket_path),
        None => Config::load(config_path, db_path, socket_path),
    }
    .context("failed to load daemon config")?;
    if let Some(scenario) = args.fixture {
        fixtures::reset_database(&config.paths.db).context("failed to reset fixture database")?;
        for provider in config.providers.values_mut() {
            provider.enabled = true;
        }
        config.notifications.enabled = true;
        config.persist()?;
        tracing::info!(
            scenario = scenario.as_str(),
            "development fixture mode enabled"
        );
    }
    tracing::info!(
        config = %config.paths.config.display(),
        db = %config.paths.db.display(),
        socket = %config.paths.socket.display(),
        poll_interval_seconds = config.poll_interval_seconds,
        notifications_enabled = config.notifications.enabled,
        enabled_providers = ?config.enabled_provider_ids(),
        "daemon config loaded"
    );
    match args.fixture {
        Some(scenario) => Daemon::new_fixture(config, scenario).await?.run().await,
        None => Daemon::new(config).await?.run().await,
    }
}

fn fixture_path_overrides(
    args: &Args,
) -> anyhow::Result<(Option<PathBuf>, Option<PathBuf>, Option<PathBuf>)> {
    let Some(scenario) = args.fixture else {
        return Ok((
            args.config.clone(),
            args.db_path.clone(),
            args.socket_path.clone(),
        ));
    };
    if args.config.is_some() && args.db_path.is_some() && args.socket_path.is_some() {
        return Ok((
            args.config.clone(),
            args.db_path.clone(),
            args.socket_path.clone(),
        ));
    }

    let home_is_overridden = std::env::var_os(APP_HOME_ENV).is_some_and(|value| !value.is_empty());
    let base = default_app_dir().context("failed to resolve fixture directory")?;
    let root = if home_is_overridden {
        base
    } else {
        base.join("fixtures").join(scenario.as_str())
    };
    Ok((
        args.config.clone().or_else(|| Some(root.join(CONFIG_FILE))),
        args.db_path.clone().or_else(|| Some(root.join(DB_FILE))),
        args.socket_path
            .clone()
            .or_else(|| Some(root.join(SOCKET_FILE))),
    ))
}

fn init_tracing(log_level: &str) -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env().or_else(|_| EnvFilter::try_new(log_level))?;
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .init();
    Ok(())
}
