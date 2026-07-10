mod config;
mod daemon;
mod health;
mod local_logs;
mod notifications;
mod polling;
mod providers;
mod server;
mod storage;

use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

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
}

#[tokio::main(worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    init_tracing(&args.log_level)?;

    let config = Config::load(args.config, args.db_path, args.socket_path)
        .context("failed to load daemon config")?;
    tracing::info!(
        config = %config.paths.config.display(),
        db = %config.paths.db.display(),
        socket = %config.paths.socket.display(),
        poll_interval_seconds = config.poll_interval_seconds,
        notifications_enabled = config.notifications.enabled,
        debug_capture_raw_payloads = config.debug_capture_raw_payloads,
        enabled_providers = ?config.enabled_provider_ids(),
        "daemon config loaded"
    );
    Daemon::new(config).await?.run().await
}

fn init_tracing(log_level: &str) -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env().or_else(|_| EnvFilter::try_new(log_level))?;
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .init();
    Ok(())
}
