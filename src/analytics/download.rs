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
        // running server or a failed download never sees half a file.
        let partial = path.with_extension("mmdb.partial");
        let file = std::fs::File::create(&partial)
            .with_context(|| format!("cannot write {}", partial.display()))?;
        let mut gunzip = flate2::write::GzDecoder::new(std::io::BufWriter::new(file));
        let saved = async {
            while let Some(chunk) = response
                .chunk()
                .await
                .with_context(|| format!("download of {url} failed"))?
            {
                gunzip.write_all(&chunk)?;
            }
            gunzip.finish()?.flush()?;
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
            std::fs::rename(&partial, path)
                .with_context(|| format!("cannot replace {}", path.display()))?;
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
}
