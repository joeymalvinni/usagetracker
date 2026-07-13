use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::{env, io};

use anyhow::{bail, Context};
use clap::{Args as ClapArgs, Parser, Subcommand, ValueEnum};
use io::{IsTerminal, Write as _};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::{sleep, timeout, Duration, Instant},
};
use usage_core::{
    default_socket_path, Account, AccountId, ApiRequest, ApiResponse, ConfigResponse,
    NotificationConfig, ProviderId, ProviderToggle, RefreshJob, RefreshJobStatus, RequestEnvelope,
    ResponseEnvelope, StateSnapshot, UsageDashboardSummary, UsageForecast, UsageSnapshot,
    UsageWindowProvenance, API_VERSION, MAX_RESPONSE_BYTES,
};

mod render;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const WRITE_TIMEOUT: Duration = Duration::from_secs(3);
const READ_TIMEOUT: Duration = Duration::from_secs(10);
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(15);
const REFRESH_WAIT_TIMEOUT: Duration = Duration::from_secs(2 * 60);
const REFRESH_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Parser)]
#[command(
    name = "usage",
    version,
    about = "Inspect and manage UsageTracker",
    arg_required_else_help = false
)]
struct Cli {
    /// Override the daemon Unix socket.
    #[arg(long, env = "USAGE_TRACKER_SOCKET", global = true)]
    socket_path: Option<PathBuf>,
    /// Human dashboard or machine-readable JSON.
    #[arg(long, value_enum, default_value_t = OutputStyle::Dashboard, global = true)]
    style: OutputStyle,
    /// Colorize human-readable output.
    #[arg(long, value_enum, default_value_t = ColorChoice::Auto, global = true)]
    color: ColorChoice,
    /// Maximum dashboard width in columns.
    #[arg(
        long,
        env = "USAGE_TRACKER_MAX_WIDTH",
        default_value_t = 80,
        value_parser = parse_max_width,
        global = true
    )]
    max_width: usize,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum OutputStyle {
    Dashboard,
    Json,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum ColorChoice {
    Auto,
    Always,
    Never,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum Switch {
    On,
    Off,
}

fn parse_max_width(value: &str) -> Result<usize, String> {
    let width = value
        .parse::<usize>()
        .map_err(|_| "width must be a positive integer".to_string())?;
    if width < 60 {
        return Err("width must be at least 60 columns".to_string());
    }
    Ok(width)
}

impl Switch {
    fn enabled(self) -> bool {
        self == Self::On
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Show daemon, provider, account, and freshness status.
    #[command(alias = "health")]
    Status,
    /// Show the usage dashboard (the default command).
    Usage(UsageArgs),
    /// Poll providers immediately.
    Refresh {
        /// Refresh only these provider IDs; repeat for more than one.
        #[arg(long = "provider", short = 'p')]
        providers: Vec<String>,
    },
    /// List and manage provider accounts.
    Accounts(AccountsArgs),
    /// Inspect and configure providers.
    Providers(ProvidersArgs),
    /// Inspect and edit daemon configuration.
    Config(ConfigArgs),
}

#[derive(Debug, Default, ClapArgs)]
struct UsageArgs {
    /// Include only these provider IDs; repeat for more than one.
    #[arg(long = "provider", short = 'p')]
    providers: Vec<String>,
    /// Include only these account IDs; repeat for more than one.
    #[arg(long = "account", short = 'a')]
    accounts: Vec<String>,
    /// Include stored usage for providers that are currently disabled.
    #[arg(long)]
    all_providers: bool,
    /// Show extra per-window detail, credits, forecast, and identity.
    #[arg(long, short = 'd')]
    details: bool,
}

#[derive(Debug, ClapArgs)]
struct AccountsArgs {
    #[command(subcommand)]
    command: Option<AccountCommand>,
}

#[derive(Debug, Subcommand)]
enum AccountCommand {
    /// List accounts and their stable IDs.
    List {
        /// Include only this provider.
        #[arg(long, short = 'p')]
        provider: Option<String>,
        /// Hide removed and hidden accounts from the listing.
        #[arg(long)]
        active: bool,
        /// Show profile and external-ID columns.
        #[arg(long, short = 'v')]
        verbose: bool,
    },
    /// Create a separate browser profile for a provider account.
    Add {
        provider: String,
        #[arg(long)]
        name: Option<String>,
    },
    /// Set an account's display name.
    Rename { account: String, name: String },
    /// Hide an account from usage views without stopping collection.
    Hide { account: String },
    /// Make a hidden account visible again.
    Show { account: String },
    /// Resume collection for an account.
    Enable { account: String },
    /// Pause collection for an account while keeping its history visible.
    Disable { account: String },
    /// Remove an account from collection while retaining usage history.
    Remove { account: String },
    /// Permanently delete an account and its usage history.
    Delete {
        account: String,
        /// Confirm permanent deletion.
        #[arg(long)]
        yes: bool,
    },
    /// Launch the provider using this account's isolated profile.
    Launch { account: String },
}

#[derive(Debug, ClapArgs)]
struct ProvidersArgs {
    #[command(subcommand)]
    command: Option<ProviderCommand>,
}

#[derive(Debug, Subcommand)]
enum ProviderCommand {
    /// List provider enablement.
    List,
    /// Enable collection for a provider.
    Enable { provider: String },
    /// Disable collection for a provider.
    Disable { provider: String },
    /// Show discovered profiles and workspace choices.
    Setup { provider: String },
    /// Select a provider workspace.
    Workspace {
        provider: String,
        /// Workspace ID, such as wrk_...; omit when using --automatic.
        workspace: Option<String>,
        /// Clear the explicit workspace and return to automatic discovery.
        #[arg(long, conflicts_with = "workspace")]
        automatic: bool,
    },
    /// Re-authenticate a provider, optionally for one account.
    Repair {
        provider: String,
        #[arg(long, short = 'a')]
        account: Option<String>,
    },
}

#[derive(Debug, ClapArgs)]
struct ConfigArgs {
    #[command(subcommand)]
    command: Option<ConfigCommand>,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Show the effective daemon configuration.
    Show,
    /// Update live daemon settings and persist them.
    Set {
        /// Polling interval in seconds.
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        poll_interval: Option<u64>,
        /// Enable or disable desktop notifications.
        #[arg(long, value_enum)]
        notifications: Option<Switch>,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let socket_path = cli
        .socket_path
        .or_else(default_socket_path)
        .context("failed to resolve ~/.usagetracker/usage.sock")?;
    let style = cli.style;
    let color = style != OutputStyle::Json && color_enabled(cli.color);
    let max_width = cli.max_width;
    match cli.command.unwrap_or(Command::Usage(UsageArgs::default())) {
        Command::Status => run_status(&socket_path, style, color).await,
        Command::Usage(args) => run_usage(&socket_path, args, style, color, max_width).await,
        Command::Refresh { providers } => run_refresh(&socket_path, providers, style, color).await,
        Command::Accounts(args) => run_accounts(&socket_path, args, style, color).await,
        Command::Providers(args) => run_providers(&socket_path, args, style, color).await,
        Command::Config(args) => run_config(&socket_path, args, style, color).await,
    }
}

async fn run_status(socket: &Path, style: OutputStyle, color: bool) -> anyhow::Result<()> {
    let state = state_from_response(request(socket, ApiRequest::GetState).await?)?;
    let status = render::StatusView::from_parts(
        socket.display().to_string(),
        &state.snapshots,
        &state.accounts,
        &state.health,
        &state.config,
    );
    if style == OutputStyle::Json {
        print_json(&status)
    } else {
        println!("{}", render::render_status(&status, style, color));
        Ok(())
    }
}

fn state_from_response(response: ApiResponse) -> anyhow::Result<StateSnapshot> {
    match response {
        ApiResponse::State { state } => Ok(state),
        other => unexpected("state", other),
    }
}

#[derive(Debug)]
struct UsageFetchPlan {
    requests: Vec<ApiRequest>,
    needs_accounts: bool,
    needs_config: bool,
}

fn usage_fetch_plan(style: OutputStyle, all_providers: bool) -> UsageFetchPlan {
    let needs_accounts = style != OutputStyle::Json;
    let needs_config = !all_providers;
    let mut requests = Vec::with_capacity(1 + needs_accounts as usize + needs_config as usize);
    requests.push(ApiRequest::GetUsage);
    if needs_accounts {
        requests.push(ApiRequest::GetAccounts);
    }
    if needs_config {
        requests.push(ApiRequest::GetConfig);
    }
    UsageFetchPlan {
        requests,
        needs_accounts,
        needs_config,
    }
}

async fn run_usage(
    socket: &Path,
    args: UsageArgs,
    style: OutputStyle,
    color: bool,
    max_width: usize,
) -> anyhow::Result<()> {
    let UsageFetchPlan {
        requests,
        needs_accounts,
        needs_config,
    } = usage_fetch_plan(style, args.all_providers);
    let mut responses = request_batch(socket, requests).await?.into_iter();
    let (mut snapshots, mut forecasts, mut window_provenance, dashboard) =
        usage_from_response(next_response(&mut responses, "usage")?)?;
    let mut accounts = if needs_accounts {
        accounts_from_response(next_response(&mut responses, "accounts")?)?
    } else {
        Vec::new()
    };
    let config = if needs_config {
        Some(config_from_response(next_response(
            &mut responses,
            "config",
        )?)?)
    } else {
        None
    };

    if let Some(config) = &config {
        snapshots.retain(|row| default_visible_provider(row.provider_id.as_str(), config));
        accounts.retain(|row| default_visible_provider(row.provider_id.as_str(), config));
    }
    accounts.retain(|account| !account.hidden);
    if !args.providers.is_empty() {
        snapshots.retain(|row| contains_id(&args.providers, row.provider_id.as_str()));
        forecasts.retain(|row| contains_id(&args.providers, row.provider_id.as_str()));
        window_provenance.retain(|row| contains_id(&args.providers, row.provider_id.as_str()));
        accounts.retain(|row| contains_id(&args.providers, row.provider_id.as_str()));
    }
    if !args.accounts.is_empty() {
        snapshots.retain(|row| contains_id(&args.accounts, row.account_id.as_str()));
        forecasts.retain(|row| contains_id(&args.accounts, row.account_id.as_str()));
        window_provenance.retain(|row| contains_id(&args.accounts, row.account_id.as_str()));
        accounts.retain(|row| contains_id(&args.accounts, row.id.as_str()));
    }
    retain_forecasts_for_snapshots(&mut forecasts, &snapshots);
    let dashboard = dashboard_for_snapshots(dashboard, &snapshots);

    if style == OutputStyle::Json {
        print_json(&ApiResponse::Usage {
            snapshots,
            dashboard,
            forecasts,
            window_provenance,
        })
    } else {
        println!(
            "{}",
            render::render_usage_with_summary(
                &snapshots,
                &forecasts,
                &accounts,
                Some(&dashboard),
                render::UsageRenderOptions {
                    style,
                    color,
                    width: render::output_width(max_width),
                    details: args.details,
                },
            )
        );
        Ok(())
    }
}

async fn run_refresh(
    socket: &Path,
    providers: Vec<String>,
    style: OutputStyle,
    color: bool,
) -> anyhow::Result<()> {
    let refresh_request = ApiRequest::Refresh {
        providers: (!providers.is_empty()).then(|| {
            providers
                .into_iter()
                .map(ProviderId::new)
                .collect::<Vec<_>>()
        }),
    };
    let started = request(socket, refresh_request).await?;
    let job = match started {
        ApiResponse::RefreshStarted { job, .. } => wait_for_refresh(socket, job).await?,
        other => return unexpected("refresh_started", other),
    };
    if style == OutputStyle::Json {
        return print_json(&ApiResponse::RefreshJob { job });
    }

    let mut responses = request_batch(socket, [ApiRequest::GetAccounts, ApiRequest::GetUsage])
        .await?
        .into_iter();
    let accounts = accounts_from_response(next_response(&mut responses, "accounts")?)?;
    let snapshots = usage_from_response(next_response(&mut responses, "usage")?)?.0;
    let started_at = job.started_at.unwrap_or(job.created_at);
    let finished_at = job.finished_at.unwrap_or_else(chrono::Utc::now);
    println!(
        "{}",
        render::render_refresh(
            started_at,
            finished_at,
            &job.provider_results,
            &accounts,
            &snapshots,
            style,
            color,
        )
    );
    Ok(())
}

async fn wait_for_refresh(socket: &Path, mut job: RefreshJob) -> anyhow::Result<RefreshJob> {
    let deadline = Instant::now() + REFRESH_WAIT_TIMEOUT;
    while !job.status.is_terminal() {
        if Instant::now() >= deadline {
            bail!(
                "refresh job {} did not finish within {} seconds",
                job.id,
                REFRESH_WAIT_TIMEOUT.as_secs()
            );
        }
        sleep(REFRESH_POLL_INTERVAL).await;
        job = match request(
            socket,
            ApiRequest::GetRefreshJob {
                job_id: job.id.clone(),
            },
        )
        .await?
        {
            ApiResponse::RefreshJob { job } => job,
            other => return unexpected("refresh_job", other),
        };
    }
    if job.status == RefreshJobStatus::Failed {
        bail!(
            "refresh job {} failed: {}",
            job.id,
            job.failure_message.as_deref().unwrap_or("unknown failure")
        );
    }
    Ok(job)
}

async fn run_accounts(
    socket: &Path,
    args: AccountsArgs,
    style: OutputStyle,
    color: bool,
) -> anyhow::Result<()> {
    match args.command.unwrap_or(AccountCommand::List {
        provider: None,
        active: false,
        verbose: false,
    }) {
        AccountCommand::List {
            provider,
            active,
            verbose,
        } => {
            let (mut accounts, snapshots) = if style == OutputStyle::Json {
                (fetch_accounts(socket).await?, Vec::new())
            } else {
                let mut responses =
                    request_batch(socket, [ApiRequest::GetAccounts, ApiRequest::GetUsage])
                        .await?
                        .into_iter();
                (
                    accounts_from_response(next_response(&mut responses, "accounts")?)?,
                    usage_from_response(next_response(&mut responses, "usage")?)?.0,
                )
            };
            if let Some(provider) = provider {
                accounts.retain(|account| account.provider_id.as_str() == provider);
            }
            if active {
                accounts.retain(|account| !account.hidden && account.collection_enabled);
            }
            if style == OutputStyle::Json {
                print_json(&ApiResponse::Accounts { accounts })
            } else {
                println!(
                    "{}",
                    render::render_accounts(&accounts, &snapshots, style, color, verbose,)
                );
                Ok(())
            }
        }
        AccountCommand::Add { provider, name } => {
            let response = request(
                socket,
                ApiRequest::AddProviderAccount {
                    provider_id: ProviderId::new(provider),
                    display_name: name,
                },
            )
            .await?;
            print_action_response(response, style, color)
        }
        AccountCommand::Rename { account, name } => {
            update_account(socket, account, Some(name), None, None, style, color).await
        }
        AccountCommand::Hide { account } => {
            update_account(socket, account, None, Some(true), None, style, color).await
        }
        AccountCommand::Show { account } => {
            update_account(socket, account, None, Some(false), None, style, color).await
        }
        AccountCommand::Enable { account } => {
            update_account(socket, account, None, Some(false), Some(true), style, color).await
        }
        AccountCommand::Disable { account } => {
            update_account(socket, account, None, None, Some(false), style, color).await
        }
        AccountCommand::Remove { account } => {
            let response = request(
                socket,
                ApiRequest::RemoveAccount {
                    account_id: AccountId::new(account),
                },
            )
            .await?;
            print_action_response(response, style, color)
        }
        AccountCommand::Delete { account, yes } => {
            if !yes {
                bail!("permanent deletion requires --yes; use `accounts remove` to keep history");
            }
            let response = request(
                socket,
                ApiRequest::DeleteAccount {
                    account_id: AccountId::new(account),
                },
            )
            .await?;
            print_action_response(response, style, color)
        }
        AccountCommand::Launch { account } => {
            let response = request(
                socket,
                ApiRequest::LaunchProviderAccount {
                    account_id: AccountId::new(account),
                },
            )
            .await?;
            print_action_response(response, style, color)
        }
    }
}

async fn update_account(
    socket: &Path,
    account: String,
    display_name: Option<String>,
    hidden: Option<bool>,
    collection_enabled: Option<bool>,
    style: OutputStyle,
    color: bool,
) -> anyhow::Result<()> {
    let response = request(
        socket,
        ApiRequest::UpdateAccount {
            account_id: AccountId::new(account),
            display_name,
            hidden,
            collection_enabled,
        },
    )
    .await?;
    print_action_response(response, style, color)
}

async fn run_providers(
    socket: &Path,
    args: ProvidersArgs,
    style: OutputStyle,
    color: bool,
) -> anyhow::Result<()> {
    match args.command.unwrap_or(ProviderCommand::List) {
        ProviderCommand::List => print_config(fetch_config(socket).await?, style, color),
        ProviderCommand::Enable { provider } => {
            set_provider_enabled(socket, provider, true, style, color).await
        }
        ProviderCommand::Disable { provider } => {
            set_provider_enabled(socket, provider, false, style, color).await
        }
        ProviderCommand::Setup { provider } => {
            let response = request(
                socket,
                ApiRequest::GetProviderSetup {
                    provider_id: ProviderId::new(provider),
                },
            )
            .await?;
            print_action_response(response, style, color)
        }
        ProviderCommand::Workspace {
            provider,
            workspace,
            automatic,
        } => {
            if workspace.is_none() && !automatic {
                bail!("provide a workspace ID or pass --automatic");
            }
            let response = request(
                socket,
                ApiRequest::UpdateProviderSetup {
                    provider_id: ProviderId::new(provider),
                    workspace_id: workspace,
                },
            )
            .await?;
            print_action_response(response, style, color)
        }
        ProviderCommand::Repair { provider, account } => {
            let response = request(
                socket,
                ApiRequest::RepairProvider {
                    provider_id: ProviderId::new(provider),
                    account_id: account.map(AccountId::new),
                },
            )
            .await?;
            print_action_response(response, style, color)
        }
    }
}

async fn set_provider_enabled(
    socket: &Path,
    provider: String,
    enabled: bool,
    style: OutputStyle,
    color: bool,
) -> anyhow::Result<()> {
    let mut providers = BTreeMap::new();
    providers.insert(provider, ProviderToggle { enabled });
    let response = request(
        socket,
        ApiRequest::UpdateConfig {
            poll_interval_seconds: None,
            providers: Some(providers),
            notifications: None,
        },
    )
    .await?;
    match response {
        ApiResponse::Config { config } => print_config(config, style, color),
        other => unexpected("config", other),
    }
}

async fn run_config(
    socket: &Path,
    args: ConfigArgs,
    style: OutputStyle,
    color: bool,
) -> anyhow::Result<()> {
    match args.command.unwrap_or(ConfigCommand::Show) {
        ConfigCommand::Show => print_config(fetch_config(socket).await?, style, color),
        ConfigCommand::Set {
            poll_interval,
            notifications,
        } => {
            if poll_interval.is_none() && notifications.is_none() {
                bail!("config set requires --poll-interval or --notifications");
            }
            let notifications = match notifications {
                Some(value) => Some(notification_config_with_enabled(
                    fetch_config(socket).await?.notifications,
                    value,
                )),
                None => None,
            };
            let response = request(
                socket,
                ApiRequest::UpdateConfig {
                    poll_interval_seconds: poll_interval,
                    providers: None,
                    notifications,
                },
            )
            .await?;
            match response {
                ApiResponse::Config { config } => print_config(config, style, color),
                other => unexpected("config", other),
            }
        }
    }
}

fn notification_config_with_enabled(
    mut notifications: NotificationConfig,
    enabled: Switch,
) -> NotificationConfig {
    notifications.enabled = enabled.enabled();
    notifications
}

fn dashboard_for_snapshots(
    mut dashboard: UsageDashboardSummary,
    snapshots: &[UsageSnapshot],
) -> UsageDashboardSummary {
    let selected = snapshots
        .iter()
        .map(|snapshot| (&snapshot.provider_id, &snapshot.account_id))
        .collect::<HashSet<_>>();
    dashboard
        .accounts
        .retain(|account| selected.contains(&(&account.provider_id, &account.account_id)));
    usage_core::aggregate_usage_dashboard(dashboard.accounts)
}

fn print_config(config: ConfigResponse, style: OutputStyle, color: bool) -> anyhow::Result<()> {
    if style == OutputStyle::Json {
        print_json(&ApiResponse::Config { config })
    } else {
        println!("{}", render::render_config(&config, style, color));
        Ok(())
    }
}

fn print_action_response(
    response: ApiResponse,
    style: OutputStyle,
    color: bool,
) -> anyhow::Result<()> {
    if style == OutputStyle::Json {
        return print_json(&response);
    }
    match response {
        ApiResponse::AddProviderAccount { account } => {
            println!("{}", render::render_added_account(&account, style, color));
        }
        ApiResponse::Account { account } => {
            println!("{}", render::render_account_action(&account, style, color));
        }
        ApiResponse::AccountDeleted { account_id } => {
            println!("Deleted account {account_id} and its usage history.");
        }
        ApiResponse::ProviderSetup { setup } => {
            println!("{}", render::render_provider_setup(&setup, style, color));
        }
        ApiResponse::ProviderAction { action } => {
            println!("{}", render::render_provider_action(&action, color));
        }
        other => return unexpected("action", other),
    }
    Ok(())
}

async fn fetch_accounts(socket: &Path) -> anyhow::Result<Vec<Account>> {
    accounts_from_response(request(socket, ApiRequest::GetAccounts).await?)
}

fn accounts_from_response(response: ApiResponse) -> anyhow::Result<Vec<Account>> {
    match response {
        ApiResponse::Accounts { accounts } => Ok(accounts),
        other => unexpected("accounts", other),
    }
}

type UsagePayload = (
    Vec<UsageSnapshot>,
    Vec<UsageForecast>,
    Vec<UsageWindowProvenance>,
    UsageDashboardSummary,
);

fn usage_from_response(response: ApiResponse) -> anyhow::Result<UsagePayload> {
    match response {
        ApiResponse::Usage {
            snapshots,
            dashboard,
            forecasts,
            window_provenance,
        } => Ok((snapshots, forecasts, window_provenance, dashboard)),
        other => unexpected("usage", other),
    }
}

async fn fetch_config(socket: &Path) -> anyhow::Result<ConfigResponse> {
    config_from_response(request(socket, ApiRequest::GetConfig).await?)
}

fn config_from_response(response: ApiResponse) -> anyhow::Result<ConfigResponse> {
    match response {
        ApiResponse::Config { config } => Ok(config),
        other => unexpected("config", other),
    }
}

fn color_enabled(choice: ColorChoice) -> bool {
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => env::var_os("NO_COLOR").is_none() && io::stdout().is_terminal(),
    }
}

fn default_visible_provider(provider_id: &str, config: &ConfigResponse) -> bool {
    config
        .providers
        .get(provider_id)
        .is_some_and(|provider| provider.enabled)
}

fn contains_id(values: &[String], id: &str) -> bool {
    values.iter().any(|value| value == id)
}

fn retain_forecasts_for_snapshots(forecasts: &mut Vec<UsageForecast>, snapshots: &[UsageSnapshot]) {
    let snapshot_accounts = snapshots
        .iter()
        .map(|snapshot| (snapshot.provider_id.as_str(), snapshot.account_id.as_str()))
        .collect::<HashSet<_>>();
    forecasts.retain(|forecast| {
        snapshot_accounts.contains(&(forecast.provider_id.as_str(), forecast.account_id.as_str()))
    });
}

fn print_json(value: &impl serde::Serialize) -> anyhow::Result<()> {
    let stdout = io::stdout();
    let mut output = io::BufWriter::new(stdout.lock());
    serde_json::to_writer(&mut output, value)?;
    writeln!(output)?;
    output.flush()?;
    Ok(())
}

fn unexpected<T>(expected: &str, response: ApiResponse) -> anyhow::Result<T> {
    match response {
        ApiResponse::Error { error } => {
            bail!("daemon returned {}: {}", error.code, error.message)
        }
        other => bail!("daemon returned unexpected {expected} response: {other:?}"),
    }
}

async fn request(socket: &Path, request: ApiRequest) -> anyhow::Result<ApiResponse> {
    request_batch(socket, [request])
        .await?
        .pop()
        .context("daemon closed connection without a response")
}

fn next_response(
    responses: &mut impl Iterator<Item = ApiResponse>,
    expected: &str,
) -> anyhow::Result<ApiResponse> {
    responses
        .next()
        .with_context(|| format!("daemon omitted the {expected} response"))
}

async fn request_batch(
    socket: &Path,
    requests: impl IntoIterator<Item = ApiRequest>,
) -> anyhow::Result<Vec<ApiResponse>> {
    let requests = requests.into_iter().collect::<Vec<_>>();
    timeout(EXCHANGE_TIMEOUT, request_batch_inner(socket, requests))
        .await
        .with_context(|| {
            format!(
                "daemon exchange exceeded the {}-second timeout",
                EXCHANGE_TIMEOUT.as_secs()
            )
        })?
}

async fn request_batch_inner(
    socket: &Path,
    requests: Vec<ApiRequest>,
) -> anyhow::Result<Vec<ApiResponse>> {
    let stream = timeout(CONNECT_TIMEOUT, UnixStream::connect(socket))
        .await
        .with_context(|| format!("timed out connecting to daemon socket {}", socket.display()))?
        .with_context(|| format!("failed to connect to daemon socket {}", socket.display()))?;
    let (reader, mut writer) = stream.into_split();
    let requests = requests.into_iter();
    let mut payload = Vec::with_capacity(requests.size_hint().0.saturating_mul(64));
    let mut request_count = 0;
    for request in requests {
        serde_json::to_writer(&mut payload, &RequestEnvelope::new(request))?;
        payload.push(b'\n');
        request_count += 1;
    }
    timeout(WRITE_TIMEOUT, writer.write_all(&payload))
        .await
        .context("timed out writing daemon request")??;

    let mut reader = BufReader::new(reader);
    let mut line = Vec::with_capacity(32 * 1024);
    let mut responses = Vec::with_capacity(request_count);
    for response_index in 0..request_count {
        let has_response = timeout(READ_TIMEOUT, read_bounded_response(&mut reader, &mut line))
            .await
            .with_context(|| {
                format!(
                    "timed out reading daemon response {} of {}",
                    response_index + 1,
                    request_count
                )
            })??;
        if !has_response {
            bail!("daemon closed connection after {response_index} of {request_count} responses");
        }
        responses.push(decode_response(&line)?);
    }
    Ok(responses)
}

fn decode_response(line: &[u8]) -> anyhow::Result<ApiResponse> {
    let envelope: ResponseEnvelope =
        serde_json::from_slice(line).context("daemon returned invalid response JSON")?;
    if envelope.api_version != API_VERSION {
        bail!(
            "daemon protocol version {} is incompatible with CLI version {API_VERSION}",
            envelope.api_version
        );
    }
    Ok(envelope.response)
}

async fn read_bounded_response<R>(reader: &mut R, line: &mut Vec<u8>) -> io::Result<bool>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    line.clear();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(!line.is_empty());
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if line.len().saturating_add(take) > MAX_RESPONSE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("daemon response exceeds {MAX_RESPONSE_BYTES} bytes"),
            ));
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if line.last() == Some(&b'\n') {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };
    use tokio::net::UnixListener;
    use usage_core::{ApiErrorCode, ForecastConfidence, ForecastStatus};

    static NEXT_SOCKET_ID: AtomicU64 = AtomicU64::new(0);

    struct TestSocketPath(PathBuf);

    impl TestSocketPath {
        fn new() -> Self {
            let id = NEXT_SOCKET_ID.fetch_add(1, Ordering::Relaxed);
            Self(env::temp_dir().join(format!("usage-cli-test-{}-{id}.sock", std::process::id())))
        }

        fn as_path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestSocketPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn usage_request_names(plan: &UsageFetchPlan) -> Vec<&'static str> {
        plan.requests
            .iter()
            .map(|request| match request {
                ApiRequest::GetUsage => "usage",
                ApiRequest::GetAccounts => "accounts",
                ApiRequest::GetConfig => "config",
                other => panic!("unexpected usage request: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn parses_usage_filters() {
        let cli = Cli::try_parse_from([
            "usage",
            "usage",
            "--provider",
            "codex",
            "--account",
            "acct-1",
            "--all-providers",
        ])
        .unwrap();
        let Some(Command::Usage(args)) = cli.command else {
            panic!("expected usage command");
        };
        assert_eq!(args.providers, ["codex"]);
        assert_eq!(args.accounts, ["acct-1"]);
        assert!(args.all_providers);
    }

    #[test]
    fn parses_dashboard_max_width() {
        let cli = Cli::try_parse_from(["usage", "--max-width", "72"]).unwrap();

        assert_eq!(cli.max_width, 72);
    }

    #[test]
    fn rejects_dashboard_max_width_below_layout_minimum() {
        assert!(Cli::try_parse_from(["usage", "--max-width", "59"]).is_err());
    }

    #[test]
    fn rejects_removed_compact_style_with_supported_replacements() {
        let error = Cli::try_parse_from(["usage", "--style", "compact"]).unwrap_err();
        let message = error.to_string();

        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidValue);
        assert!(message.contains("dashboard"));
        assert!(message.contains("json"));
    }

    #[test]
    fn parses_account_management_commands() {
        let cli = Cli::try_parse_from([
            "usage",
            "accounts",
            "rename",
            "acct-1",
            "Work account",
            "--style",
            "json",
        ])
        .unwrap();
        assert_eq!(cli.style, OutputStyle::Json);
        assert!(matches!(
            cli.command,
            Some(Command::Accounts(AccountsArgs {
                command: Some(AccountCommand::Rename { .. })
            }))
        ));
    }

    #[test]
    fn parses_live_config_update() {
        let cli = Cli::try_parse_from([
            "usage",
            "config",
            "set",
            "--poll-interval",
            "120",
            "--notifications",
            "off",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Config(ConfigArgs {
                command: Some(ConfigCommand::Set {
                    poll_interval: Some(120),
                    notifications: Some(Switch::Off)
                })
            }))
        ));
    }

    #[test]
    fn notification_toggle_preserves_advanced_policy() {
        let policy = NotificationConfig {
            enabled: true,
            thresholds_percent_remaining: vec![33, 7],
            reset_alerts: false,
            predictive_alerts: true,
            cooldown_minutes: 42,
            quiet_hours: Some(usage_core::NotificationQuietHours {
                start_hour_local: 22,
                end_hour_local: 7,
            }),
            rules: vec![usage_core::NotificationRule {
                account_id: Some(AccountId::new("work")),
                window_id: Some("weekly".to_string()),
                enabled: Some(true),
                thresholds_percent_remaining: Some(vec![20, 5]),
                reset_alerts: Some(false),
                predictive_alerts: Some(true),
                snoozed_until: None,
            }],
        };

        let updated = notification_config_with_enabled(policy.clone(), Switch::Off);

        assert!(!updated.enabled);
        assert_eq!(updated.thresholds_percent_remaining, vec![33, 7]);
        assert!(!updated.reset_alerts);
        assert!(updated.predictive_alerts);
        assert_eq!(updated.cooldown_minutes, 42);
        assert_eq!(updated.quiet_hours, policy.quiet_hours);
        assert_eq!(updated.rules, policy.rules);
    }

    #[test]
    fn filtered_dashboard_is_reaggregated_from_selected_snapshots() {
        let provenance = usage_core::DataProvenance {
            source: usage_core::UsageDataSource::ProviderReported,
            scope: usage_core::UsageDataScope::AccountWide,
            quality: usage_core::UsageDataQuality::Authoritative,
            completeness: usage_core::UsageDataCompleteness::Complete,
            confidence: usage_core::UsageDataConfidence::High,
        };
        let day = chrono::NaiveDate::from_ymd_opt(2026, 7, 11).unwrap();
        let summary = |provider: &str, account: &str, tokens| usage_core::AccountUsageSummary {
            provider_id: ProviderId::new(provider),
            account_id: AccountId::new(account),
            activity: Some(usage_core::ActivitySummary {
                provenance: provenance.clone(),
                days: vec![usage_core::DailyUsagePoint {
                    date: day,
                    tokens,
                    cost_usd: None,
                    priced_tokens: 0,
                    unpriced_tokens: 0,
                }],
                today_tokens: tokens,
                lookback_tokens: tokens,
                lifetime_tokens: Some(tokens),
            }),
            cost: None,
            reset_credits: None,
        };
        let dashboard = usage_core::aggregate_usage_dashboard(vec![
            summary("codex", "personal", 10),
            summary("claude", "work", 90),
        ]);

        let filtered = dashboard_for_snapshots(dashboard, &[test_snapshot("codex", "personal")]);

        assert_eq!(filtered.accounts.len(), 1);
        assert_eq!(filtered.accounts[0].provider_id.as_str(), "codex");
        assert_eq!(filtered.days.len(), 1);
        assert_eq!(filtered.days[0].tokens, 10);
    }

    #[test]
    fn usage_fetch_plan_only_requests_data_needed_by_the_output() {
        let dashboard = usage_fetch_plan(OutputStyle::Dashboard, false);
        assert_eq!(
            usage_request_names(&dashboard),
            ["usage", "accounts", "config"]
        );
        assert!(dashboard.needs_accounts);
        assert!(dashboard.needs_config);

        let dashboard_all = usage_fetch_plan(OutputStyle::Dashboard, true);
        assert_eq!(usage_request_names(&dashboard_all), ["usage", "accounts"]);
        assert!(dashboard_all.needs_accounts);
        assert!(!dashboard_all.needs_config);

        let json = usage_fetch_plan(OutputStyle::Json, false);
        assert_eq!(usage_request_names(&json), ["usage", "config"]);
        assert!(!json.needs_accounts);
        assert!(json.needs_config);

        let json_all = usage_fetch_plan(OutputStyle::Json, true);
        assert_eq!(usage_request_names(&json_all), ["usage"]);
        assert!(!json_all.needs_accounts);
        assert!(!json_all.needs_config);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn request_batch_pipelines_frames_and_preserves_response_order() {
        let socket_path = TestSocketPath::new();
        let listener = UnixListener::bind(socket_path.as_path()).unwrap();
        let server = async {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();
            let mut requests = Vec::new();

            // Read every request before replying. A send-one/read-one client would
            // deadlock here, so this also verifies that request_batch pipelines.
            for _ in 0..3 {
                line.clear();
                assert!(reader.read_until(b'\n', &mut line).await.unwrap() > 0);
                assert_eq!(line.last(), Some(&b'\n'));
                requests.push(
                    serde_json::from_slice::<RequestEnvelope>(&line[..line.len() - 1])
                        .unwrap()
                        .request,
                );
            }
            assert!(matches!(requests[0], ApiRequest::GetUsage));
            assert!(matches!(requests[1], ApiRequest::GetAccounts));
            assert!(matches!(requests[2], ApiRequest::GetConfig));

            for (index, code) in [
                ApiErrorCode::InvalidJson,
                ApiErrorCode::InvalidRequest,
                ApiErrorCode::Internal,
            ]
            .into_iter()
            .enumerate()
            {
                let mut frame =
                    serde_json::to_vec(&ResponseEnvelope::new(ApiResponse::error(code, "test")))
                        .unwrap();
                frame.extend_from_slice(if index == 1 { b"\r\n" } else { b"\n" });
                let split = frame.len() / 2;
                writer.write_all(&frame[..split]).await.unwrap();
                tokio::task::yield_now().await;
                writer.write_all(&frame[split..]).await.unwrap();
            }
        };

        let client = request_batch(
            socket_path.as_path(),
            [
                ApiRequest::GetUsage,
                ApiRequest::GetAccounts,
                ApiRequest::GetConfig,
            ],
        );
        let (responses, ()) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(client, server)
        })
        .await
        .expect("batched socket exchange timed out");
        let response_codes = responses
            .unwrap()
            .into_iter()
            .map(|response| match response {
                ApiResponse::Error { error } => error.code,
                other => panic!("unexpected response: {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            response_codes,
            [
                ApiErrorCode::InvalidJson,
                ApiErrorCode::InvalidRequest,
                ApiErrorCode::Internal,
            ]
        );
    }

    #[test]
    fn rejects_structurally_decodable_response_from_future_protocol() {
        let mut envelope = ResponseEnvelope::new(ApiResponse::error(
            ApiErrorCode::Internal,
            "future response",
        ));
        envelope.api_version = API_VERSION + 1;
        let frame = serde_json::to_vec(&envelope).unwrap();

        let error = decode_response(&frame).unwrap_err();

        assert!(error.to_string().contains("incompatible with CLI version"));
    }

    #[test]
    fn forecast_filter_matches_provider_and_account_without_reordering() {
        let snapshots = [
            test_snapshot("codex", "shared"),
            test_snapshot("claude", "work"),
        ];
        let mut forecasts = vec![
            test_forecast("codex", "shared", "first"),
            test_forecast("claude", "shared", "wrong-provider"),
            test_forecast("codex", "missing", "missing-account"),
            test_forecast("claude", "work", "second"),
            test_forecast("codex", "shared", "third"),
        ];

        retain_forecasts_for_snapshots(&mut forecasts, &snapshots);

        assert_eq!(
            forecasts
                .iter()
                .map(|forecast| forecast.window_id.as_str())
                .collect::<Vec<_>>(),
            ["first", "second", "third"]
        );
    }

    fn test_snapshot(provider: &str, account: &str) -> UsageSnapshot {
        UsageSnapshot {
            provider_id: ProviderId::new(provider),
            account_id: AccountId::new(account),
            collected_at: chrono::Utc::now(),
            windows: Vec::new(),
            metadata: serde_json::Value::Null,
        }
    }

    fn test_forecast(provider: &str, account: &str, window: &str) -> UsageForecast {
        UsageForecast {
            provider_id: ProviderId::new(provider),
            account_id: AccountId::new(account),
            window_id: window.to_string(),
            generated_at: chrono::Utc::now(),
            reset_at: None,
            current_percent_used: 0.0,
            expected_percent_used: None,
            pace_delta_percent: None,
            rate_percent_per_hour: None,
            projected_percent_at_reset: None,
            projected_percent_remaining_at_reset: None,
            predicted_exhaustion_at: None,
            status: ForecastStatus::InsufficientData,
            sample_count: 0,
            confidence: ForecastConfidence::Low,
        }
    }
}
