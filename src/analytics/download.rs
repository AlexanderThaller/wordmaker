//! `wordmaker geoip`: fetches the free DB-IP Lite databases, which use the
//! `MaxMind` format and work as `--geoip-city-db` and `--geoip-asn-db`.

use std::{
    io::Write,
    path::{
        Path,
        PathBuf,
    },
    time::Duration,
};

use anyhow::{
    Context,
    bail,
};
use chrono::{
    Datelike,
    NaiveDate,
    Utc,
};

const BASE_URL: &str = "https://download.db-ip.com/free";
/// Longest a download may take, and may stall without a byte arriving.
const TIMEOUT: Duration = Duration::from_mins(30);
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// Largest unpacked database accepted; the city database is about 130 MB.
const MAX_SIZE: u64 = 1 << 30;

/// A DB-IP Lite database and the file it is saved as.
struct Database {
    /// `city` or `asn`, as in `dbip-city-lite-2026-10.mmdb.gz`.
    kind: &'static str,
    file: &'static str,
    env: &'static str,
}

const DATABASES: &[Database] = &[
    Database {
        kind: "city",
        file: "dbip-city-lite.mmdb",
        env: "WORDMAKER_GEOIP_CITY_DB",
    },
    Database {
        kind: "asn",
        file: "dbip-asn-lite.mmdb",
        env: "WORDMAKER_GEOIP_ASN_DB",
    },
];

#[derive(clap::Args, Debug)]
pub struct GeoipArgs {
    /// Directory to save the databases in; existing ones are replaced.
    #[arg(long, env = "WORDMAKER_GEOIP_DIR", default_value = "geoip")]
    dir: PathBuf,
}

pub async fn run(args: &GeoipArgs) -> anyhow::Result<()> {
    std::fs::create_dir_all(&args.dir)
        .with_context(|| format!("cannot create {}", args.dir.display()))?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(READ_TIMEOUT)
        .timeout(TIMEOUT)
        .user_agent(concat!("wordmaker/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let today = Utc::now().date_naive();
    for database in DATABASES {
        let path = args.dir.join(database.file);
        let month = download(&client, database, today, &path).await?;
        eprintln!(
            "saved DB-IP {} Lite {month} as {}",
            database.kind,
            path.display()
        );
    }

    let dir = std::path::absolute(&args.dir).unwrap_or_else(|_| args.dir.clone());
    eprintln!(
        "\nIP geolocation by DB-IP (https://db-ip.com), licensed under CC BY 4.0.\nA running \
         `wordmaker serve` picks up new databases within a minute. To use them, set:"
    );
    for database in DATABASES {
        println!("{}={}", database.env, dir.join(database.file).display());
    }
    Ok(())
}

/// Saves this month's edition, or last month's while this month's is not
/// out yet, and returns which month it was.
async fn download(
    client: &reqwest::Client,
    database: &Database,
    today: NaiveDate,
    path: &Path,
) -> anyhow::Result<String> {
    for month in [month(today), month(previous_month(today))] {
        let url = format!("{BASE_URL}/dbip-{}-lite-{month}.mmdb.gz", database.kind);
        let mut response = client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("cannot download {url}"))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            continue;
        }
        if !response.status().is_success() {
            bail!("cannot download {url}: {}", response.status());
        }

        // Written next to the target and renamed once it checks out, so a
        // running server or a failed download never sees half a file. The
        // name is unique, so runs that overlap do not write the same file.
        let partial =
            path.with_extension(format!("mmdb.{}.partial", uuid::Uuid::new_v4().simple()));
        let file = std::fs::File::create_new(&partial)
            .with_context(|| format!("cannot write {}", partial.display()))?;
        let mut gunzip =
            flate2::write::GzDecoder::new(Limited::new(std::io::BufWriter::new(file), MAX_SIZE));
        let saved = async {
            while let Some(chunk) = response
                .chunk()
                .await
                .with_context(|| format!("download of {url} failed"))?
            {
                gunzip
                    .write_all(&chunk)
                    .with_context(|| format!("cannot unpack {url}"))?;
            }
            let file = gunzip
                .finish()
                .with_context(|| format!("cannot unpack {url}"))?
                .into_inner()
                .into_inner()
                .map_err(std::io::IntoInnerError::into_error)?;
            // On disk before it replaces the old one, so a crash cannot
            // leave a truncated database behind.
            file.sync_all()?;
            drop(file);
            let reader = maxminddb::Reader::open_readfile(&partial)
                .with_context(|| format!("{url} is not a valid database"))?;
            if !reader
                .metadata()
                .database_type
                .to_lowercase()
                .contains(database.kind)
            {
                bail!(
                    "{url} is a {} database, not {}",
                    reader.metadata().database_type,
                    database.kind
                );
            }
            drop(reader);
            std::fs::rename(&partial, path)
                .with_context(|| format!("cannot replace {}", path.display()))?;
            // And the rename too.
            if let Some(dir) = path.parent() {
                let dir = if dir.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    dir
                };
                std::fs::File::open(dir).and_then(|dir| dir.sync_all())?;
            }
            anyhow::Ok(())
        }
        .await;
        if saved.is_err() {
            let _ = std::fs::remove_file(&partial);
        }
        return saved.map(|()| month);
    }
    bail!(
        "no DB-IP {} Lite database for {} or the month before",
        database.kind,
        month(today)
    )
}

/// Passes writes on to `inner` until `limit` bytes, then fails, so a
/// decompression bomb cannot fill the disk.
struct Limited<W> {
    inner: W,
    left: u64,
}

impl<W> Limited<W> {
    fn new(inner: W, limit: u64) -> Self {
        Self { inner, left: limit }
    }

    fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: Write> Write for Limited<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if u64::try_from(buf.len()).unwrap_or(u64::MAX) > self.left {
            return Err(std::io::Error::other(format!(
                "database is larger than {MAX_SIZE} bytes"
            )));
        }
        let written = self.inner.write(buf)?;
        self.left -= u64::try_from(written).unwrap_or(u64::MAX);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn month(day: NaiveDate) -> String {
    day.format("%Y-%m").to_string()
}

fn previous_month(day: NaiveDate) -> NaiveDate {
    day.with_day(1)
        .and_then(|first| first.pred_opt())
        .unwrap_or(day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn months() {
        let day = |s| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        assert_eq!(month(day("2026-10-08")), "2026-10");
        assert_eq!(month(previous_month(day("2026-10-08"))), "2026-09");
        assert_eq!(month(previous_month(day("2026-01-01"))), "2025-12");
    }

    #[test]
    fn unpacking_stops_at_the_limit() {
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        gzip.write_all(&vec![0; 10_000]).unwrap();
        let packed = gzip.finish().unwrap();

        let unpack = |limit| {
            let mut gunzip = flate2::write::GzDecoder::new(Limited::new(Vec::new(), limit));
            gunzip.write_all(&packed)?;
            gunzip.finish().map(Limited::into_inner)
        };
        assert_eq!(unpack(10_000).unwrap().len(), 10_000);
        assert!(unpack(9_999).is_err());
    }
}
