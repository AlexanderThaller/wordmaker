mod api;
mod app;
mod client;
mod config;
mod db;
mod domain;
mod error;
mod mcp;
mod render;
mod site;
mod theme;

use std::{
    net::SocketAddr,
    sync::Arc,
};

use anyhow::Context;
use clap::{
    Parser,
    Subcommand,
};
use rmcp::ServiceExt;
use tracing_subscriber::EnvFilter;

use crate::{
    app::AppState,
    config::Config,
    mcp::WordMakerMcp,
};

/// A small `AsciiDoc` CMS with a REST API and an MCP server.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(flatten)]
    config: Config,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the web server: public site, REST API and MCP over HTTP (default).
    Serve(ServeArgs),
    /// Run the MCP server over stdio, for Claude Desktop.
    Mcp,
    /// Talk to a running server's MCP endpoint from the shell, for agents
    /// without MCP support.
    Client(client::ClientArgs),
    /// Print a new random API token.
    Token,
}

#[derive(clap::Args, Default)]
struct ServeArgs {
    /// Address to listen on.
    #[arg(long, env = "WORDMAKER_BIND", default_value = "127.0.0.1:3000")]
    bind: Option<SocketAddr>,

    /// Bearer token required by `/api` and `/mcp`.
    #[arg(long, env = "WORDMAKER_TOKEN", hide_env_values = true)]
    token: Option<String>,

    /// Extra `Host` values accepted by `/mcp`, besides loopback and the base
    /// URL's host.
    #[arg(
        long = "allowed-host",
        env = "WORDMAKER_ALLOWED_HOSTS",
        value_delimiter = ','
    )]
    allowed_hosts: Vec<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Logs go to stderr: in stdio mode stdout carries the MCP protocol. The
    // client keeps quiet so agents only see results and errors; it reports
    // transport failures itself, so rmcp's own logging stays off.
    let default_level = match cli.command {
        Some(Command::Client(_)) => "warn,rmcp=off",
        _ => "info",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level)),
        )
        .with_writer(std::io::stderr)
        .init();

    match cli.command.unwrap_or(Command::Serve(ServeArgs::default())) {
        Command::Serve(args) => serve(cli.config, args).await,
        Command::Mcp => mcp_stdio(cli.config).await,
        Command::Client(args) => client::run(&cli.config, args).await,
        Command::Token => {
            let token = format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            );
            println!("{token}");
            Ok(())
        }
    }
}

async fn serve(config: Config, args: ServeArgs) -> anyhow::Result<()> {
    let token = args
        .token
        .filter(|t| !t.trim().is_empty())
        .context("WORDMAKER_TOKEN must be set; generate one with `wordmaker token`")?;
    let bind = args
        .bind
        .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 3000)));

    let pool = db::connect(&config.database_url).await?;
    let mut allowed_hosts: Vec<String> = ["localhost", "127.0.0.1", "::1"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    allowed_hosts.extend(base_url_hosts(&config.base_url));
    allowed_hosts.extend(args.allowed_hosts);

    let state = AppState {
        pool,
        config: Arc::new(config),
        token: token.into(),
        themes: Arc::default(),
    };
    let router = app::router(state, allowed_hosts);

    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!("listening on http://{bind}");
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

async fn mcp_stdio(config: Config) -> anyhow::Result<()> {
    let pool = db::connect(&config.database_url).await?;
    let server = WordMakerMcp::new(pool, Arc::new(config))
        .serve(rmcp::transport::stdio())
        .await?;
    server.waiting().await?;
    Ok(())
}

/// `https://example.com:8443/x` yields `example.com:8443` and `example.com`.
fn base_url_hosts(base_url: &str) -> Vec<String> {
    let rest = base_url
        .split_once("://")
        .map_or(base_url, |(_, rest)| rest);
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.is_empty() {
        return Vec::new();
    }
    let host = authority
        .rsplit_once(':')
        .map_or(authority, |(host, _)| host);
    vec![authority.to_owned(), host.to_owned()]
}
