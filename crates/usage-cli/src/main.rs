use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};
use usage_core::{default_socket_path, ApiRequest, ProviderId};

#[derive(Debug, Parser)]
#[command(name = "usage")]
struct Args {
    #[arg(long, env = "USAGE_TRACKER_SOCKET")]
    socket_path: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
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
    let request = match args.command.unwrap_or(Command::Usage) {
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
    println!("{response}");
    Ok(())
}

async fn send_request(socket_path: &PathBuf, request: &ApiRequest) -> anyhow::Result<String> {
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
    lines
        .next_line()
        .await?
        .context("daemon closed connection without a response")
}
