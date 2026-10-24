use std::{
    collections::VecDeque,
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
    NaiveDate,
    Utc,
};
use clickhouse::Row;
use serde::{
    Deserialize,
    Serialize,
};
use sha2::{
    Digest,
    Sha256,
};
use tokio::{
    sync::mpsc,
    time::Instant,
};

use super::{
    Hit,
    geo::Geo,
    ua,
};

const TABLE: &str = "page_views";
/// How often buffered page views are written while `ClickHouse` is healthy.
const FLUSH_INTERVAL: Duration = Duration::from_secs(5);
/// Written at once without waiting for the interval.
const BATCH: usize = 1000;
/// Longest wait between attempts while `ClickHouse` is unreachable.
const MAX_BACKOFF: Duration = Duration::from_secs(300);
/// Page views kept while `ClickHouse` is unreachable; older ones are dropped.
const MAX_BUFFER: usize = 50_000;
/// The same limit in bytes, for page views with long fields.
const MAX_BUFFER_BYTES: usize = 64 << 20;
/// Longest a single `ClickHouse` call may take; inserts get a second more
/// per MiB they write.
const TIMEOUT: Duration = Duration::from_secs(10);
/// Longest kept UTM value.
const MAX_UTM: usize = 128;
/// How often the `GeoIP` databases are checked for a new version.
const GEO_CHECK: Duration = Duration::from_secs(60);

const CREATE_TABLE: &str = "CREATE TABLE IF NOT EXISTS page_views (
    timestamp       DateTime64(3, 'UTC'),
    site            LowCardinality(String),
    path            String,
    slug            String,
    status          UInt16,
    duration_ms     UInt32,
    referrer        String,
    referrer_host   LowCardinality(String),
    utm_source      LowCardinality(String),
    utm_medium      LowCardinality(String),
    utm_campaign    LowCardinality(String),
    user_agent      String,
    browser         LowCardinality(String),
    browser_version LowCardinality(String),
    os              LowCardinality(String),
    os_version      LowCardinality(String),
    device          LowCardinality(String),
    is_bot          Bool,
    language        LowCardinality(String),
    country_code    LowCardinality(String),
    country         LowCardinality(String),
    region          LowCardinality(String),
    city            LowCardinality(String),
    asn             UInt32,
    as_org          LowCardinality(String),
    visitor_id      UInt64
) ENGINE = MergeTree
PARTITION BY toYYYYMM(timestamp)
ORDER BY (site, toDate(timestamp), path, timestamp)
SETTINGS non_replicated_deduplication_window = 1000";

/// A client for the `ClickHouse` server at `url`, set up the way the sink
/// needs it.
pub fn client(url: &str) -> clickhouse::Client {
    clickhouse::Client::default()
        .with_url(url)
        // clickhouse-rs 0.15 cannot read lz4 responses from current servers
        // (26.x), which breaks the schema check before inserts; page-view
        // batches are small enough not to need compression.
        .with_compression(clickhouse::Compression::None)
}

/// A row of `page_views`.
#[derive(Clone, Debug, PartialEq, Eq, Row, Serialize, Deserialize)]
pub struct PageView {
    #[serde(with = "clickhouse::serde::chrono::datetime64::millis")]
    pub timestamp: DateTime<Utc>,
    pub site: String,
    pub path: String,
    pub slug: String,
    pub status: u16,
    pub duration_ms: u32,
    pub referrer: String,
    pub referrer_host: String,
    pub utm_source: String,
    pub utm_medium: String,
    pub utm_campaign: String,
    pub user_agent: String,
    pub browser: String,
    pub browser_version: String,
    pub os: String,
    pub os_version: String,
    pub device: String,
    pub is_bot: bool,
    pub language: String,
    pub country_code: String,
    pub country: String,
    pub region: String,
    pub city: String,
    pub asn: u32,
    pub as_org: String,
    /// Same visitor, same site, same UTC day; unlinkable across days.
    pub visitor_id: u64,
}

impl PageView {
    /// Roughly the memory the row takes up.
    fn size(&self) -> usize {
        let strings = [
            &self.site,
            &self.path,
            &self.slug,
            &self.referrer,
            &self.referrer_host,
            &self.utm_source,
            &self.utm_medium,
            &self.utm_campaign,
            &self.user_agent,
            &self.browser,
            &self.browser_version,
            &self.os,
            &self.os_version,
            &self.device,
            &self.language,
            &self.country_code,
            &self.country,
            &self.region,
            &self.city,
            &self.as_org,
        ];
        size_of::<Self>() + strings.iter().map(|s| s.len()).sum::<usize>()
    }
}

#[derive(Debug)]
pub struct Sink {
    client: clickhouse::Client,
    site: String,
    geo: Geo,
    hits: mpsc::Receiver<Hit>,
    /// Hits the request side could not queue.
    dropped: Arc<AtomicU64>,
    salt: DailySalt,
    buffer: VecDeque<PageView>,
    /// [`PageView::size`] of everything in `buffer`.
    buffer_bytes: usize,
    /// The insert being retried, if the last one failed.
    attempt: Option<Attempt>,
    table_ready: bool,
    healthy: bool,
}

impl Sink {
    pub fn new(
        client: clickhouse::Client,
        site: &str,
        geo: Geo,
        hits: mpsc::Receiver<Hit>,
        dropped: Arc<AtomicU64>,
    ) -> Self {
        Self {
            client,
            site: site.to_owned(),
            geo,
            hits,
            dropped,
            salt: DailySalt::default(),
            buffer: VecDeque::new(),
            buffer_bytes: 0,
            attempt: None,
            table_ready: false,
            healthy: true,
        }
    }

    /// Runs until every [`super::Analytics`] handle is gone, then writes
    /// what is left.
    pub async fn run(mut self) {
        // Fail early in the log if ClickHouse is misconfigured; page views
        // are buffered either way.
        if let Err(err) = self.ensure_table().await {
            self.went_down(&err);
        }
        let mut backoff = FLUSH_INTERVAL;
        let mut next_flush = Instant::now() + FLUSH_INTERVAL;
        let mut geo_check = tokio::time::interval_at(Instant::now() + GEO_CHECK, GEO_CHECK);
        geo_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                hit = self.hits.recv() => {
                    let Some(hit) = hit else { break };
                    let view = self.enrich(hit);
                    self.push(view);
                    if !(self.healthy && self.buffer.len() >= BATCH) {
                        continue;
                    }
                }
                () = tokio::time::sleep_until(next_flush) => {}
                _ = geo_check.tick() => {
                    self.geo.reload_if_changed().await;
                    continue;
                }
            }
            if self.flush().await {
                backoff = FLUSH_INTERVAL;
            } else {
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
            next_flush = Instant::now() + backoff;
        }
        if !self.flush().await && !self.buffer.is_empty() {
            tracing::warn!(
                "analytics: dropping {} page views on shutdown",
                self.buffer.len()
            );
        }
    }

    fn push(&mut self, view: PageView) {
        let size = view.size();
        while self.buffer.len() >= MAX_BUFFER || self.buffer_bytes + size > MAX_BUFFER_BYTES {
            let Some(oldest) = self.buffer.pop_front() else {
                break;
            };
            self.buffer_bytes -= oldest.size();
            self.dropped.fetch_add(1, Ordering::Relaxed);
            // The rows of a failed insert changed, so its retry must not be
            // mistaken for it; should it have arrived after all, its rows
            // are written twice.
            self.attempt = None;
        }
        self.buffer_bytes += size;
        self.buffer.push_back(view);
    }

    /// Writes the buffer; on failure it is kept for the next attempt.
    async fn flush(&mut self) -> bool {
        let dropped = self.dropped.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            tracing::warn!("analytics: dropped {dropped} page views, the queue was full");
        }
        let mut written = 0;
        while !self.buffer.is_empty() {
            // A timed-out insert may still have been written, so the same
            // rows are sent again with the same deduplication token, which
            // has ClickHouse ignore them the second time. Page views that
            // came in since go in the next insert.
            let attempt = self.attempt.get_or_insert_with(|| Attempt {
                token: uuid::Uuid::new_v4().to_string(),
                rows: self.buffer.len(),
                bytes: self.buffer_bytes,
            });
            let attempt = attempt.clone();
            let result = match self.ensure_table().await {
                Ok(()) => self.insert(&attempt).await,
                Err(err) => Err(err),
            };
            if let Err(err) = result {
                self.went_down(&err);
                return false;
            }
            self.buffer.drain(..attempt.rows);
            self.buffer_bytes -= attempt.bytes;
            self.attempt = None;
            written += attempt.rows;
        }
        if !self.healthy {
            tracing::info!("analytics: ClickHouse is back, wrote {written} buffered page views");
            self.healthy = true;
        }
        true
    }

    fn went_down(&mut self, err: &anyhow::Error) {
        if self.healthy {
            tracing::warn!("analytics: ClickHouse unavailable, buffering page views: {err:#}");
            self.healthy = false;
        }
    }

    async fn ensure_table(&mut self) -> anyhow::Result<()> {
        if self.table_ready {
            return Ok(());
        }
        tokio::time::timeout(TIMEOUT, self.client.query(CREATE_TABLE).execute())
            .await
            .map_err(|_| anyhow::anyhow!("creating the {TABLE} table timed out"))??;
        self.table_ready = true;
        Ok(())
    }

    /// Writes the first `attempt.rows` buffered page views.
    async fn insert(&self, attempt: &Attempt) -> anyhow::Result<()> {
        let insert = async {
            let mut insert = self
                .client
                .insert::<PageView>(TABLE)
                .await?
                .with_setting("insert_deduplication_token", &attempt.token);
            for view in self.buffer.iter().take(attempt.rows) {
                insert.write(view).await?;
            }
            insert.end().await
        };
        let mib = u64::try_from(attempt.bytes >> 20).unwrap_or(u64::MAX);
        tokio::time::timeout(TIMEOUT + Duration::from_secs(mib), insert)
            .await
            .map_err(|_| anyhow::anyhow!("writing page views timed out"))??;
        Ok(())
    }

    fn enrich(&mut self, hit: Hit) -> PageView {
        let agent = ua::parse(&hit.user_agent);
        let location = hit.ip.map_or_default(|ip| self.geo.lookup(ip));
        let visitor_id = {
            let salt = self.salt.for_day(hit.time.date_naive());
            let mut hash = Sha256::new();
            hash.update(salt);
            hash.update(self.site.as_bytes());
            hash.update([0]);
            hash.update(hit.ip.map_or_default(|ip| ip.to_string()));
            hash.update([0]);
            hash.update(hit.user_agent.as_bytes());
            let digest = hash.finalize();
            let mut id = [0; 8];
            id.copy_from_slice(&digest[..8]);
            u64::from_le_bytes(id)
        };
        let utm = |key: &str| {
            let mut value = hit
                .query
                .as_deref()
                .and_then(|query| {
                    url::form_urlencoded::parse(query.as_bytes())
                        .find(|(name, _)| name == key)
                        .map(|(_, value)| value.into_owned())
                })
                .unwrap_or_default();
            super::truncate(&mut value, MAX_UTM);
            value
        };
        let (referrer, referrer_host) = referrer(&hit.referrer);
        PageView {
            timestamp: hit.time,
            site: self.site.clone(),
            path: hit.path.clone(),
            slug: hit.slug.clone(),
            status: hit.status,
            duration_ms: u32::try_from(hit.duration.as_millis()).unwrap_or(u32::MAX),
            referrer,
            referrer_host,
            utm_source: utm("utm_source"),
            utm_medium: utm("utm_medium"),
            utm_campaign: utm("utm_campaign"),
            language: primary_language(&hit.accept_language),
            browser: agent.browser,
            browser_version: agent.browser_version,
            os: agent.os,
            os_version: agent.os_version,
            device: agent.device,
            is_bot: agent.is_bot,
            country_code: location.country_code,
            country: location.country,
            region: location.region,
            city: location.city,
            asn: location.asn,
            as_org: location.as_org,
            visitor_id,
            user_agent: hit.user_agent,
        }
    }
}

/// The `Referer` as stored, and its host: only scheme, host and path, since
/// the query string, fragment and user info can carry search terms, tokens
/// or email addresses. Anything that is not a URL is dropped.
fn referrer(referer: &str) -> (String, String) {
    let Ok(mut url) = url::Url::parse(referer) else {
        return (String::new(), String::new());
    };
    url.set_query(None);
    url.set_fragment(None);
    // Fails only for URLs that cannot have credentials in the first place.
    let _ = url.set_username("");
    let _ = url.set_password(None);
    let host = url.host_str().unwrap_or_default().to_owned();
    (url.into(), host)
}

/// An insert of the oldest `rows` buffered page views (`bytes` in size),
/// identified by `token` across retries.
#[derive(Clone, Debug)]
struct Attempt {
    token: String,
    rows: usize,
    bytes: usize,
}

/// `de-DE,de;q=0.9,en;q=0.8` yields `de`.
fn primary_language(accept_language: &str) -> String {
    accept_language
        .split([',', ';', '-'])
        .next()
        .unwrap_or_default()
        .trim()
        .to_lowercase()
        .chars()
        .filter(char::is_ascii_alphabetic)
        .collect()
}

/// Random bytes for the visitor hash, replaced every UTC day and only ever
/// held in memory, so a visitor ID cannot be traced back to an address or
/// linked to the same visitor on another day.
///
/// Hits are timed when a request starts but arrive when it ends, so around
/// midnight they come out of order; the previous day's salt is kept for
/// those, and a day's salt is never made twice.
#[derive(Debug, Default)]
struct DailySalt {
    /// The two latest days seen, newest first.
    days: Vec<(NaiveDate, [u8; 32])>,
    /// For a hit older than both, which only a day-long request makes.
    stale: [u8; 32],
}

impl DailySalt {
    fn for_day(&mut self, day: NaiveDate) -> &[u8; 32] {
        let index = match self.days.iter().position(|(known, _)| *known == day) {
            Some(index) => index,
            None if self.days.len() == 2 && day < self.days[1].0 => {
                self.stale = random_salt();
                return &self.stale;
            }
            None => {
                self.days.push((day, random_salt()));
                self.days
                    .sort_unstable_by_key(|(day, _)| std::cmp::Reverse(*day));
                self.days.truncate(2);
                self.days
                    .iter()
                    .position(|(known, _)| *known == day)
                    .unwrap_or(0)
            }
        };
        &self.days[index].1
    }
}

fn random_salt() -> [u8; 32] {
    let mut salt = [0; 32];
    salt[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    salt[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    salt
}

#[cfg(test)]
mod tests {
    use clickhouse::test::{
        Mock,
        handlers,
    };

    use super::*;

    fn hit(time: DateTime<Utc>) -> Hit {
        Hit {
            time,
            ip: Some("203.0.113.7".parse().unwrap()),
            path: "/hello".into(),
            query: Some("utm_source=newsletter&x=1".into()),
            slug: "hello".into(),
            status: 200,
            duration: Duration::from_millis(12),
            user_agent: "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
                         Chrome/129.0.0.0 Safari/537.36"
                .into(),
            referrer: "https://search.example/?q=x".into(),
            accept_language: "de-DE,de;q=0.9,en;q=0.8".into(),
        }
    }

    fn sink(client: clickhouse::Client) -> (Sink, mpsc::Sender<Hit>) {
        let (tx, rx) = mpsc::channel(16);
        let sink = Sink::new(
            client,
            "https://site.example",
            Geo::default(),
            rx,
            Arc::default(),
        );
        (sink, tx)
    }

    #[test]
    fn enriches_hits() {
        let (mut sink, _) = sink(clickhouse::Client::default());
        let view = sink.enrich(hit(Utc::now()));
        assert_eq!(view.site, "https://site.example");
        assert_eq!(view.browser, "Chrome");
        assert_eq!(view.device, "pc");
        assert!(!view.is_bot);
        assert_eq!(view.referrer, "https://search.example/");
        assert_eq!(view.referrer_host, "search.example");
        assert_eq!(view.utm_source, "newsletter");
        assert_eq!(view.utm_medium, "");
        assert_eq!(view.language, "de");
        assert_eq!(view.duration_ms, 12);
        assert_eq!(view.country_code, "", "no GeoIP database");
    }

    #[test]
    fn visitor_id_changes_with_the_day() {
        let (mut sink, _) = sink(clickhouse::Client::default());
        let day1 = DateTime::parse_from_rfc3339("2026-10-08T10:00:00Z")
            .unwrap()
            .to_utc();
        let later = DateTime::parse_from_rfc3339("2026-10-08T23:00:00Z")
            .unwrap()
            .to_utc();
        let day2 = DateTime::parse_from_rfc3339("2026-10-09T01:00:00Z")
            .unwrap()
            .to_utc();
        let first = sink.enrich(hit(day1)).visitor_id;
        assert_eq!(sink.enrich(hit(later)).visitor_id, first);
        assert_ne!(sink.enrich(hit(day2)).visitor_id, first);

        let mut other = hit(later);
        other.ip = Some("198.51.100.1".parse().unwrap());
        sink.salt.for_day(day1.date_naive());
        assert_ne!(sink.enrich(other).visitor_id, first);
    }

    #[test]
    fn buffer_is_capped_in_bytes() {
        let (mut sink, _) = sink(clickhouse::Client::default());
        let mut view = sink.enrich(hit(Utc::now()));
        view.user_agent = "x".repeat(1 << 20);
        for _ in 0..100 {
            sink.push(view.clone());
        }
        assert!(sink.buffer.len() < 64, "{}", sink.buffer.len());
        assert!(sink.buffer_bytes <= MAX_BUFFER_BYTES);
        assert_eq!(
            sink.buffer_bytes,
            sink.buffer.iter().map(PageView::size).sum::<usize>()
        );
        assert_eq!(
            sink.dropped.load(Ordering::Relaxed),
            100 - sink.buffer.len() as u64
        );
    }

    #[test]
    fn utm_values_are_truncated() {
        let (mut sink, _) = sink(clickhouse::Client::default());
        let mut long = hit(Utc::now());
        long.query = Some(format!("utm_campaign={}", "c".repeat(500)));
        assert_eq!(sink.enrich(long).utm_campaign.len(), MAX_UTM);
    }

    #[test]
    fn referrers_lose_query_and_credentials() {
        assert_eq!(
            referrer("https://user:pw@mail.example/inbox/read?to=a%40b.example&t=secret#msg"),
            (
                "https://mail.example/inbox/read".to_owned(),
                "mail.example".to_owned()
            )
        );
        assert_eq!(
            referrer("android-app://com.example.app/"),
            (
                "android-app://com.example.app/".to_owned(),
                "com.example.app".to_owned()
            )
        );
        assert_eq!(referrer(""), (String::new(), String::new()));
        assert_eq!(referrer("not a url?q=x"), (String::new(), String::new()));
    }

    #[test]
    fn visitor_id_survives_hits_out_of_order_around_midnight() {
        let (mut sink, _) = sink(clickhouse::Client::default());
        let at = |time| DateTime::parse_from_rfc3339(time).unwrap().to_utc();
        let before = sink.enrich(hit(at("2026-10-08T23:59:59.9Z"))).visitor_id;
        let after = sink.enrich(hit(at("2026-10-09T00:00:00.1Z"))).visitor_id;
        assert_ne!(before, after);
        // A slow request from before midnight, then fast ones after it.
        assert_eq!(
            sink.enrich(hit(at("2026-10-08T23:59:59.8Z"))).visitor_id,
            before
        );
        assert_eq!(
            sink.enrich(hit(at("2026-10-09T00:00:00.2Z"))).visitor_id,
            after
        );
        assert_eq!(
            sink.enrich(hit(at("2026-10-08T23:59:59.7Z"))).visitor_id,
            before
        );

        // Two days on, the oldest salt is gone.
        let later = sink.enrich(hit(at("2026-10-10T00:00:00Z"))).visitor_id;
        assert_ne!(later, after);
        assert_eq!(
            sink.enrich(hit(at("2026-10-09T23:59:59Z"))).visitor_id,
            after
        );
        assert_eq!(sink.salt.days.len(), 2);
        assert_ne!(
            sink.enrich(hit(at("2026-10-08T12:00:00Z"))).visitor_id,
            before
        );
    }

    #[test]
    fn primary_languages() {
        assert_eq!(primary_language("de-DE,de;q=0.9"), "de");
        assert_eq!(primary_language("EN"), "en");
        assert_eq!(primary_language("*"), "");
        assert_eq!(primary_language(""), "");
    }

    /// A real server in Docker, which unlike the mock checks rows against
    /// the table schema; `None` (and the test skipped) without Docker.
    async fn real_clickhouse() -> Option<(
        testcontainers_modules::testcontainers::ContainerAsync<
            testcontainers_modules::clickhouse::ClickHouse,
        >,
        clickhouse::Client,
    )> {
        use testcontainers_modules::{
            clickhouse::{
                CLICKHOUSE_PORT,
                ClickHouse,
            },
            testcontainers::{
                ImageExt,
                runners::AsyncRunner,
            },
        };

        let container = match ClickHouse::default()
            .with_tag("26.9")
            .with_env_var("CLICKHOUSE_DB", "wordmaker")
            .with_env_var("CLICKHOUSE_PASSWORD", "secret")
            .start()
            .await
        {
            Ok(container) => container,
            Err(err) => {
                eprintln!("skipped: cannot start ClickHouse in Docker: {err}");
                return None;
            }
        };
        let host = container.get_host().await.unwrap();
        let port = container.get_host_port_ipv4(CLICKHOUSE_PORT).await.unwrap();
        let db = client(&format!("http://{host}:{port}"))
            .with_database("wordmaker")
            .with_user("default")
            .with_password("secret");
        Some((container, db))
    }

    /// Runs a sink over `hits` until it has written them.
    async fn write(db: &clickhouse::Client, hits: impl IntoIterator<Item = Hit>) {
        let (sink, tx) = sink(db.clone());
        let task = tokio::spawn(sink.run());
        for hit in hits {
            tx.send(hit).await.unwrap();
        }
        drop(tx);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn writes_to_a_real_clickhouse() {
        let Some((_container, db)) = real_clickhouse().await else {
            return;
        };
        let time = DateTime::parse_from_rfc3339("2025-01-01T10:00:00.123Z")
            .unwrap()
            .to_utc();
        write(&db, [hit(time)]).await;

        let rows = db
            .query("SELECT ?fields FROM page_views")
            .fetch_all::<PageView>()
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].timestamp, time);
        assert_eq!(rows[0].site, "https://site.example");
        assert_eq!(rows[0].browser, "Chrome");
        assert_eq!(rows[0].utm_source, "newsletter");
    }

    #[tokio::test]
    async fn reports_from_a_real_clickhouse() {
        let Some((_container, db)) = real_clickhouse().await else {
            return;
        };
        let old = DateTime::parse_from_rfc3339("2025-01-01T10:00:00.123Z")
            .unwrap()
            .to_utc();
        let mut missing = hit(Utc::now());
        missing.status = 404;
        missing.path = "/gone".into();
        let bot = |user_agent: &str| Hit {
            user_agent: user_agent.into(),
            ..hit(Utc::now())
        };
        let bots = [
            bot("Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)"),
            bot("Googlebot/2.1"),
            bot("Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko; compatible; GPTBot/1.2)"),
            bot("Mozilla/5.0 (Linux; Android 5.0) Mobile Safari/537.36 (compatible; Bytespider)"),
            bot("Mozilla/5.0 (X11; Linux x86_64) HeadlessChrome/129.0.0.0 Safari/537.36"),
            bot("facebookexternalhit/1.1 (+http://www.facebook.com/externalhit_uatext.php)"),
            bot(""),
        ];
        write(
            &db,
            [hit(old), hit(Utc::now()), hit(Utc::now()), missing]
                .into_iter()
                .chain(bots),
        )
        .await;
        // GeoIP needs a database; a located view goes straight in instead.
        db.query(
            "INSERT INTO page_views (timestamp, site, path, status, country_code) \
             VALUES (now64(3), 'https://site.example', '/hello', 200, 'DE')",
        )
        .execute()
        .await
        .unwrap();

        let report = super::super::report::report(&db, "https://site.example", 7)
            .await
            .unwrap();
        assert_eq!(report.views, 3, "the one from 2025 is out of range");
        assert_eq!(report.bot_views, 7);
        assert_eq!(report.visitor_days, 2);
        assert_eq!(report.daily.len(), 7);
        assert_eq!(report.daily.last().unwrap().views, 3);
        let codes: Vec<_> = report
            .country_codes
            .iter()
            .map(|country| (country.name.as_str(), country.views))
            .collect();
        assert_eq!(
            codes,
            [("DE", 1)],
            "views without a country stay off the map"
        );
        assert_eq!(report.pages[0].name, "/hello");
        assert_eq!(report.referrers[0].name, "search.example");
        assert_eq!(report.countries[0].name, "Unknown");
        assert_eq!(report.browsers[0].name, "Chrome");
        assert_eq!(report.not_found[0].name, "/gone");
        let mut bots: Vec<_> = report
            .bots
            .iter()
            .map(|bot| (bot.name.as_str(), bot.views))
            .collect();
        bots.sort_unstable();
        assert_eq!(
            bots,
            [
                ("Bytespider", 1),
                ("GPTBot", 1),
                ("Googlebot", 2),
                ("HeadlessChrome", 1),
                ("No user agent", 1),
                ("facebookexternalhit", 1),
            ]
        );

        let other = super::super::report::report(&db, "https://other.example", 7)
            .await
            .unwrap();
        assert_eq!(other.views, 0, "sites sharing the table stay apart");
    }

    #[tokio::test]
    async fn writes_page_views() {
        let mock = Mock::new();
        mock.add(handlers::record_ddl());
        let recording = mock.add(handlers::record::<PageView>());
        let (sink, tx) = sink(clickhouse::Client::default().with_mock(&mock));
        let task = tokio::spawn(sink.run());

        tx.send(hit(Utc::now())).await.unwrap();
        drop(tx);
        task.await.unwrap();

        let rows: Vec<PageView> = recording.collect().await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].slug, "hello");
        assert_eq!(rows[0].browser, "Chrome");
    }

    #[tokio::test]
    async fn keeps_page_views_while_clickhouse_fails() {
        let mock = Mock::new();
        mock.add(handlers::record_ddl());
        mock.add(handlers::failure(
            clickhouse::test::status::INTERNAL_SERVER_ERROR,
        ));
        let recording = mock.add(handlers::record::<PageView>());
        let (mut sink, _tx) = sink(clickhouse::Client::default().with_mock(&mock));

        let time = DateTime::parse_from_rfc3339("2026-10-08T10:00:00.123Z")
            .unwrap()
            .to_utc();
        let view = sink.enrich(hit(time));
        sink.push(view.clone());
        assert!(!sink.flush().await);
        assert!(!sink.healthy);
        assert_eq!(sink.buffer.len(), 1, "kept for the next attempt");

        assert!(sink.flush().await);
        assert!(sink.healthy);
        assert!(sink.buffer.is_empty());
        let rows: Vec<PageView> = recording.collect().await;
        assert_eq!(rows, vec![view]);
    }

    #[tokio::test]
    async fn retries_the_same_rows_and_writes_newer_ones_separately() {
        let mock = Mock::new();
        mock.add(handlers::record_ddl());
        mock.add(handlers::failure(
            clickhouse::test::status::INTERNAL_SERVER_ERROR,
        ));
        let retried = mock.add(handlers::record::<PageView>());
        let newer = mock.add(handlers::record::<PageView>());
        let (mut sink, _tx) = sink(clickhouse::Client::default().with_mock(&mock));

        let time = DateTime::parse_from_rfc3339("2026-10-08T10:00:00.123Z")
            .unwrap()
            .to_utc();
        let mut first = sink.enrich(hit(time));
        first.path = "/first".into();
        sink.push(first.clone());
        assert!(!sink.flush().await);
        let token = sink.attempt.as_ref().unwrap().token.clone();

        let mut second = sink.enrich(hit(time));
        second.path = "/second".into();
        sink.push(second.clone());
        assert_eq!(sink.attempt.as_ref().unwrap().token, token, "kept");
        assert_eq!(sink.attempt.as_ref().unwrap().rows, 1);

        assert!(sink.flush().await);
        assert!(sink.buffer.is_empty());
        assert_eq!(sink.buffer_bytes, 0);
        assert!(sink.attempt.is_none());
        assert_eq!(retried.collect::<Vec<PageView>>().await, vec![first]);
        assert_eq!(newer.collect::<Vec<PageView>>().await, vec![second]);
    }

    #[tokio::test]
    async fn real_clickhouse_ignores_a_repeated_insert() {
        let Some((_container, db)) = real_clickhouse().await else {
            return;
        };
        let (mut sink, _tx) = sink(db.clone());
        for _ in 0..2 {
            let view = sink.enrich(hit(Utc::now()));
            sink.push(view);
        }
        sink.ensure_table().await.unwrap();
        let attempt = Attempt {
            token: "retry".into(),
            rows: 2,
            bytes: sink.buffer_bytes,
        };
        // As if the first answer had timed out.
        sink.insert(&attempt).await.unwrap();
        sink.insert(&attempt).await.unwrap();

        let count = db
            .query("SELECT count() FROM page_views")
            .fetch_one::<u64>()
            .await
            .unwrap();
        assert_eq!(count, 2);
    }
}
