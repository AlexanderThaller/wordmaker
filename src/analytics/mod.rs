//! Page-view analytics, written to `ClickHouse` in the background.
//!
//! A request only hands a [`Hit`] to a bounded queue and never waits: when
//! the queue is full the hit is dropped. Everything else (`GeoIP`, user
//! agent parsing, the visitor hash, talking to `ClickHouse`) happens in the
//! sink task, which buffers while `ClickHouse` is unreachable. Client IPs
//! are only used for the `GeoIP` lookup and the visitor hash, never stored.

pub mod download;
mod geo;
pub mod report;
mod sink;
mod ua;

use std::{
    net::{
        IpAddr,
        SocketAddr,
    },
    path::PathBuf,
    sync::{
        Arc,
        atomic::{
            AtomicU64,
            Ordering,
        },
    },
    time::Duration,
};

use chrono::{
    DateTime,
    Utc,
};
use tokio::{
    sync::mpsc,
    task::JoinHandle,
};

use crate::token::read_secret;

/// Hits waiting for the sink; more are dropped rather than slowing requests.
const QUEUE: usize = 10_000;

/// Longest kept path (and slug), query and referrer, in bytes. Requests may
/// carry far longer ones, and every hit is held in memory until written.
const MAX_URL: usize = 1024;
/// Longest kept user agent, in bytes.
const MAX_USER_AGENT: usize = 512;
/// Longest kept `Accept-Language`; only its first tag is used.
const MAX_ACCEPT_LANGUAGE: usize = 128;

/// Where to send page views; without a `ClickHouse` URL nothing is recorded.
#[derive(clap::Args, Debug, Default)]
pub struct AnalyticsArgs {
    /// `ClickHouse` HTTP endpoint for page-view analytics, e.g.
    /// `http://127.0.0.1:8123`. Analytics is off without it.
    #[arg(long, env = "WORDMAKER_CLICKHOUSE_URL", value_name = "URL")]
    clickhouse_url: Option<String>,

    /// `ClickHouse` database holding the `page_views` table, shared by all
    /// instances; it must exist, the table is created on start.
    #[arg(
        long,
        env = "WORDMAKER_CLICKHOUSE_DATABASE",
        default_value = "wordmaker"
    )]
    clickhouse_database: String,

    /// `ClickHouse` user.
    #[arg(long, env = "WORDMAKER_CLICKHOUSE_USER", default_value = "default")]
    clickhouse_user: String,

    /// `ClickHouse` password.
    #[arg(
        long,
        env = "WORDMAKER_CLICKHOUSE_PASSWORD",
        hide_env_values = true,
        conflicts_with = "clickhouse_password_file"
    )]
    clickhouse_password: Option<String>,

    /// File holding the `ClickHouse` password. Surrounding whitespace is
    /// ignored.
    #[arg(long, env = "WORDMAKER_CLICKHOUSE_PASSWORD_FILE", value_name = "PATH")]
    clickhouse_password_file: Option<PathBuf>,

    /// `MaxMind` `GeoLite2-City` (or `GeoIP2-City`) database for country,
    /// region and city.
    #[arg(long, env = "WORDMAKER_GEOIP_CITY_DB", value_name = "PATH")]
    geoip_city_db: Option<PathBuf>,

    /// `MaxMind` `GeoLite2-ASN` database for the visitor's network.
    #[arg(long, env = "WORDMAKER_GEOIP_ASN_DB", value_name = "PATH")]
    geoip_asn_db: Option<PathBuf>,

    /// Reverse proxies in front of the server that append to
    /// `X-Forwarded-For`; the client address is that many entries from the
    /// right. 0 ignores the header and uses the connection's address.
    #[arg(long, env = "WORDMAKER_PROXY_HOPS", default_value_t = 1)]
    proxy_hops: usize,
}

impl AnalyticsArgs {
    /// Starts the sink if analytics is configured. `site` tags every page
    /// view, so instances sharing a database can be told apart.
    pub fn start(&self, site: &str) -> anyhow::Result<Option<(Analytics, JoinHandle<()>)>> {
        let Some(url) = &self.clickhouse_url else {
            return Ok(None);
        };
        let mut client = sink::client(url)
            .with_database(&self.clickhouse_database)
            .with_user(&self.clickhouse_user);
        if let Some(password) = read_secret(
            self.clickhouse_password.as_deref(),
            self.clickhouse_password_file.as_deref(),
            "ClickHouse password",
        )? {
            client = client.with_password(password);
        }
        let geo = geo::Geo::open(self.geoip_city_db.as_deref(), self.geoip_asn_db.as_deref());
        tracing::info!(
            "analytics: page views go to {}, database {}",
            redact(url),
            self.clickhouse_database
        );
        Ok(Some(Analytics::start(client, site, geo, self.proxy_hops)))
    }
}

/// `url` for the log, without credentials someone may have put in it.
fn redact(url: &str) -> String {
    match url::Url::parse(url) {
        // `user:password@host` parses too, as a path after `user:`.
        Ok(mut url) if url.has_host() => {
            if !url.username().is_empty() || url.password().is_some() {
                // Cannot fail: a URL with credentials has a host.
                let _ = url.set_username("***");
                let _ = url.set_password(None);
            }
            url.into()
        }
        _ => "an unparsable URL".to_owned(),
    }
}

/// Handle for recording page views; cheap to clone.
#[derive(Clone, Debug)]
pub struct Analytics {
    queue: mpsc::Sender<Hit>,
    dropped: Arc<AtomicU64>,
    proxy_hops: usize,
    /// For reading the numbers back.
    client: clickhouse::Client,
    /// The `site` column of this instance's page views.
    site: Arc<str>,
    geo_databases: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Analytics {
    fn start(
        client: clickhouse::Client,
        site: &str,
        geo: geo::Geo,
        proxy_hops: usize,
    ) -> (Self, JoinHandle<()>) {
        let (queue, hits) = mpsc::channel(QUEUE);
        let dropped = Arc::default();
        let site = site.trim_end_matches('/');
        let geo_databases = geo.types();
        let sink = sink::Sink::new(client.clone(), site, geo, hits, Arc::clone(&dropped));
        let handle = tokio::spawn(sink.run());
        (
            Self {
                queue,
                dropped,
                proxy_hops,
                client,
                site: site.into(),
                geo_databases,
            },
            handle,
        )
    }

    /// A handle whose hits end up in the returned receiver, for tests.
    #[cfg(test)]
    pub fn channel(proxy_hops: usize) -> (Self, mpsc::Receiver<Hit>) {
        let (queue, hits) = mpsc::channel(QUEUE);
        (
            Self {
                queue,
                dropped: Arc::default(),
                proxy_hops,
                client: clickhouse::Client::default(),
                site: "http://localhost:3000".into(),
                geo_databases: Arc::default(),
            },
            hits,
        )
    }

    /// Queues a hit without waiting; when the queue is full it is dropped
    /// and counted, and the sink reports the count.
    pub fn record(&self, hit: Hit) {
        if let Err(mpsc::error::TrySendError::Full(_)) = self.queue.try_send(hit.truncated()) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// This site's page views over the last `days` days.
    pub async fn report(&self, days: u32) -> anyhow::Result<report::Report> {
        let mut report = tokio::time::timeout(
            report::TIMEOUT,
            report::report(&self.client, &self.site, days),
        )
        .await
        .map_err(|_| anyhow::anyhow!("ClickHouse did not answer in time"))??;
        report.geo_databases = self
            .geo_databases
            .lock()
            .map_or_default(|types| types.clone());
        Ok(report)
    }

    /// The client's address: `proxy_hops` entries from the right of
    /// `X-Forwarded-For`, or the connection's peer.
    pub fn client_ip(
        &self,
        forwarded_for: Option<&str>,
        peer: Option<SocketAddr>,
    ) -> Option<IpAddr> {
        client_ip(forwarded_for, peer, self.proxy_hops)
    }
}

/// What a request tells us about a page view, before enrichment.
#[derive(Debug)]
pub struct Hit {
    pub time: DateTime<Utc>,
    pub ip: Option<IpAddr>,
    pub path: String,
    pub query: Option<String>,
    pub slug: String,
    pub status: u16,
    pub duration: Duration,
    pub user_agent: String,
    pub referrer: String,
    pub accept_language: String,
}

impl Hit {
    /// The hit with every field cut to its limit, so neither the queue nor
    /// the sink's buffer can be filled with huge headers.
    fn truncated(mut self) -> Self {
        truncate(&mut self.path, MAX_URL);
        truncate(&mut self.slug, MAX_URL);
        if let Some(query) = &mut self.query {
            truncate(query, MAX_URL);
        }
        truncate(&mut self.referrer, MAX_URL);
        truncate(&mut self.user_agent, MAX_USER_AGENT);
        truncate(&mut self.accept_language, MAX_ACCEPT_LANGUAGE);
        self
    }
}

/// Cuts `text` to at most `max` bytes, at a character boundary.
pub(crate) fn truncate(text: &mut String, max: usize) {
    if text.len() > max {
        text.truncate(text.floor_char_boundary(max));
        text.shrink_to_fit();
    }
}

fn client_ip(
    forwarded_for: Option<&str>,
    peer: Option<SocketAddr>,
    proxy_hops: usize,
) -> Option<IpAddr> {
    let forwarded = forwarded_for
        .filter(|_| proxy_hops > 0)
        .and_then(|header| header.rsplit(',').nth(proxy_hops - 1))
        .and_then(|entry| parse_forwarded_ip(entry.trim()));
    forwarded.or(peer.map(|peer| peer.ip()))
}

/// An address as proxies write it: bare, or with a port (`1.2.3.4:5`,
/// `[::1]:5`).
fn parse_forwarded_ip(entry: &str) -> Option<IpAddr> {
    if let Ok(ip) = entry.parse() {
        return Some(ip);
    }
    if let Ok(addr) = entry.parse::<SocketAddr>() {
        return Some(addr.ip());
    }
    entry
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .and_then(|ip| ip.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_ip_from_forwarded_for() {
        let peer = Some(SocketAddr::from(([127, 0, 0, 1], 4000)));
        let ip = |header, hops| client_ip(header, peer, hops).unwrap().to_string();

        assert_eq!(ip(Some("203.0.113.7"), 1), "203.0.113.7");
        assert_eq!(ip(Some("10.0.0.1, 203.0.113.7"), 1), "203.0.113.7");
        assert_eq!(ip(Some("203.0.113.7, 10.0.0.1"), 2), "203.0.113.7");
        assert_eq!(ip(Some("2001:db8::1"), 1), "2001:db8::1");
        assert_eq!(ip(Some("[2001:db8::1]:443"), 1), "2001:db8::1");
        assert_eq!(ip(Some("203.0.113.7:8080"), 1), "203.0.113.7");

        // Falls back to the peer.
        assert_eq!(ip(Some("203.0.113.7"), 0), "127.0.0.1");
        assert_eq!(ip(Some("garbage"), 1), "127.0.0.1");
        assert_eq!(ip(Some("203.0.113.7"), 2), "127.0.0.1");
        assert_eq!(ip(None, 1), "127.0.0.1");
        assert_eq!(client_ip(None, None, 1), None);
    }

    #[test]
    fn logged_urls_lose_credentials() {
        assert_eq!(
            redact("http://user:hunter2@clickhouse:8123/"),
            "http://***@clickhouse:8123/"
        );
        assert_eq!(redact("http://:hunter2@clickhouse"), "http://***@clickhouse/");
        assert_eq!(redact("http://127.0.0.1:8123"), "http://127.0.0.1:8123/");
        assert_eq!(redact("user:hunter2@clickhouse"), "an unparsable URL");
    }

    #[test]
    fn hits_are_truncated() {
        let (analytics, mut hits) = Analytics::channel(1);
        let long = "ä".repeat(100_000);
        analytics.record(Hit {
            time: Utc::now(),
            ip: None,
            path: format!("/{long}"),
            query: Some(long.clone()),
            slug: long.clone(),
            status: 404,
            duration: Duration::ZERO,
            user_agent: long.clone(),
            referrer: long.clone(),
            accept_language: long,
        });
        let hit = hits.try_recv().unwrap();
        assert_eq!(hit.path.len(), MAX_URL - 1, "cut before a split character");
        assert!(hit.path.starts_with("/ä"));
        assert_eq!(hit.slug.len(), MAX_URL);
        assert_eq!(hit.query.unwrap().len(), MAX_URL);
        assert_eq!(hit.referrer.len(), MAX_URL);
        assert_eq!(hit.user_agent.len(), MAX_USER_AGENT);
        assert_eq!(hit.accept_language.len(), MAX_ACCEPT_LANGUAGE);
    }

    #[test]
    fn password_from_file() {
        let path = std::env::temp_dir().join(format!(
            "wordmaker-clickhouse-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::write(&path, "hunter2\n").unwrap();
        let args = AnalyticsArgs {
            clickhouse_password_file: Some(path.clone()),
            ..AnalyticsArgs::default()
        };
        let password = read_secret(
            args.clickhouse_password.as_deref(),
            args.clickhouse_password_file.as_deref(),
            "ClickHouse password",
        )
        .unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(password.as_deref(), Some("hunter2"));
    }
}
