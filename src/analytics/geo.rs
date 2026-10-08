use std::{
    net::IpAddr,
    path::{
        Path,
        PathBuf,
    },
    sync::{
        Arc,
        Mutex,
    },
    time::SystemTime,
};

use maxminddb::{
    Reader,
    geoip2,
};

/// Optional `MaxMind`-format databases; lookups without one leave its
/// fields empty.
#[derive(Debug, Default)]
pub struct Geo {
    city: Source,
    asn: Source,
    /// Types of the loaded databases, e.g. `DBIP-City-Lite`, for the
    /// dashboard's attribution; shared with whoever asked via [`Geo::types`].
    types: Arc<Mutex<Vec<String>>>,
}

/// One database file, reopened when it changes on disk.
#[derive(Default)]
struct Source {
    path: Option<PathBuf>,
    what: &'static str,
    reader: Option<Reader<Vec<u8>>>,
    /// The file's modification time and size when last opened (or tried).
    seen: Option<(SystemTime, u64)>,
}

impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Source")
            .field("path", &self.path)
            .field("loaded", &self.reader.is_some())
            .finish_non_exhaustive()
    }
}

impl Source {
    fn new(path: Option<&Path>, what: &'static str) -> Self {
        let mut source = Self {
            path: path.map(Path::to_owned),
            what,
            ..Self::default()
        };
        if let Some(path) = &source.path {
            source.seen = stamp(path);
            source.reader = open(path, what);
        }
        source
    }

    /// Reopens the file if it changed since it was last opened. A file that
    /// cannot be opened leaves the loaded database in place.
    async fn reload_if_changed(&mut self) {
        let Some(path) = &self.path else { return };
        let seen = stamp(path);
        if seen.is_none() || seen == self.seen {
            return;
        }
        self.seen = seen;
        let (path, what) = (path.clone(), self.what);
        let opened = tokio::task::spawn_blocking(move || {
            let reader = open(&path, what)?;
            tracing::info!(
                "analytics: loaded the GeoIP {what} database {} ({})",
                path.display(),
                reader.metadata().database_type
            );
            Some(reader)
        })
        .await;
        if let Ok(Some(reader)) = opened {
            self.reader = Some(reader);
        }
    }
}

fn stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.modified().ok()?, metadata.len()))
}

/// Where a visitor is, as far as `GeoIP` knows.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Location {
    pub country_code: String,
    pub country: String,
    pub region: String,
    pub city: String,
    pub asn: u32,
    pub as_org: String,
}

impl Geo {
    /// Opens the databases that are given. One that cannot be opened is
    /// logged and left out: analytics must not keep the site from starting.
    pub fn open(city: Option<&Path>, asn: Option<&Path>) -> Self {
        let geo = Self {
            city: Source::new(city, "city"),
            asn: Source::new(asn, "ASN"),
            types: Arc::default(),
        };
        geo.publish_types();
        geo
    }

    /// The types of the loaded databases, kept current across reloads.
    pub fn types(&self) -> Arc<Mutex<Vec<String>>> {
        Arc::clone(&self.types)
    }

    fn publish_types(&self) {
        let types = [&self.city, &self.asn]
            .into_iter()
            .filter_map(|source| source.reader.as_ref())
            .map(|reader| reader.metadata().database_type.clone())
            .collect();
        if let Ok(mut shared) = self.types.lock() {
            *shared = types;
        }
    }

    /// Picks up databases that were replaced, or created since, e.g. by
    /// `wordmaker geoip` or `geoipupdate`.
    pub async fn reload_if_changed(&mut self) {
        self.city.reload_if_changed().await;
        self.asn.reload_if_changed().await;
        self.publish_types();
    }

    pub fn lookup(&self, ip: IpAddr) -> Location {
        let city = self
            .city
            .reader
            .as_ref()
            .and_then(|reader| reader.lookup(ip).ok()?.decode::<geoip2::City<'_>>().ok()?)
            .unwrap_or_default();
        let asn = self
            .asn
            .reader
            .as_ref()
            .and_then(|reader| reader.lookup(ip).ok()?.decode::<geoip2::Asn<'_>>().ok()?);
        let (asn, as_org) = asn.map_or_default(|asn| {
            (
                asn.autonomous_system_number.unwrap_or_default(),
                asn.autonomous_system_organization.unwrap_or_default(),
            )
        });
        Location {
            country_code: city.country.iso_code.unwrap_or_default().to_owned(),
            country: city.country.names.english.unwrap_or_default().to_owned(),
            region: city
                .subdivisions
                .first()
                .and_then(|region| region.names.english)
                .unwrap_or_default()
                .to_owned(),
            city: city.city.names.english.unwrap_or_default().to_owned(),
            asn,
            as_org: as_org.to_owned(),
        }
    }
}

fn open(path: &Path, what: &str) -> Option<Reader<Vec<u8>>> {
    match Reader::open_readfile(path) {
        Ok(reader) => Some(reader),
        Err(err) => {
            tracing::warn!(
                "analytics: cannot open the GeoIP {what} database {}: {err}",
                path.display()
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_databases_leave_fields_empty() {
        let geo = Geo::open(Some(Path::new("/nonexistent/city.mmdb")), None);
        assert_eq!(geo.lookup("8.8.8.8".parse().unwrap()), Location::default());
    }

    #[tokio::test]
    async fn reloads_changed_files_and_keeps_the_old_one_on_errors() {
        let path = std::env::temp_dir().join(format!(
            "wordmaker-geo-{}.mmdb",
            uuid::Uuid::new_v4().simple()
        ));
        let mut geo = Geo::open(Some(&path), None);
        assert!(geo.city.reader.is_none(), "not there yet");

        // A file that is not a database is tried once, and not again until
        // it changes.
        std::fs::write(&path, "not a database").unwrap();
        geo.reload_if_changed().await;
        assert!(geo.city.reader.is_none());
        assert_eq!(geo.city.seen, stamp(&path));

        std::fs::write(&path, test_database()).unwrap();
        geo.reload_if_changed().await;
        assert!(geo.city.reader.is_some(), "loaded the new file");

        std::fs::write(&path, "broken replacement").unwrap();
        geo.reload_if_changed().await;
        assert!(geo.city.reader.is_some(), "kept the loaded database");

        std::fs::remove_file(&path).unwrap();
        geo.reload_if_changed().await;
        assert!(geo.city.reader.is_some(), "a missing file keeps it too");
    }

    /// The smallest valid database: an empty IPv4 tree and its metadata.
    fn test_database() -> Vec<u8> {
        fn map_header(len: u8) -> u8 {
            0b1110_0000 | len
        }
        fn utf8(out: &mut Vec<u8>, s: &str) {
            out.push(0b0100_0000 | u8::try_from(s.len()).unwrap());
            out.extend_from_slice(s.as_bytes());
        }
        fn uint16(out: &mut Vec<u8>, n: u8) {
            out.extend_from_slice(&[0b1010_0001, n]);
        }
        fn uint32(out: &mut Vec<u8>, n: u8) {
            out.extend_from_slice(&[0b1100_0001, n]);
        }
        fn uint64(out: &mut Vec<u8>, n: u8) {
            // Extended type 9: type bits 0, then 9 - 7 in the next byte.
            out.extend_from_slice(&[0b0000_0001, 2, n]);
        }
        // One node whose two 24-bit records both point at "no data" (= the
        // node count), followed by the 16-byte data section separator.
        let mut db = vec![0, 0, 1, 0, 0, 1];
        db.extend_from_slice(&[0; 16]);
        db.extend_from_slice(b"\xAB\xCD\xEFMaxMind.com");
        db.push(map_header(9));
        utf8(&mut db, "node_count");
        uint32(&mut db, 1);
        utf8(&mut db, "record_size");
        uint16(&mut db, 24);
        utf8(&mut db, "ip_version");
        uint16(&mut db, 4);
        utf8(&mut db, "database_type");
        utf8(&mut db, "Test");
        utf8(&mut db, "binary_format_major_version");
        uint16(&mut db, 2);
        utf8(&mut db, "binary_format_minor_version");
        uint16(&mut db, 0);
        utf8(&mut db, "description");
        db.push(map_header(0));
        utf8(&mut db, "languages");
        // Extended type 11 (array), empty.
        db.extend_from_slice(&[0b0000_0000, 4]);
        utf8(&mut db, "build_epoch");
        uint64(&mut db, 1);
        db
    }
}
