mod app;
mod auth;
mod proxy;
mod token;

use std::net::SocketAddr;

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::{
    app::Admin,
    proxy::Upstream,
};

/// Web interface for a running `WordMaker` server: write, preview and publish
/// pages, manage media and site settings.
///
/// It holds the API token itself, so the browser never sees it, and passes the
/// browser's requests on to the server's REST API.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Address to listen on. Without sign-in (`--oidc-issuer`) anyone who can
    /// reach it can change the site, so keep it on loopback unless something
    /// in front of it checks who is asking.
    #[arg(long, env = "WORDMAKER_ADMIN_BIND", default_value = "127.0.0.1:3001")]
    bind: SocketAddr,

    /// Where the `WordMaker` server is reached.
    #[arg(
        long,
        env = "WORDMAKER_BASE_URL",
        default_value = "http://127.0.0.1:3000"
    )]
    base_url: String,

    /// Where visitors reach the site, for the interface's links to it, when
    /// that is not `--base-url` (say, the server is reached internally).
    #[arg(long, env = "WORDMAKER_ADMIN_SITE_URL")]
    site_url: Option<String>,

    #[command(flatten)]
    token: token::TokenArgs,

    /// Extra `Host` values the interface answers to, besides loopback and the
    /// bind address.
    #[arg(
        long = "allowed-host",
        env = "WORDMAKER_ADMIN_ALLOWED_HOSTS",
        value_delimiter = ','
    )]
    allowed_hosts: Vec<String>,

    #[command(flatten)]
    oidc: auth::OidcArgs,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let token = cli.token.require(" to the server's API token")?;

    let mut allowed_hosts: Vec<String> = ["localhost", "127.0.0.1", "::1"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    if !cli.bind.ip().is_unspecified() {
        allowed_hosts.push(cli.bind.ip().to_string());
    }
    allowed_hosts.extend(cli.allowed_hosts);

    let auth = if let Some(oidc) = cli.oidc.resolve()? {
        if let Some(host) = oidc.public_url().host_str() {
            allowed_hosts.push(host.trim_matches(['[', ']']).to_owned());
        }
        if oidc.public_url().scheme() != "https" && !cli.bind.ip().is_loopback() {
            tracing::warn!(
                "the public URL is not https, so session cookies cross the network in the clear"
            );
        }
        Some(auth::Auth::new(oidc).await?)
    } else {
        if !cli.bind.ip().is_loopback() {
            tracing::warn!(
                "listening beyond loopback without sign-in; set WORDMAKER_ADMIN_OIDC_ISSUER"
            );
        }
        None
    };

    let site_url = match cli.site_url.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(url) => {
            anyhow::ensure!(
                url.starts_with("http://") || url.starts_with("https://"),
                "the site URL must start with http:// or https://, got {url:?}"
            );
            Some(url.trim_end_matches('/').into())
        }
    };

    let admin = Admin {
        upstream: Upstream::new(&cli.base_url, token)?,
        site_url,
        allowed_hosts: allowed_hosts.into(),
        auth,
    };
    let router = app::router(admin);

    let listener = tokio::net::TcpListener::bind(cli.bind)
        .await
        .with_context(|| format!("cannot listen on {}", cli.bind))?;
    tracing::info!(
        "admin interface on http://{}, editing {}",
        cli.bind,
        cli.base_url
    );
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
