use std::ffi::OsString;
use std::path::PathBuf;

use clap::error::ErrorKind;
use clap::{Args as ClapArgs, Parser, Subcommand, ValueEnum};

const AFTER_HELP: &str = "Provider shortcut:\n  <provider>   Show one provider, for example `usage codex`\n\nExamples:\n  usage\n  usage codex\n  usage claude --details\n  usage summary\n  usage activity codex --days 7\n  usage refresh codex";

#[derive(Debug, Parser)]
#[command(
    name = "usage",
    bin_name = "usage",
    disable_version_flag = true,
    about = "Inspect and manage UsageTracker",
    arg_required_else_help = false,
    allow_external_subcommands = true,
    after_help = AFTER_HELP
)]
struct RawCli {
    #[command(flatten)]
    global: GlobalArgs,
    #[command(flatten)]
    dashboard: RootDashboardArgs,
    #[command(subcommand)]
    command: Option<RawCommand>,
}

#[derive(Clone, Debug, Default, ClapArgs)]
pub struct GlobalArgs {
    /// Override the daemon Unix socket.
    #[arg(long, env = "USAGE_TRACKER_SOCKET", global = true)]
    pub socket_path: Option<PathBuf>,
    /// Emit machine-readable JSON.
    #[arg(long, global = true)]
    pub json: bool,
    /// Legacy output selector.
    #[arg(long, value_enum, global = true, hide = true)]
    pub style: Option<OutputStyle>,
    /// Colorize human-readable output.
    #[arg(long, value_enum, global = true)]
    pub color: Option<ColorChoice>,
    /// Maximum dashboard width in columns.
    #[arg(
        long,
        env = "USAGE_TRACKER_MAX_WIDTH",
        value_parser = parse_max_width,
        global = true
    )]
    pub max_width: Option<usize>,
    /// Print the CLI version.
    #[arg(short = 'V', long, global = true)]
    pub version: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum OutputStyle {
    Dashboard,
    Json,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Switch {
    On,
    Off,
}

impl Switch {
    pub fn enabled(self) -> bool {
        self == Self::On
    }
}

#[derive(Debug, Subcommand)]
enum RawCommand {
    /// Show a compact provider rollup.
    Summary(SummaryArgs),
    /// Show recent token and cost activity.
    Activity(ActivityArgs),
    /// Show daemon and provider health.
    #[command(alias = "health")]
    Status(StatusArgs),
    /// Show the usage dashboard (the default command).
    #[command(hide = true)]
    Usage(DashboardArgs),
    /// Poll providers immediately.
    Refresh(RefreshArgs),
    /// List and manage provider accounts.
    Accounts(AccountsArgs),
    /// Inspect and configure providers.
    Providers(ProvidersArgs),
    /// Inspect and edit daemon configuration.
    Config(ConfigArgs),
    /// Print the CLI version.
    #[command(hide = true)]
    Version,
    #[command(external_subcommand)]
    External(Vec<OsString>),
}

#[derive(Debug)]
pub enum Command {
    Dashboard(DashboardArgs),
    ProviderDashboard {
        provider: String,
        args: DashboardArgs,
    },
    Summary(SummaryArgs),
    Activity(ActivityArgs),
    Status(StatusArgs),
    Refresh(RefreshArgs),
    Accounts(AccountsArgs),
    Providers(ProvidersArgs),
    Config(ConfigArgs),
    Version,
}

#[derive(Debug)]
pub struct Cli {
    pub global: ResolvedGlobalArgs,
    pub command: Command,
}

#[derive(Debug)]
pub struct ResolvedGlobalArgs {
    pub socket_path: Option<PathBuf>,
    pub style: OutputStyle,
    pub color: ColorChoice,
    pub max_width: usize,
    pub version: bool,
}

impl Cli {
    pub fn try_parse_from(
        args: impl IntoIterator<Item = impl Into<OsString> + Clone>,
    ) -> Result<Self, clap::Error> {
        let raw = RawCli::try_parse_from(args)?;
        if raw.command.is_some() && !raw.dashboard.is_empty() {
            return Err(clap::Error::raw(
                ErrorKind::ArgumentConflict,
                "default dashboard options cannot be placed before a subcommand",
            ));
        }
        let (global, command) = match raw.command {
            Some(RawCommand::External(parts)) => parse_provider_shortcut(raw.global, parts)?,
            Some(RawCommand::Usage(args)) => (raw.global, Command::Dashboard(args)),
            Some(RawCommand::Summary(args)) => (raw.global, Command::Summary(args)),
            Some(RawCommand::Activity(args)) => (raw.global, Command::Activity(args)),
            Some(RawCommand::Status(args)) => (raw.global, Command::Status(args)),
            Some(RawCommand::Refresh(args)) => (raw.global, Command::Refresh(args)),
            Some(RawCommand::Accounts(args)) => (raw.global, Command::Accounts(args)),
            Some(RawCommand::Providers(args)) => (raw.global, Command::Providers(args)),
            Some(RawCommand::Config(args)) => (raw.global, Command::Config(args)),
            Some(RawCommand::Version) => (raw.global, Command::Version),
            None => (
                raw.global,
                Command::Dashboard(DashboardArgs {
                    accounts: raw.dashboard.accounts,
                    all_providers: raw.dashboard.all_providers,
                    details: raw.dashboard.details,
                    ..DashboardArgs::default()
                }),
            ),
        };
        validate_command(&command)?;
        Ok(Self {
            global: global.resolve()?,
            command,
        })
    }

    pub fn parse() -> Self {
        match Self::try_parse_from(std::env::args_os()) {
            Ok(cli) => cli,
            Err(error) => error.exit(),
        }
    }
}

#[derive(Debug, Parser)]
#[command(name = "usage <PROVIDER>", disable_version_flag = true)]
struct ProviderShortcutCli {
    #[command(flatten)]
    global: GlobalArgs,
    #[command(flatten)]
    dashboard: ProviderDashboardArgs,
}

fn parse_provider_shortcut(
    before: GlobalArgs,
    mut parts: Vec<OsString>,
) -> Result<(GlobalArgs, Command), clap::Error> {
    let Some(provider) = parts.first().cloned() else {
        return Err(clap::Error::raw(
            ErrorKind::InvalidSubcommand,
            "missing provider shortcut",
        ));
    };
    let provider = provider.into_string().map_err(|_| {
        clap::Error::raw(ErrorKind::InvalidUtf8, "provider IDs must be valid UTF-8")
    })?;
    parts[0] = OsString::from("usage <PROVIDER>");
    let shortcut = ProviderShortcutCli::try_parse_from(parts)?;
    let global = before.merge(shortcut.global)?;
    Ok((
        global,
        Command::ProviderDashboard {
            provider,
            args: DashboardArgs {
                accounts: shortcut.dashboard.accounts,
                details: shortcut.dashboard.details,
                ..DashboardArgs::default()
            },
        },
    ))
}

impl GlobalArgs {
    fn merge(self, after: Self) -> Result<Self, clap::Error> {
        Ok(Self {
            socket_path: merge_option("--socket-path", self.socket_path, after.socket_path)?,
            json: self.json || after.json,
            style: merge_option("--style", self.style, after.style)?,
            color: merge_option("--color", self.color, after.color)?,
            max_width: merge_option("--max-width", self.max_width, after.max_width)?,
            version: self.version || after.version,
        })
    }

    fn resolve(self) -> Result<ResolvedGlobalArgs, clap::Error> {
        let style = match (self.json, self.style) {
            (true, Some(OutputStyle::Dashboard)) => {
                return Err(clap::Error::raw(
                    ErrorKind::ArgumentConflict,
                    "--json conflicts with --style dashboard",
                ));
            }
            (true, _) | (false, Some(OutputStyle::Json)) => OutputStyle::Json,
            (false, None | Some(OutputStyle::Dashboard)) => OutputStyle::Dashboard,
        };
        Ok(ResolvedGlobalArgs {
            socket_path: self.socket_path,
            style,
            color: self.color.unwrap_or(ColorChoice::Auto),
            max_width: self.max_width.unwrap_or(80),
            version: self.version,
        })
    }
}

fn merge_option<T>(
    name: &str,
    before: Option<T>,
    after: Option<T>,
) -> Result<Option<T>, clap::Error> {
    match (before, after) {
        (Some(_), Some(_)) => Err(clap::Error::raw(
            ErrorKind::ArgumentConflict,
            format!("{name} cannot be specified on both sides of a provider shortcut"),
        )),
        (Some(value), None) | (None, Some(value)) => Ok(Some(value)),
        (None, None) => Ok(None),
    }
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

#[derive(Debug, Default, Clone, ClapArgs)]
pub struct DashboardArgs {
    /// Include only these provider IDs; repeat for more than one.
    #[arg(long = "provider", short = 'p')]
    pub providers: Vec<String>,
    /// Include only these account IDs; repeat for more than one.
    #[arg(long = "account", short = 'a')]
    pub accounts: Vec<String>,
    /// Include stored usage for providers that are currently disabled.
    #[arg(long)]
    pub all_providers: bool,
    /// Show extra per-window detail, credits, forecast, and identity.
    #[arg(long, short = 'd')]
    pub details: bool,
}

#[derive(Debug, Default, Clone, ClapArgs)]
struct RootDashboardArgs {
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

impl RootDashboardArgs {
    fn is_empty(&self) -> bool {
        self.accounts.is_empty() && !self.all_providers && !self.details
    }
}

#[derive(Debug, Default, Clone, ClapArgs)]
struct ProviderDashboardArgs {
    /// Include only these account IDs; repeat for more than one.
    #[arg(long = "account", short = 'a')]
    accounts: Vec<String>,
    /// Show extra per-window detail, credits, forecast, and identity.
    #[arg(long, short = 'd')]
    details: bool,
}

#[derive(Debug, Default, ClapArgs)]
pub struct SummaryArgs {
    /// Provider IDs to include.
    pub providers: Vec<String>,
    #[arg(long = "account", short = 'a')]
    pub accounts: Vec<String>,
    #[arg(long)]
    pub all_providers: bool,
}

#[derive(Debug, ClapArgs)]
pub struct ActivityArgs {
    /// Provider IDs to include.
    pub providers: Vec<String>,
    #[arg(long = "account", short = 'a')]
    pub accounts: Vec<String>,
    #[arg(long, default_value_t = 14, value_parser = clap::value_parser!(u8).range(1..=30))]
    pub days: u8,
    #[arg(long)]
    pub all_providers: bool,
}

#[derive(Debug, Default, ClapArgs)]
pub struct StatusArgs {
    /// Provider IDs to include.
    pub providers: Vec<String>,
    #[arg(long = "account", short = 'a')]
    pub accounts: Vec<String>,
    #[arg(long)]
    pub all_providers: bool,
}

#[derive(Debug, Default, ClapArgs)]
pub struct RefreshArgs {
    /// Provider IDs to refresh.
    #[arg(conflicts_with = "legacy_providers")]
    pub providers: Vec<String>,
    /// Compatibility spelling; repeat for more than one provider.
    #[arg(
        long = "provider",
        short = 'p',
        hide = true,
        conflicts_with = "providers"
    )]
    pub legacy_providers: Vec<String>,
}

impl RefreshArgs {
    pub fn providers(self) -> Vec<String> {
        if self.providers.is_empty() {
            self.legacy_providers
        } else {
            self.providers
        }
    }
}

#[derive(Debug, ClapArgs)]
pub struct AccountsArgs {
    /// Include only this provider when listing accounts.
    #[arg(long, short = 'p')]
    pub provider: Option<String>,
    /// Hide removed, hidden, and collection-disabled accounts.
    #[arg(long)]
    pub active: bool,
    /// Show profile and external-account ID columns.
    #[arg(long, short = 'v')]
    pub verbose: bool,
    #[command(subcommand)]
    pub command: Option<AccountCommand>,
}

#[derive(Debug, Subcommand)]
pub enum AccountCommand {
    /// List accounts and their stable IDs.
    List {
        #[arg(long, short = 'p')]
        provider: Option<String>,
        #[arg(long)]
        active: bool,
        #[arg(long, short = 'v')]
        verbose: bool,
    },
    Add {
        provider: String,
        #[arg(long)]
        name: Option<String>,
    },
    Rename {
        account: String,
        name: String,
    },
    Hide {
        account: String,
    },
    Show {
        account: String,
    },
    Enable {
        account: String,
    },
    Disable {
        account: String,
    },
    Remove {
        account: String,
    },
    Delete {
        account: String,
        #[arg(long)]
        yes: bool,
    },
    Launch {
        account: String,
    },
}

#[derive(Debug, ClapArgs)]
pub struct ProvidersArgs {
    #[command(subcommand)]
    pub command: Option<ProviderCommand>,
}

#[derive(Debug, Subcommand)]
pub enum ProviderCommand {
    List,
    Enable {
        provider: String,
    },
    Disable {
        provider: String,
    },
    Setup {
        provider: String,
    },
    Workspace {
        provider: String,
        workspace: Option<String>,
        #[arg(long, conflicts_with = "workspace")]
        automatic: bool,
    },
    Repair {
        provider: String,
        #[arg(long, short = 'a')]
        account: Option<String>,
    },
}

#[derive(Debug, ClapArgs)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: Option<ConfigCommand>,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    Show,
    Set {
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        poll_interval: Option<u64>,
        #[arg(long, value_enum)]
        notifications: Option<Switch>,
    },
}

fn validate_command(command: &Command) -> Result<(), clap::Error> {
    match command {
        Command::Accounts(AccountsArgs {
            provider,
            active,
            verbose,
            command: Some(_),
        }) if provider.is_some() || *active || *verbose => Err(clap::Error::raw(
            ErrorKind::ArgumentConflict,
            "account list options cannot be combined with an account subcommand",
        )),
        Command::Config(ConfigArgs {
            command:
                Some(ConfigCommand::Set {
                    poll_interval: None,
                    notifications: None,
                }),
        }) => Err(clap::Error::raw(
            ErrorKind::MissingRequiredArgument,
            "config set requires --poll-interval or --notifications",
        )),
        Command::Providers(ProvidersArgs {
            command:
                Some(ProviderCommand::Workspace {
                    workspace: None,
                    automatic: false,
                    ..
                }),
        }) => Err(clap::Error::raw(
            ErrorKind::MissingRequiredArgument,
            "provide a workspace ID or pass --automatic",
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_provider_shortcut_options_and_trailing_globals() {
        let cli = Cli::try_parse_from([
            "usage",
            "codex",
            "--details",
            "--account",
            "work",
            "--json",
            "--max-width",
            "72",
        ])
        .unwrap();
        assert_eq!(cli.global.style, OutputStyle::Json);
        assert_eq!(cli.global.max_width, 72);
        let Command::ProviderDashboard { provider, args } = cli.command else {
            panic!("expected provider dashboard");
        };
        assert_eq!(provider, "codex");
        assert!(args.details);
        assert_eq!(args.accounts, ["work"]);
    }

    #[test]
    fn parses_reserved_words_as_provider_positionals() {
        let cli = Cli::try_parse_from(["usage", "summary", "status"]).unwrap();
        let Command::Summary(args) = cli.command else {
            panic!("expected summary");
        };
        assert_eq!(args.providers, ["status"]);
    }

    #[test]
    fn rejects_conflicting_output_selectors() {
        let error = Cli::try_parse_from(["usage", "--json", "--style", "dashboard"]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ArgumentConflict);
    }

    #[test]
    fn rejects_activity_range_outside_api_lookback() {
        assert!(Cli::try_parse_from(["usage", "activity", "--days", "0"]).is_err());
        assert!(Cli::try_parse_from(["usage", "activity", "--days", "31"]).is_err());
    }

    #[test]
    fn rejects_mixed_refresh_selector_spellings() {
        assert!(
            Cli::try_parse_from(["usage", "refresh", "codex", "--provider", "claude"]).is_err()
        );
    }

    #[test]
    fn keeps_legacy_usage_and_style_forms() {
        let cli = Cli::try_parse_from([
            "usage",
            "usage",
            "--provider",
            "codex",
            "--details",
            "--style",
            "json",
        ])
        .unwrap();
        assert_eq!(cli.global.style, OutputStyle::Json);
        let Command::Dashboard(args) = cli.command else {
            panic!("expected legacy dashboard");
        };
        assert_eq!(args.providers, ["codex"]);
        assert!(args.details);
    }

    #[test]
    fn rejects_duplicate_globals_around_provider_shortcut() {
        let error = Cli::try_parse_from(["usage", "--color", "auto", "codex", "--color", "never"])
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ArgumentConflict);
    }

    #[test]
    fn enforces_dashboard_minimum_width() {
        assert!(Cli::try_parse_from(["usage", "--max-width", "59"]).is_err());
        assert_eq!(
            Cli::try_parse_from(["usage", "--max-width", "60"])
                .unwrap()
                .global
                .max_width,
            60
        );
    }

    #[test]
    fn parses_default_dashboard_options_without_usage_subcommand() {
        let cli =
            Cli::try_parse_from(["usage", "--details", "--account", "work", "--all-providers"])
                .unwrap();
        let Command::Dashboard(args) = cli.command else {
            panic!("expected default dashboard");
        };
        assert!(args.details);
        assert!(args.all_providers);
        assert_eq!(args.accounts, ["work"]);
    }

    #[test]
    fn provider_shortcut_rejects_multi_provider_dashboard_options() {
        assert!(Cli::try_parse_from(["usage", "codex", "--provider", "claude"]).is_err());
        assert!(Cli::try_parse_from(["usage", "codex", "--all-providers"]).is_err());
    }

    #[test]
    fn validates_config_and_workspace_syntax_during_parsing() {
        assert!(Cli::try_parse_from(["usage", "config", "set"]).is_err());
        assert!(Cli::try_parse_from(["usage", "providers", "workspace", "codex"]).is_err());
    }

    #[test]
    fn accepts_version_globally_after_dynamic_or_builtin_commands() {
        assert!(
            Cli::try_parse_from(["usage", "codex", "--version"])
                .unwrap()
                .global
                .version
        );
        assert!(
            Cli::try_parse_from(["usage", "summary", "--version"])
                .unwrap()
                .global
                .version
        );
    }

    #[test]
    fn account_list_options_work_with_or_without_explicit_list() {
        let implicit =
            Cli::try_parse_from(["usage", "accounts", "--provider", "opencode", "--active"])
                .unwrap();
        let Command::Accounts(args) = implicit.command else {
            panic!("expected accounts command");
        };
        assert_eq!(args.provider.as_deref(), Some("opencode"));
        assert!(args.active);
        assert!(args.command.is_none());

        let explicit =
            Cli::try_parse_from(["usage", "accounts", "list", "--provider", "opencode"]).unwrap();
        assert!(matches!(
            explicit.command,
            Command::Accounts(AccountsArgs {
                command: Some(AccountCommand::List { .. }),
                ..
            })
        ));
    }
}
