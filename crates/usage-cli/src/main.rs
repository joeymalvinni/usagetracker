use std::path::PathBuf;
use std::{env, io};

use anyhow::{bail, Context};
use clap::{Parser, Subcommand, ValueEnum};
use io::IsTerminal;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};
use usage_core::{
    default_socket_path, Account, ApiRequest, ApiResponse, ConfigResponse, ProviderHealth,
    ProviderId, UsageSnapshot,
};

mod render;

#[derive(Debug, Parser)]
#[command(name = "usage")]
struct Args {
    #[arg(long, env = "USAGE_TRACKER_SOCKET")]
    socket_path: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = OutputStyle::Dashboard)]
    style: OutputStyle,
    #[arg(long, value_enum, default_value_t = ColorChoice::Auto)]
    color: ColorChoice,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum OutputStyle {
    Dashboard,
    Compact,
    Json,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum ColorChoice {
    Auto,
    Always,
    Never,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Summary of systems operations.
    Status,
    /// Consumption dashboard.
    Usage,
    /// Refresh providers.
    Refresh {
        #[arg(long = "provider")]
        providers: Vec<String>,
    },
    /// Daemon and provider health.
    Health,
    /// Identity mapping.
    Accounts,
    /// Daemon config.
    Config,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let command = args.command.unwrap_or(Command::Usage);
    let request = match &command {
        Command::Status | Command::Usage | Command::Health => ApiRequest::GetUsage,
        Command::Refresh { providers } => ApiRequest::Refresh {
            providers: (!providers.is_empty()).then(|| {
                providers
                    .iter()
                    .cloned()
                    .map(ProviderId::new)
                    .collect::<Vec<_>>()
            }),
        },
        Command::Accounts => ApiRequest::GetAccounts,
        Command::Config => ApiRequest::GetConfig,
    };

    let socket_path = args
        .socket_path
        .or_else(default_socket_path)
        .context("failed to resolve ~/.usagetracker/usage.sock")?;
    if matches!(command, Command::Status | Command::Health) {
        let snapshots = fetch_usage(&socket_path).await?;
        let accounts = fetch_accounts(&socket_path).await?;
        let health = fetch_health(&socket_path).await?;
        let config = fetch_config(&socket_path).await?;
        let status = render::StatusView::from_parts(
            socket_path.display().to_string(),
            &snapshots,
            &accounts,
            &health,
            &config,
        );
        if args.style == OutputStyle::Json {
            println!("{}", serde_json::to_string(&status)?);
        } else {
            println!(
                "{}",
                render::render_status(&status, args.style, color_enabled(args.color))
            );
        }
        return Ok(());
    }

    let response = send_request(&socket_path, &request).await?;
    if args.style == OutputStyle::Json {
        println!("{}", serde_json::to_string(&response)?);
    } else {
        let color = color_enabled(args.color);
        match (command, response) {
            (
                Command::Usage,
                ApiResponse::Usage {
                    snapshots,
                    forecasts,
                },
            ) => {
                let accounts = fetch_accounts(&socket_path).await?;
                let config = fetch_config(&socket_path).await?;
                let snapshots = default_visible_snapshots(snapshots, &config);
                let accounts = default_visible_accounts(accounts, &config);
                println!(
                    "{}",
                    render::render_usage(&snapshots, &forecasts, &accounts, args.style, color)
                );
            }
            (Command::Accounts, ApiResponse::Accounts { accounts }) => {
                let snapshots = fetch_usage(&socket_path).await?;
                println!(
                    "{}",
                    render::render_accounts(&accounts, &snapshots, args.style, color)
                );
            }
            (Command::Config, ApiResponse::Config { config }) => {
                println!("{}", render::render_config(&config, args.style, color));
            }
            (
                Command::Refresh { .. },
                ApiResponse::Refresh {
                    started_at,
                    finished_at,
                    provider_results,
                },
            ) => {
                let accounts = fetch_accounts(&socket_path).await?;
                let snapshots = fetch_usage(&socket_path).await?;
                println!(
                    "{}",
                    render::render_refresh(
                        started_at,
                        finished_at,
                        &provider_results,
                        &accounts,
                        &snapshots,
                        args.style,
                        color
                    )
                );
            }
            (_, ApiResponse::Error { error }) => {
                bail!("daemon returned {}: {}", error.code, error.message);
            }
            (_, other) => bail!("daemon returned unexpected response: {other:?}"),
        }
    }
    Ok(())
}

async fn fetch_accounts(socket_path: &PathBuf) -> anyhow::Result<Vec<Account>> {
    match send_request(socket_path, &ApiRequest::GetAccounts).await? {
        ApiResponse::Accounts { accounts } => Ok(accounts),
        ApiResponse::Error { error } => {
            bail!("daemon returned {}: {}", error.code, error.message);
        }
        other => bail!("daemon returned unexpected accounts response: {other:?}"),
    }
}

async fn fetch_usage(socket_path: &PathBuf) -> anyhow::Result<Vec<UsageSnapshot>> {
    match send_request(socket_path, &ApiRequest::GetUsage).await? {
        ApiResponse::Usage { snapshots, .. } => Ok(snapshots),
        ApiResponse::Error { error } => {
            bail!("daemon returned {}: {}", error.code, error.message);
        }
        other => bail!("daemon returned unexpected usage response: {other:?}"),
    }
}

async fn fetch_health(socket_path: &PathBuf) -> anyhow::Result<Vec<ProviderHealth>> {
    match send_request(socket_path, &ApiRequest::GetProviderHealth).await? {
        ApiResponse::ProviderHealth { health } => Ok(health),
        ApiResponse::Error { error } => {
            bail!("daemon returned {}: {}", error.code, error.message);
        }
        other => bail!("daemon returned unexpected health response: {other:?}"),
    }
}

async fn fetch_config(socket_path: &PathBuf) -> anyhow::Result<ConfigResponse> {
    match send_request(socket_path, &ApiRequest::GetConfig).await? {
        ApiResponse::Config { config } => Ok(config),
        ApiResponse::Error { error } => {
            bail!("daemon returned {}: {}", error.code, error.message);
        }
        other => bail!("daemon returned unexpected config response: {other:?}"),
    }
}

fn color_enabled(choice: ColorChoice) -> bool {
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => env::var_os("NO_COLOR").is_none() && io::stdout().is_terminal(),
    }
}

fn default_visible_snapshots(
    snapshots: Vec<UsageSnapshot>,
    config: &ConfigResponse,
) -> Vec<UsageSnapshot> {
    snapshots
        .into_iter()
        .filter(|snapshot| default_visible_provider(snapshot.provider_id.as_str(), config))
        .collect()
}

fn default_visible_accounts(accounts: Vec<Account>, config: &ConfigResponse) -> Vec<Account> {
    accounts
        .into_iter()
        .filter(|account| default_visible_provider(account.provider_id.as_str(), config))
        .filter(|account| !account.hidden)
        .collect()
}

fn default_visible_provider(provider_id: &str, config: &ConfigResponse) -> bool {
    if config
        .providers
        .get(provider_id)
        .is_some_and(|provider| provider.enabled)
    {
        return true;
    }
    config
        .enabled_providers
        .iter()
        .any(|id| id.as_str() == provider_id)
}

async fn send_request(socket_path: &PathBuf, request: &ApiRequest) -> anyhow::Result<ApiResponse> {
    let stream = UnixStream::connect(socket_path).await.with_context(|| {
        format!(
            "failed to connect to daemon socket {}",
            socket_path.display()
        )
    })?;
    let (reader, mut writer) = stream.into_split();
    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await?;

    let mut lines = BufReader::new(reader).lines();
    let response = lines
        .next_line()
        .await?
        .context("daemon closed connection without a response")?;
    serde_json::from_str(&response).context("daemon returned invalid response JSON")
}
