//! Who a client is: either registered with us, or described by a client ID
//! metadata document at the HTTPS URL it uses as its `client_id`.

use std::{
    collections::HashMap,
    net::{
        IpAddr,
        SocketAddr,
    },
    sync::{
        Arc,
        Mutex,
    },
    time::{
        Duration,
        Instant,
    },
};

use serde::Deserialize;
use tokio::sync::Semaphore;
use url::{
    Host,
    Url,
};

/// How long a fetched metadata document is trusted.
const CACHE_TTL: Duration = Duration::from_mins(10);
/// How long a failed fetch is remembered, so that repeating a request does
/// not repeat the fetch.
const FAILURE_TTL: Duration = Duration::from_mins(1);
const CACHE_ENTRIES: usize = 256;
/// Fetches are started by anonymous requests; this bounds how many run at
/// once.
const CONCURRENT_FETCHES: usize = 4;
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
/// Metadata documents are small; anything larger is not one.
const MAX_DOCUMENT_BYTES: usize = 16 * 1024;
/// What the user is told when the document cannot be fetched. The details
/// are only logged: they would tell anyone how names resolve and which
/// hosts answer from here.
const FETCH_FAILED: &str = "the client metadata document could not be fetched";

/// A client as far as the authorization flow is concerned.
#[derive(Debug, Clone)]
pub struct ClientInfo {
    pub client_id: String,
    pub client_name: Option<String>,
    pub redirect_uris: Vec<String>,
    /// Whether the client proved who it is by serving its metadata from
    /// its `client_id` URL; registered names are whatever the client said.
    pub verified: bool,
}

type Fetched = Result<Arc<ClientInfo>, String>;

/// Recently fetched metadata documents, and recent failures.
#[derive(Debug)]
pub struct ClientCache {
    entries: Mutex<HashMap<String, (Instant, Fetched)>>,
    fetching: Semaphore,
}

impl Default for ClientCache {
    fn default() -> Self {
        Self {
            entries: Mutex::default(),
            fetching: Semaphore::new(CONCURRENT_FETCHES),
        }
    }
}

/// `client_id` values that name a metadata document.
pub fn is_metadata_url(client_id: &str) -> bool {
    client_id.starts_with("https://")
}

fn still_valid(fetched: Instant, result: &Fetched) -> bool {
    let ttl = if result.is_ok() {
        CACHE_TTL
    } else {
        FAILURE_TTL
    };
    fetched.elapsed() < ttl
}

impl ClientCache {
    pub async fn fetch(&self, client_id: &str) -> Fetched {
        if let Some((fetched, result)) = self.lock().get(client_id)
            && still_valid(*fetched, result)
        {
            return result.clone();
        }
        let Ok(_permit) = self.fetching.try_acquire() else {
            return Err("too many clients are being verified right now; try again".into());
        };
        let result = fetch(client_id).await.map(Arc::new);
        self.insert(client_id, result.clone());
        result
    }

    /// Remember `result`, making room by dropping expired entries and then
    /// the oldest one.
    fn insert(&self, client_id: &str, result: Fetched) {
        let mut entries = self.lock();
        entries.retain(|_, (fetched, result)| still_valid(*fetched, result));
        if entries.len() >= CACHE_ENTRIES
            && !entries.contains_key(client_id)
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, (fetched, _))| *fetched)
                .map(|(id, _)| id.clone())
        {
            entries.remove(&oldest);
        }
        entries.insert(client_id.to_owned(), (Instant::now(), result));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, (Instant, Fetched)>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[derive(Deserialize)]
struct Document {
    client_id: String,
    client_name: Option<String>,
    redirect_uris: Vec<String>,
    token_endpoint_auth_method: Option<String>,
}

async fn fetch(client_id: &str) -> Result<ClientInfo, String> {
    let url = Url::parse(client_id).map_err(|_| "client_id is not a valid URL".to_owned())?;
    if url.scheme() != "https"
        || url.path() == "/"
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("client_id must be an https URL with a path".into());
    }
    let body = download(&url).await.map_err(|err| {
        tracing::info!(client_id, error = %err, "could not fetch client metadata");
        FETCH_FAILED.to_owned()
    })?;
    parse_document(client_id, &body)
}

async fn download(url: &Url) -> Result<Vec<u8>, String> {
    let host = url.host_str().ok_or("client_id has no host")?.to_owned();
    let port = url.port_or_known_default().unwrap_or(443);

    // Resolve once, refuse anything that is not on the public internet,
    // and connect to exactly the address that was checked, so the server
    // cannot be pointed at itself or the local network.
    let address = match url.host() {
        Some(Host::Ipv4(ip)) => SocketAddr::new(IpAddr::V4(ip), port),
        Some(Host::Ipv6(ip)) => SocketAddr::new(IpAddr::V6(ip), port),
        _ => {
            let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
                .await
                .map_err(|err| format!("could not resolve {host}: {err}"))?
                .collect();
            if addresses.iter().any(|a| !is_public(a.ip())) {
                return Err(format!("{host} resolves to a non-public address"));
            }
            *addresses
                .first()
                .ok_or_else(|| format!("{host} has no addresses"))?
        }
    };
    if !is_public(address.ip()) {
        return Err(format!("{host} is not a public address"));
    }

    // No proxy: a proxy would resolve the host itself, past the check.
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(FETCH_TIMEOUT)
        .resolve(&host, address)
        .build()
        .map_err(|err| err.to_string())?;
    let mut response = client
        .get(url.clone())
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|err| format!("could not fetch client metadata: {err}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "client metadata answered with HTTP {}",
            response.status()
        ));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|err| format!("could not read client metadata: {err}"))?
    {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_DOCUMENT_BYTES {
            return Err("client metadata document is too large".into());
        }
    }
    Ok(body)
}

fn parse_document(client_id: &str, body: &[u8]) -> Result<ClientInfo, String> {
    let document: Document = serde_json::from_slice(body)
        .map_err(|err| format!("client metadata is not valid: {err}"))?;
    if document.client_id != client_id {
        return Err("client metadata names a different client_id".into());
    }
    if document.redirect_uris.is_empty()
        || !document
            .redirect_uris
            .iter()
            .all(|uri| valid_redirect_uri(uri))
    {
        return Err("client metadata has no acceptable redirect_uris".into());
    }
    if document
        .token_endpoint_auth_method
        .as_deref()
        .is_some_and(|method| method != "none")
    {
        return Err(
            "only public clients (token_endpoint_auth_method \"none\") are supported".into(),
        );
    }
    Ok(ClientInfo {
        client_id: document.client_id,
        client_name: document.client_name,
        redirect_uris: document.redirect_uris,
        verified: true,
    })
}

/// Private-use schemes of well-known clients that do not follow the
/// reverse domain name convention.
const APP_SCHEMES: &[&str] = &["cursor", "vscode", "vscode-insiders"];

/// Redirect URIs we hand codes to: HTTPS, HTTP on the loopback interface,
/// or an app's own scheme named after a reverse domain name, such as
/// `com.example.app:` (RFC 8252, section 7.1). Other schemes could open
/// any protocol handler of the operating system.
pub fn valid_redirect_uri(uri: &str) -> bool {
    let Ok(url) = Url::parse(uri) else {
        return false;
    };
    if url.fragment().is_some() || uri.len() > 2000 {
        return false;
    }
    match url.scheme() {
        "https" => url.host().is_some(),
        "http" => is_loopback(&url),
        scheme => scheme.contains('.') || APP_SCHEMES.contains(&scheme),
    }
}

/// Whether `presented` is one of the registered redirect URIs. Loopback
/// redirects may use any port, since native apps pick a free one when
/// they start (RFC 8252, section 7.3).
pub fn redirect_matches(registered: &[String], presented: &str) -> bool {
    registered.iter().any(|registered| {
        if registered == presented {
            return true;
        }
        let (Ok(mut registered), Ok(mut presented)) =
            (Url::parse(registered), Url::parse(presented))
        else {
            return false;
        };
        if registered.scheme() != "http" || !is_loopback(&registered) {
            return false;
        }
        let _ = registered.set_port(None);
        let _ = presented.set_port(None);
        registered == presented
    })
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(domain)) => domain == "localhost",
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// Addresses on the public internet: not loopback, private, link-local,
/// shared, documentation, multicast or otherwise reserved. IPv6 tunnel
/// ranges (6to4, Teredo) are refused, since they lead to IPv4 addresses
/// that cannot be checked.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, ..] = ip.octets();
            !(ip.is_unspecified()
                || ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_multicast()
                || a == 0
                || a >= 240
                || (a == 100 && (64..128).contains(&b))
                || (a == 198 && (b == 18 || b == 19))
                || (a == 192 && b == 0))
        }
        IpAddr::V6(ip) => {
            // IPv4-mapped (`::ffff:a.b.c.d`) and IPv4-compatible
            // (`::a.b.c.d`, which includes `::` and `::1`) addresses are
            // judged by the IPv4 address they carry.
            if let Some(v4) = ip.to_ipv4() {
                return is_public(IpAddr::V4(v4));
            }
            let segments = ip.segments();
            let first = segments[0];
            !(ip.is_multicast()
                || (first & 0xfe00) == 0xfc00 // unique local
                || (first & 0xffc0) == 0xfe80 // link-local
                || (first & 0xffc0) == 0xfec0 // site-local
                || first == 0x0064 // NAT64
                || (first == 0x0100 && segments[1..4] == [0, 0, 0]) // discard
                || (first == 0x2001 && segments[1] < 0x0200) // Teredo, benchmarking, ORCHID
                || (first == 0x2001 && segments[1] == 0x0db8) // documentation
                || first == 0x2002 // 6to4, which embeds an IPv4 address
                || (first == 0x3fff && segments[1] < 0x1000)) // documentation
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_addresses() {
        for ip in [
            "1.1.1.1",
            "2a00:1450:4001::1",
            "2606:4700:4700::1111",
            "100:1::1",
            "2001:4860::1",
        ] {
            assert!(is_public(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::",
            "::7f00:1",
            "::10.0.0.1",
            "2002:7f00:1::1",
            "2001::1",
            "2001:0:4136:e378:8000:63bf:3fff:fdd2",
            "fec0::1",
            "100::1",
            "64:ff9b::7f00:1",
            "2001:db8::1",
            "3fff::1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn redirect_uris() {
        assert!(valid_redirect_uri(
            "https://claude.ai/api/mcp/auth_callback"
        ));
        assert!(valid_redirect_uri("http://127.0.0.1:33418/callback"));
        assert!(valid_redirect_uri("http://localhost/cb"));
        assert!(valid_redirect_uri(
            "cursor://anysphere.cursor-mcp/oauth/callback"
        ));
        assert!(!valid_redirect_uri("http://example.com/cb"));
        assert!(!valid_redirect_uri("https://example.com/cb#frag"));
        assert!(valid_redirect_uri("com.example.app:/oauth/callback"));
        assert!(!valid_redirect_uri("javascript:alert(1)"));
        assert!(!valid_redirect_uri("ms-msdt:/id"));
        assert!(!valid_redirect_uri("search-ms:query=x"));
        assert!(!valid_redirect_uri("intent://x"));
        assert!(!valid_redirect_uri("not a url"));
    }

    #[test]
    fn redirect_matching() {
        let registered = vec![
            "https://claude.ai/api/mcp/auth_callback".to_owned(),
            "http://127.0.0.1/callback".to_owned(),
        ];
        assert!(redirect_matches(
            &registered,
            "https://claude.ai/api/mcp/auth_callback"
        ));
        assert!(redirect_matches(
            &registered,
            "http://127.0.0.1:51234/callback"
        ));
        assert!(!redirect_matches(
            &registered,
            "https://claude.ai/api/mcp/auth_callback/x"
        ));
        assert!(!redirect_matches(
            &registered,
            "https://claude.ai:444/api/mcp/auth_callback"
        ));
        assert!(!redirect_matches(
            &registered,
            "http://127.0.0.1:51234/other"
        ));
    }

    #[test]
    fn metadata_documents() {
        let id = "https://app.example.com/client.json";
        let ok = br#"{"client_id": "https://app.example.com/client.json", "client_name": "App",
                      "redirect_uris": ["https://app.example.com/cb"]}"#;
        let info = parse_document(id, ok).unwrap();
        assert_eq!(info.client_name.as_deref(), Some("App"));
        assert!(info.verified);

        let other_id = br#"{"client_id": "https://evil.example/c.json", "redirect_uris": ["https://evil.example/cb"]}"#;
        assert!(parse_document(id, other_id).is_err());
        let bad_redirect = br#"{"client_id": "https://app.example.com/client.json", "redirect_uris": ["http://evil.example/cb"]}"#;
        assert!(parse_document(id, bad_redirect).is_err());
        let confidential = br#"{"client_id": "https://app.example.com/client.json", "redirect_uris": ["https://app.example.com/cb"], "token_endpoint_auth_method": "private_key_jwt"}"#;
        assert!(parse_document(id, confidential).is_err());
    }

    #[tokio::test]
    async fn refuses_local_metadata_urls() {
        let cache = ClientCache::default();
        for id in [
            "https://127.0.0.1/client.json",
            "https://localhost/client.json",
            "https://[::1]/client.json",
            "https://169.254.169.254/latest",
            "https://example.com/",
            "http://example.com/client.json",
        ] {
            assert!(cache.fetch(id).await.is_err(), "{id}");
        }
        // Local addresses get the same answer as unreachable hosts.
        let err = cache
            .fetch("https://127.0.0.1/client.json")
            .await
            .unwrap_err();
        assert_eq!(err, FETCH_FAILED);
        // Failures are remembered, so repeating the request costs nothing.
        assert!(cache.lock().contains_key("https://127.0.0.1/client.json"));
    }

    #[test]
    fn full_cache_drops_the_oldest_entry() {
        let cache = ClientCache::default();
        for i in 0..CACHE_ENTRIES {
            cache.insert(&format!("https://c{i}.example/c.json"), Err("x".into()));
        }
        cache.insert("https://new.example/c.json", Err("x".into()));
        let entries = cache.lock();
        assert_eq!(entries.len(), CACHE_ENTRIES);
        assert!(entries.contains_key("https://new.example/c.json"));
        assert!(!entries.contains_key("https://c0.example/c.json"));
    }
}
