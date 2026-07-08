use std::path::PathBuf;
use std::{env, io};

use anyhow::{bail, Context};
use clap::{Parser, Subcommand, ValueEnum};
use io::IsTerminal;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};
use usage_core::{default_socket_path, ApiRequest, ApiResponse, ProviderId};

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
    Status,
    Usage,
    Refresh {
        #[arg(long = "provider")]
        providers: Vec<String>,
    },
    Health,
    Accounts,
    Config,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let command = args.command.unwrap_or(Command::Usage);
    let usage_command = matches!(command, Command::Status | Command::Usage);
    let request = match command {
        Command::Status | Command::Usage => ApiRequest::GetUsage,
        Command::Refresh { providers } => ApiRequest::Refresh {
            providers: (!providers.is_empty()).then(|| {
                providers
                    .into_iter()
                    .map(ProviderId::new)
                    .collect::<Vec<_>>()
            }),
        },
        Command::Health => ApiRequest::GetProviderHealth,
        Command::Accounts => ApiRequest::GetAccounts,
        Command::Config => ApiRequest::GetConfig,
    };

    let socket_path = args
        .socket_path
        .or_else(default_socket_path)
        .context("failed to resolve ~/.usagetracker/usage.sock")?;
    let response = send_request(&socket_path, &request).await?;
    if usage_command && args.style != OutputStyle::Json {
        let accounts = match send_request(&socket_path, &ApiRequest::GetAccounts).await? {
            ApiResponse::Accounts { accounts } => accounts,
            ApiResponse::Error { error } => {
                bail!("daemon returned {}: {}", error.code, error.message);
            }
            other => bail!("daemon returned unexpected accounts response: {other:?}"),
        };
        match response {
            ApiResponse::Usage { snapshots } => {
                println!(
                    "{}",
                    render::render_usage(
                        &snapshots,
                        &accounts,
                        args.style,
                        color_enabled(args.color)
                    )
                );
            }
            ApiResponse::Error { error } => {
                bail!("daemon returned {}: {}", error.code, error.message);
            }
            other => bail!("daemon returned unexpected usage response: {other:?}"),
        }
    } else {
        println!("{}", serde_json::to_string(&response)?);
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
