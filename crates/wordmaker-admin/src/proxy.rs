use std::time::Duration;

use anyhow::Context;
use axum::{
    Json,
    body::Body,
    extract::Request,
    http::{
        HeaderName,
        StatusCode,
        Uri,
        header,
        uri::PathAndQuery,
    },
    response::{
        IntoResponse,
        Response,
    },
};
use serde_json::json;

/// Largest request body passed on: room for the server's media uploads
/// (25 MiB by default) and its theme zips (100 MiB).
const MAX_BODY: usize = 128 * 1024 * 1024;

/// How long reaching the server may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a request may take as a whole, answer included; installing a
/// theme from a URL is about the slowest the server gets: up to 10 s to
/// resolve and 2 min to download, then unpacking, so this leaves room.
const REQUEST_TIMEOUT: Duration = Duration::from_mins(4);
/// The same for requests with a body larger than [`LARGE_BODY`], which
/// may come from a slow uplink.
const UPLOAD_TIMEOUT: Duration = Duration::from_mins(15);
const LARGE_BODY: usize = 1024 * 1024;

/// Request headers the server gets to see. Cookies and the like stay here.
const REQUEST_HEADERS: &[HeaderName] = &[
    header::ACCEPT,
    header::CONTENT_TYPE,
    header::IF_MODIFIED_SINCE,
    header::IF_NONE_MATCH,
];

/// Response headers the browser gets to see.
const RESPONSE_HEADERS: &[HeaderName] = &[
    header::CACHE_CONTROL,
    header::CONTENT_DISPOSITION,
    header::CONTENT_TYPE,
    header::ETAG,
    header::LAST_MODIFIED,
];

/// The `WordMaker` server the interface edits.
#[derive(Debug, Clone)]
pub struct Upstream {
    client: reqwest::Client,
    base_url: String,
    /// The path of `base_url`, without a trailing slash; empty when the
    /// server is at the root of its host.
    base_path: String,
    token: String,
}

impl Upstream {
    pub fn new(base_url: &str, token: String) -> anyhow::Result<Self> {
        let base_url = base_url.trim_end_matches('/').to_owned();
        anyhow::ensure!(
            base_url.starts_with("http://") || base_url.starts_with("https://"),
            "the server URL must start with http:// or https://, got {base_url:?}"
        );
        let base_path = reqwest::Url::parse(&base_url)
            .with_context(|| format!("invalid server URL {base_url:?}"))?
            .path()
            .trim_end_matches('/')
            .to_owned();
        Ok(Self {
            client: reqwest::Client::builder()
                // The browser follows redirects itself, against this origin.
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(CONNECT_TIMEOUT)
                .build()?,
            base_url,
            base_path,
            token,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Sends `request` to the same path on the server, with the API token
    /// when `authorized`, and hands back what the server answers. The path
    /// must stay under `prefix`, e.g. `/api/v1/`.
    pub async fn forward(&self, request: Request, prefix: &str, authorized: bool) -> Response {
        let (parts, body) = request.into_parts();
        let Some(url) = self.url(&parts.uri, prefix) else {
            return error(StatusCode::BAD_REQUEST, "invalid path");
        };
        let Ok(body) = axum::body::to_bytes(body, MAX_BODY).await else {
            return error(StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
        };

        let timeout = if body.len() > LARGE_BODY {
            UPLOAD_TIMEOUT
        } else {
            REQUEST_TIMEOUT
        };
        let mut outgoing = self
            .client
            .request(parts.method, url)
            .timeout(timeout)
            .body(body);
        for name in REQUEST_HEADERS {
            if let Some(value) = parts.headers.get(name) {
                outgoing = outgoing.header(name, value);
            }
        }
        if authorized {
            outgoing = outgoing.bearer_auth(&self.token);
        }

        let upstream = match outgoing.send().await {
            Ok(upstream) => upstream,
            Err(err) if err.is_timeout() && !err.is_connect() => return too_slow(&err),
            Err(err) => {
                tracing::warn!(error = %err, "server request failed");
                return error(
                    StatusCode::BAD_GATEWAY,
                    &format!("cannot reach the WordMaker server at {}", self.base_url),
                );
            }
        };

        let status = upstream.status();
        let headers = upstream.headers().clone();
        let body = match upstream.bytes().await {
            Ok(body) => body,
            Err(err) if err.is_timeout() => return too_slow(&err),
            Err(err) => {
                tracing::warn!(error = %err, "server response failed");
                return error(
                    StatusCode::BAD_GATEWAY,
                    "the WordMaker server broke off its answer",
                );
            }
        };
        if status == StatusCode::UNAUTHORIZED && authorized {
            return error(
                StatusCode::BAD_GATEWAY,
                "the WordMaker server rejected the admin interface's token; check WORDMAKER_TOKEN",
            );
        }

        let mut response = Response::new(Body::from(body));
        *response.status_mut() = status;
        for name in RESPONSE_HEADERS {
            if let Some(value) = headers.get(name) {
                response.headers_mut().insert(name, value.clone());
            }
        }
        response
    }

    /// Where on the server `uri` goes, if that is under `prefix` there.
    ///
    /// The router matches paths as they come, but the URL parser resolves
    /// dot segments, so `/api/v1/../../x` would reach `<server>/x` with the
    /// token. Dot segments, backslashes and encoded dots and separators are
    /// refused outright; the parsed URL is checked as well.
    fn url(&self, uri: &Uri, prefix: &str) -> Option<reqwest::Url> {
        let path = uri.path();
        let lower = path.to_lowercase();
        if !path.starts_with(prefix)
            || path.contains('\\')
            || ["%2e", "%2f", "%5c"]
                .iter()
                .any(|encoded| lower.contains(encoded))
            || path
                .split('/')
                .any(|segment| segment == "." || segment == "..")
        {
            return None;
        }
        let path_and_query = uri.path_and_query().map_or(path, PathAndQuery::as_str);
        let url = reqwest::Url::parse(&format!("{}{path_and_query}", self.base_url)).ok()?;
        url.path()
            .starts_with(&format!("{}{prefix}", self.base_path))
            .then_some(url)
    }
}

fn too_slow(err: &reqwest::Error) -> Response {
    tracing::warn!(error = %err, "server request timed out");
    error(
        StatusCode::GATEWAY_TIMEOUT,
        "the WordMaker server took too long to answer",
    )
}

/// An error in the shape the server uses, so the page reads both alike.
pub fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(upstream: &Upstream, uri: &str, prefix: &str) -> Option<String> {
        let uri: Uri = uri.parse().unwrap();
        upstream.url(&uri, prefix).map(String::from)
    }

    #[test]
    fn paths_keep_the_servers_prefix() {
        // A server under a path of its own host, as behind a shared proxy.
        let upstream = Upstream::new("https://host.example/wordmaker/", "secret".into()).unwrap();
        assert_eq!(
            url(&upstream, "/api/v1/pages?x=1", "/api/v1/").as_deref(),
            Some("https://host.example/wordmaker/api/v1/pages?x=1")
        );
        assert_eq!(url(&upstream, "/media/x", "/api/v1/"), None);
        assert_eq!(url(&upstream, "/api/v1/../../other", "/api/v1/"), None);
        assert_eq!(url(&upstream, "/api/v1/%2E%2E/other", "/api/v1/"), None);

        let root = Upstream::new("http://127.0.0.1:3000", "secret".into()).unwrap();
        assert_eq!(
            url(&root, "/_wm/style.css", "/_wm/").as_deref(),
            Some("http://127.0.0.1:3000/_wm/style.css")
        );
    }
}
