use axum::{
    Json,
    body::Body,
    extract::Request,
    http::{
        HeaderName,
        StatusCode,
        header,
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
    token: String,
}

impl Upstream {
    pub fn new(base_url: &str, token: String) -> anyhow::Result<Self> {
        let base_url = base_url.trim_end_matches('/').to_owned();
        anyhow::ensure!(
            base_url.starts_with("http://") || base_url.starts_with("https://"),
            "the server URL must start with http:// or https://, got {base_url:?}"
        );
        Ok(Self {
            client: reqwest::Client::builder()
                // The browser follows redirects itself, against this origin.
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            base_url,
            token,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Sends `request` to the same path on the server, with the API token
    /// when `authorized`, and hands back what the server answers.
    pub async fn forward(&self, request: Request, authorized: bool) -> Response {
        let (parts, body) = request.into_parts();
        let path = parts.uri.path_and_query().map_or("/", |path| path.as_str());
        let Ok(body) = axum::body::to_bytes(body, MAX_BODY).await else {
            return error(StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
        };

        let mut outgoing = self
            .client
            .request(parts.method, format!("{}{path}", self.base_url))
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
}

/// An error in the shape the server uses, so the page reads both alike.
pub fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}
