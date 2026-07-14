use std::collections::BTreeMap;
use std::env;
use std::io::{self, IsTerminal, Write as _};
use std::path::Path;

use anyhow::{bail, Context};
use chrono::Utc;
use usage_core::{
    default_socket_path, AccountId, ApiRequest, ApiResponse, ConfigResponse, ProviderId,
    ProviderToggle, StateSnapshot,
};

mod cli;
mod client;
mod render;
mod selection;
mod views;

pub use cli::OutputStyle;
use cli::{
    AccountCommand, AccountsArgs, ActivityArgs, ColorChoice, Command, ConfigArgs, ConfigCommand,
    DashboardArgs, ProviderCommand, ProvidersArgs, StatusArgs, SummaryArgs,
};
use client::{unexpected, Client};
use selection::{ProviderCatalog, SelectedState, SelectionRequest};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();
    if cli.global.version {
        println!("usage {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let socket_path = cli
        .global
        .socket_path
        .or_else(default_socket_path)
        .context("failed to resolve ~/.usagetracker/usage.sock")?;
    let color = cli.global.style != OutputStyle::Json && color_enabled(cli.global.color);
    let width = render::output_width(cli.global.max_width);
    let client = Client::new(&socket_path);

    match cli.command {
        Command::Dashboard(args) => {
            run_dashboard(&client, args, cli.global.style, color, width).await
        }
        Command::ProviderDashboard { provider, mut args } => {
            let state = fetch_state(&client).await?;
            let catalog = ProviderCatalog::from_state(&state);
            let provider = catalog.resolve(&provider).unwrap_or_else(|error| {
                let mut message = format!("unknown command or provider '{}'", error.token);
                if let Some(suggestion) = error.suggestion {
                    message.push_str(&format!(
                        "\n\n  tip: a similar provider or command exists: '{suggestion}'"
                    ));
                }
                clap::Error::raw(clap::error::ErrorKind::InvalidSubcommand, message).exit()
            });
            args.providers = vec![provider];
            run_dashboard_with_state(state, args, cli.global.style, color, width)
        }
        Command::Summary(args) => run_summary(&client, args, cli.global.style, color, width).await,
        Command::Activity(args) => run_activity(&client, args, cli.global.style, color).await,
        Command::Status(args) => {
            run_status(&client, &socket_path, args, cli.global.style, color, width).await
        }
        Command::Refresh(args) => {
            run_refresh(&client, args.providers(), cli.global.style, color).await
        }
        Command::Accounts(args) => run_accounts(&client, args, cli.global.style, color).await,
        Command::Providers(args) => run_providers(&client, args, cli.global.style, color).await,
        Command::Config(args) => run_config(&client, args, cli.global.style, color).await,
        Command::Version => {
            println!("usage {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}

async fn run_dashboard(
    client: &Client<'_>,
    args: DashboardArgs,
    style: OutputStyle,
    color: bool,
    width: usize,
) -> anyhow::Result<()> {
    run_dashboard_with_state(fetch_state(client).await?, args, style, color, width)
}

fn run_dashboard_with_state(
    state: StateSnapshot,
    args: DashboardArgs,
    style: OutputStyle,
    color: bool,
    width: usize,
) -> anyhow::Result<()> {
    let provider_scoped = args.providers.len() == 1;
    let selected = SelectedState::from_state(
        state,
        SelectionRequest {
            providers: args.providers,
            accounts: args.accounts,
            all_providers: args.all_providers,
        },
    )?;
    if style == OutputStyle::Json {
        return print_json(&ApiResponse::Usage {
            snapshots: selected.snapshots,
            dashboard: selected.dashboard,
            forecasts: selected.forecasts,
            window_provenance: selected.window_provenance,
        });
    }

    let mut output = if selected.snapshots.is_empty() {
        String::new()
    } else {
        render::render_usage_dashboard_with_summary(
            &selected.snapshots,
            &selected.forecasts,
            &selected.accounts,
            &selected.dashboard,
            render::UsageRenderOptions {
                style,
                color,
                width,
                details: args.details,
                provider_scoped,
            },
        )
    };
    let disabled = selected
        .provider_ids
        .iter()
        .filter(|provider_id| !selected.provider_info(provider_id).enabled)
        .filter(|provider_id| {
            selected
                .snapshots
                .iter()
                .any(|snapshot| snapshot.provider_id.as_str() == provider_id.as_str())
        })
        .map(|provider_id| selected.provider_info(provider_id).display_name.clone())
        .collect::<Vec<_>>();
    if !disabled.is_empty() {
        output = format!("Collection disabled: {}\n\n{output}", disabled.join(", "));
    }
    let empty_states = render_dashboard_empty_states(&selected);
    if !empty_states.is_empty() {
        if !output.is_empty() {
            output.push_str("\n\n");
        }
        output.push_str(&empty_states);
    }
    println!("{output}");
    Ok(())
}

async fn run_summary(
    client: &Client<'_>,
    args: SummaryArgs,
    style: OutputStyle,
    color: bool,
    width: usize,
) -> anyhow::Result<()> {
    let selected = SelectedState::from_state(
        fetch_state(client).await?,
        SelectionRequest {
            providers: args.providers,
            accounts: args.accounts,
            all_providers: args.all_providers,
        },
    )?;
    let view = views::SummaryView::build(&selected, Utc::now());
    if style == OutputStyle::Json {
        print_json(&view)
    } else {
        println!("{}", render::render_summary(&view, color, width));
        Ok(())
    }
}

async fn run_activity(
    client: &Client<'_>,
    args: ActivityArgs,
    style: OutputStyle,
    color: bool,
) -> anyhow::Result<()> {
    let explicit_accounts = args.accounts.clone();
    let has_provider_filter = !args.providers.is_empty();
    let selected = SelectedState::from_state(
        fetch_state(client).await?,
        SelectionRequest {
            providers: args.providers,
            accounts: args.accounts,
            all_providers: args.all_providers,
        },
    )?;
    let mut view =
        views::ActivityView::build(&selected, args.days, views::ActivityView::today_local());
    view.filters.providers = has_provider_filter
        .then(|| selected.provider_ids.clone())
        .unwrap_or_default();
    view.filters.accounts = explicit_accounts;
    if style == OutputStyle::Json {
        print_json(&view)
    } else {
        println!("{}", render::render_activity(&view, color));
        Ok(())
    }
}

async fn run_status(
    client: &Client<'_>,
    socket_path: &Path,
    args: StatusArgs,
    style: OutputStyle,
    color: bool,
    width: usize,
) -> anyhow::Result<()> {
    let selected = SelectedState::from_state(
        fetch_state(client).await?,
        SelectionRequest {
            providers: args.providers,
            accounts: args.accounts,
            all_providers: args.all_providers,
        },
    )?;
    let status = render::StatusView::from_selected_parts(
        socket_path.display().to_string(),
        &selected.snapshots,
        &selected.accounts,
        &selected.health,
        &selected.config,
        &selected.provider_ids,
    );
    if style == OutputStyle::Json {
        print_json(&status)
    } else {
        println!(
            "{}",
            render::render_status_with_width(&status, style, color, width)
        );
        Ok(())
    }
}

async fn run_refresh(
    client: &Client<'_>,
    providers: Vec<String>,
    style: OutputStyle,
    color: bool,
) -> anyhow::Result<()> {
    let providers = if providers.is_empty() {
        None
    } else {
        let state = fetch_state(client).await?;
        Some(
            ProviderCatalog::from_state(&state)
                .resolve_many(&providers)
                .map_err(anyhow::Error::new)?
                .into_iter()
                .map(ProviderId::new)
                .collect(),
        )
    };
    let started = client.request(ApiRequest::Refresh { providers }).await?;
    let job = match started {
        ApiResponse::RefreshStarted { job, .. } => client.wait_for_refresh(job).await?,
        other => return unexpected("refresh_started", other),
    };
    if style == OutputStyle::Json {
        return print_json(&ApiResponse::RefreshJob { job });
    }

    let state = fetch_state(client).await?;
    let started_at = job.started_at.unwrap_or(job.created_at);
    let finished_at = job.finished_at.unwrap_or_else(Utc::now);
    println!(
        "{}",
        render::render_refresh(
            started_at,
            finished_at,
            &job.provider_results,
            &state.accounts,
            &state.snapshots,
            style,
            color,
        )
    );
    Ok(())
}

async fn run_accounts(
    client: &Client<'_>,
    args: AccountsArgs,
    style: OutputStyle,
    color: bool,
) -> anyhow::Result<()> {
    let default_list = AccountCommand::List {
        provider: args.provider,
        active: args.active,
        verbose: args.verbose,
    };
    match args.command.unwrap_or(default_list) {
        AccountCommand::List {
            provider,
            active,
            verbose,
        } => {
            let state = fetch_state(client).await?;
            let provider = provider
                .map(|provider| ProviderCatalog::from_state(&state).resolve(&provider))
                .transpose()
                .map_err(anyhow::Error::new)?;
            let mut accounts = state.accounts;
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
                    render::render_accounts(&accounts, &state.snapshots, style, color, verbose)
                );
                Ok(())
            }
        }
        AccountCommand::Add { provider, name } => {
            let provider = resolve_provider(client, &provider).await?;
            let response = client
                .request(ApiRequest::AddProviderAccount {
                    provider_id: ProviderId::new(provider),
                    display_name: name,
                })
                .await?;
            print_action_response(response, style, color)
        }
        AccountCommand::Rename { account, name } => {
            update_account(client, account, Some(name), None, None, style, color).await
        }
        AccountCommand::Hide { account } => {
            update_account(client, account, None, Some(true), None, style, color).await
        }
        AccountCommand::Show { account } => {
            update_account(client, account, None, Some(false), None, style, color).await
        }
        AccountCommand::Enable { account } => {
            update_account(client, account, None, Some(false), Some(true), style, color).await
        }
        AccountCommand::Disable { account } => {
            update_account(client, account, None, None, Some(false), style, color).await
        }
        AccountCommand::Remove { account } => {
            let response = client
                .request(ApiRequest::RemoveAccount {
                    account_id: AccountId::new(account),
                })
                .await?;
            print_action_response(response, style, color)
        }
        AccountCommand::Delete { account, yes } => {
            if !yes {
                bail!("permanent deletion requires --yes; use `accounts remove` to keep history");
            }
            let response = client
                .request(ApiRequest::DeleteAccount {
                    account_id: AccountId::new(account),
                })
                .await?;
            print_action_response(response, style, color)
        }
        AccountCommand::Launch { account } => {
            let response = client
                .request(ApiRequest::LaunchProviderAccount {
                    account_id: AccountId::new(account),
                })
                .await?;
            print_action_response(response, style, color)
        }
    }
}

async fn update_account(
    client: &Client<'_>,
    account: String,
    display_name: Option<String>,
    hidden: Option<bool>,
    collection_enabled: Option<bool>,
    style: OutputStyle,
    color: bool,
) -> anyhow::Result<()> {
    let response = client
        .request(ApiRequest::UpdateAccount {
            account_id: AccountId::new(account),
            display_name,
            hidden,
            collection_enabled,
        })
        .await?;
    print_action_response(response, style, color)
}

async fn run_providers(
    client: &Client<'_>,
    args: ProvidersArgs,
    style: OutputStyle,
    color: bool,
) -> anyhow::Result<()> {
    match args.command.unwrap_or(ProviderCommand::List) {
        ProviderCommand::List => print_config(fetch_config(client).await?, style, color),
        ProviderCommand::Enable { provider } => {
            set_provider_enabled(
                client,
                resolve_provider(client, &provider).await?,
                true,
                style,
                color,
            )
            .await
        }
        ProviderCommand::Disable { provider } => {
            set_provider_enabled(
                client,
                resolve_provider(client, &provider).await?,
                false,
                style,
                color,
            )
            .await
        }
        ProviderCommand::Setup { provider } => {
            let provider = resolve_provider(client, &provider).await?;
            print_action_response(
                client
                    .request(ApiRequest::GetProviderSetup {
                        provider_id: ProviderId::new(provider),
                    })
                    .await?,
                style,
                color,
            )
        }
        ProviderCommand::Workspace {
            provider,
            workspace,
            automatic,
        } => {
            if workspace.is_none() && !automatic {
                bail!("provide a workspace ID or pass --automatic");
            }
            let provider = resolve_provider(client, &provider).await?;
            print_action_response(
                client
                    .request(ApiRequest::UpdateProviderSetup {
                        provider_id: ProviderId::new(provider),
                        settings: BTreeMap::new(),
                        workspace_id: workspace,
                    })
                    .await?,
                style,
                color,
            )
        }
        ProviderCommand::Repair { provider, account } => {
            let provider = resolve_provider(client, &provider).await?;
            print_action_response(
                client
                    .request(ApiRequest::RepairProvider {
                        provider_id: ProviderId::new(provider),
                        account_id: account.map(AccountId::new),
                    })
                    .await?,
                style,
                color,
            )
        }
    }
}

async fn set_provider_enabled(
    client: &Client<'_>,
    provider: String,
    enabled: bool,
    style: OutputStyle,
    color: bool,
) -> anyhow::Result<()> {
    let mut providers = BTreeMap::new();
    providers.insert(provider, ProviderToggle { enabled });
    match client
        .request(ApiRequest::UpdateConfig {
            poll_interval_seconds: None,
            providers: Some(providers),
            notifications: None,
        })
        .await?
    {
        ApiResponse::Config { config } => print_config(config, style, color),
        other => unexpected("config", other),
    }
}

async fn run_config(
    client: &Client<'_>,
    args: ConfigArgs,
    style: OutputStyle,
    color: bool,
) -> anyhow::Result<()> {
    match args.command.unwrap_or(ConfigCommand::Show) {
        ConfigCommand::Show => print_config(fetch_config(client).await?, style, color),
        ConfigCommand::Set {
            poll_interval,
            notifications,
        } => {
            if poll_interval.is_none() && notifications.is_none() {
                bail!("config set requires --poll-interval or --notifications");
            }
            let notifications = match notifications {
                Some(value) => {
                    let mut config = fetch_config(client).await?.notifications;
                    config.enabled = value.enabled();
                    Some(config)
                }
                None => None,
            };
            match client
                .request(ApiRequest::UpdateConfig {
                    poll_interval_seconds: poll_interval,
                    providers: None,
                    notifications,
                })
                .await?
            {
                ApiResponse::Config { config } => print_config(config, style, color),
                other => unexpected("config", other),
            }
        }
    }
}

fn render_dashboard_empty_states(selected: &SelectedState) -> String {
    if selected.provider_ids.is_empty() {
        return "No providers are enabled.\n\nNext step   usage providers enable PROVIDER"
            .to_string();
    }
    let providers_with_snapshot = selected
        .snapshots
        .iter()
        .map(|snapshot| snapshot.provider_id.as_str())
        .collect::<std::collections::HashSet<_>>();
    selected
        .provider_ids
        .iter()
        .filter(|provider_id| !providers_with_snapshot.contains(provider_id.as_str()))
        .map(|provider_id| {
            let info = selected.provider_info(provider_id);
            let provider_accounts = selected
                .accounts
                .iter()
                .filter(|account| account.provider_id.as_str() == provider_id)
                .collect::<Vec<_>>();
            let health = selected
                .health
                .iter()
                .find(|row| row.provider_id.as_str() == provider_id);
            let state = if !info.enabled {
                "disabled".to_string()
            } else if provider_accounts
                .iter()
                .any(|account| !account.collection_enabled)
            {
                "account collection disabled".to_string()
            } else {
                health
                    .map(|row| json_name(&row.status))
                    .unwrap_or_else(|| "no data".to_string())
            };
            let next_step = if !info.enabled {
                Some(format!("usage providers enable {provider_id}"))
            } else if let Some(account) = provider_accounts
                .iter()
                .find(|account| !account.collection_enabled)
            {
                Some(format!("usage accounts enable {}", account.id))
            } else if matches!(
                health.map(|row| &row.status),
                Some(
                    usage_core::ProviderHealthStatus::CredentialsMissing
                        | usage_core::ProviderHealthStatus::AuthFailed
                )
            ) {
                Some(format!("usage providers repair {provider_id}"))
            } else if provider_accounts.is_empty() && info.capabilities.add_account {
                Some(format!("usage accounts add {provider_id}"))
            } else {
                None
            };
            let mut output = format!(
                "{}\n\nNo usage has been collected yet.\nState       {state}",
                info.display_name
            );
            for account in provider_accounts {
                let identity = account
                    .display_name
                    .as_deref()
                    .or(account.email.as_deref())
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| {
                        if account.external_account_id.is_empty() {
                            account.id.as_str()
                        } else {
                            account.external_account_id.as_str()
                        }
                    });
                output.push_str(&format!("\nAccount     {identity} ({})", account.id));
            }
            if let Some(error) = health.and_then(|row| row.last_error_message.as_deref()) {
                output.push_str(&format!("\nDetail      {error}"));
            }
            if let Some(next_step) = next_step {
                output.push_str(&format!("\nNext step   {next_step}"));
            }
            output
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

async fn resolve_provider(client: &Client<'_>, token: &str) -> anyhow::Result<String> {
    ProviderCatalog::from_state(&fetch_state(client).await?)
        .resolve(token)
        .map_err(anyhow::Error::new)
}

async fn fetch_state(client: &Client<'_>) -> anyhow::Result<StateSnapshot> {
    match client.request(ApiRequest::GetState).await? {
        ApiResponse::State { state } => Ok(state),
        other => unexpected("state", other),
    }
}

async fn fetch_config(client: &Client<'_>) -> anyhow::Result<ConfigResponse> {
    match client.request(ApiRequest::GetConfig).await? {
        ApiResponse::Config { config } => Ok(config),
        other => unexpected("config", other),
    }
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

fn color_enabled(choice: ColorChoice) -> bool {
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => env::var_os("NO_COLOR").is_none() && io::stdout().is_terminal(),
    }
}

fn print_json(value: &impl serde::Serialize) -> anyhow::Result<()> {
    let stdout = io::stdout();
    let mut output = io::BufWriter::new(stdout.lock());
    serde_json::to_writer(&mut output, value)?;
    writeln!(output)?;
    output.flush()?;
    Ok(())
}

fn json_name(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value)
        .unwrap_or_else(|_| "\"unknown\"".to_string())
        .trim_matches('"')
        .to_string()
}
