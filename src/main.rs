mod analytics;
mod api;
mod app;
mod client;
mod config;
mod db;
mod domain;
mod error;
mod images;
mod mcp;
mod oauth;
mod render;
mod site;
mod theme;
mod token;

use std::{
    future::IntoFuture,
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};

use clap::{
    Parser,
    Subcommand,
};
use rmcp::ServiceExt;
use tokio_util::sync::CancellationToken;
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
    /// Download or update the free DB-IP Lite `GeoIP` databases (city and
    /// ASN) for analytics.
    Geoip(analytics::download::GeoipArgs),
}

#[derive(clap::Args, Default)]
struct ServeArgs {
    /// Address to listen on.
    #[arg(long, env = "WORDMAKER_BIND", default_value = "127.0.0.1:3000")]
    bind: Option<SocketAddr>,

    #[command(flatten)]
    token: token::TokenArgs,

    /// Extra `Host` values accepted by `/mcp`, besides loopback and the base
    /// URL's host.
    #[arg(
        long = "allowed-host",
        env = "WORDMAKER_ALLOWED_HOSTS",
        value_delimiter = ','
    )]
    allowed_hosts: Vec<String>,

    #[command(flatten)]
    analytics: analytics::AnalyticsArgs,
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
        Command::Geoip(args) => analytics::download::run(&args).await,
        Command::Token => {
            println!("{}", token::generate());
            Ok(())
        }
    }
}

async fn serve(config: Config, args: ServeArgs) -> anyhow::Result<()> {
    let token = args
        .token
        .require("; generate one with `wordmaker token`")?;
    token::check_strength(&token)?;
    let bind = args
        .bind
        .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 3000)));

    let pool = db::connect(&config.database_url).await?;
    oauth::spawn_purge(pool.clone());
    let mut allowed_hosts: Vec<String> = ["localhost", "127.0.0.1", "::1"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    allowed_hosts.extend(base_url_hosts(&config.base_url));
    allowed_hosts.extend(args.allowed_hosts);
    args.analytics.check_bind(bind);
    let stop_sink = CancellationToken::new();
    let (analytics, sink) = args
        .analytics
        .start(&config.base_url, stop_sink.clone())?
        .unzip();

    let state = AppState {
        pool,
        config: Arc::new(config),
        token: token.into(),
        themes: Arc::default(),
        images: Arc::default(),
        renders: Arc::default(),
        oauth_clients: Arc::default(),
        guesses: Arc::new(token::Guesses::behind(args.analytics.proxy_hops())),
        analytics,
    };
    let shutdown = CancellationToken::new();
    let router = app::router(state, allowed_hosts, shutdown.clone());

    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!("listening on http://{bind}");
    let server = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown.clone().cancelled_owned())
    .into_future();
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => result?,
        () = shutdown_signal() => {
            // Also ends MCP event streams, which never end by themselves.
            shutdown.cancel();
            if let Ok(result) = tokio::time::timeout(GRACE_PERIOD, &mut server).await {
                result?;
            } else {
                tracing::warn!(
                    "closing connections still open after {}s",
                    GRACE_PERIOD.as_secs()
                );
            }
        }
    }

    // The router held the analytics handles; once they are gone, or when
    // told to because connections outlived the grace period, the sink
    // writes what it has buffered and stops.
    stop_sink.cancel();
    if let Some(sink) = sink
        && tokio::time::timeout(Duration::from_secs(15), sink)
            .await
            .is_err()
    {
        tracing::warn!("analytics: gave up writing buffered page views");
    }
    Ok(())
}

/// How long open requests may take to finish on shutdown. Together with
/// writing buffered page views it stays within Docker's 10 s before SIGKILL
/// in the usual case.
const GRACE_PERIOD: Duration = Duration::from_secs(5);

/// Ctrl-C, or SIGTERM from systemd or Docker.
async fn shutdown_signal() {
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(err) => {
                tracing::warn!("cannot listen for SIGTERM: {err}");
                std::future::pending::<()>().await;
            }
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        () = terminate => {}
    }
    tracing::info!("shutting down");
}

async fn mcp_stdio(config: Config) -> anyhow::Result<()> {
    let pool = db::connect(&config.database_url).await?;
    let server = WordMakerMcp::new(pool, Arc::new(config))
        .serve(rmcp::transport::stdio())
        .await?;
    server.waiting().await?;
    Ok(())
}

/// `https://example.com:8443/x` yields `example.com:8443` and `example.com`;
/// without an explicit port only the host is returned.
fn base_url_hosts(base_url: &str) -> Vec<String> {
    let Ok(url) = url::Url::parse(base_url) else {
        return Vec::new();
    };
    let Some(host) = url.host_str() else {
        return Vec::new();
    };
    match url.port() {
        Some(port) => vec![format!("{host}:{port}"), host.to_owned()],
        None => vec![host.to_owned()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_from_base_url() {
        assert_eq!(
            base_url_hosts("https://example.com:8443/x"),
            ["example.com:8443", "example.com"]
        );
        assert_eq!(base_url_hosts("https://Example.com/"), ["example.com"]);
        assert_eq!(base_url_hosts("http://[::1]:3000"), ["[::1]:3000", "[::1]"]);
        assert_eq!(
            base_url_hosts("https://user:pw@example.com"),
            ["example.com"]
        );
        assert_eq!(base_url_hosts("not a url"), Vec::<String>::new());
    }
}
