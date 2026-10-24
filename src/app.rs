use std::sync::Arc;

use std::net::SocketAddr;

use axum::{
    Json,
    Router,
    extract::{
        ConnectInfo,
        Request,
        State,
    },
    http::{
        HeaderMap,
        HeaderValue,
        StatusCode,
        header,
    },
    middleware::{
        self,
        Next,
    },
    response::{
        IntoResponse,
        Response,
    },
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig,
    StreamableHttpService,
    session::local::LocalSessionManager,
};
use serde_json::json;
use sqlx::SqlitePool;
use subtle::ConstantTimeEq;
use tokio_util::sync::CancellationToken;
use tower_http::{
    compression::CompressionLayer,
    set_header::SetResponseHeaderLayer,
    trace::TraceLayer,
};

use crate::{
    analytics::Analytics,
    api,
    config::Config,
    error::AppError,
    images::Images,
    mcp::WordMakerMcp,
    oauth,
    site,
    theme::ThemeCache,
    token::Guesses,
};

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub config: Arc<Config>,
    pub token: Arc<str>,
    pub themes: Arc<ThemeCache>,
    pub images: Arc<Images>,
    /// Rendered page bodies and footers.
    pub renders: Arc<site::RenderCache>,
    pub oauth_clients: Arc<oauth::ClientCache>,
    /// Wrong admin tokens, to throttle guessing.
    pub guesses: Arc<Guesses>,
    /// Records page views; `None` when analytics is off.
    pub analytics: Option<Analytics>,
}

/// The whole application: public site, `/api/v1` and `/mcp`. Cancelling
/// `shutdown` ends the MCP sessions and their event streams, which would
/// otherwise keep a graceful shutdown waiting forever.
pub fn router(
    state: AppState,
    mcp_allowed_hosts: Vec<String>,
    shutdown: CancellationToken,
) -> Router {
    let mcp = {
        let pool = state.pool.clone();
        let config = state.config.clone();
        let mut mcp_config = StreamableHttpServerConfig::default();
        mcp_config.allowed_hosts = mcp_allowed_hosts;
        mcp_config.cancellation_token = shutdown;
        mcp_config.max_request_body_bytes = state.config.max_upload_bytes * 2;
        StreamableHttpService::new(
            move || Ok(WordMakerMcp::new(pool.clone(), config.clone())),
            LocalSessionManager::default().into(),
            mcp_config,
        )
    };

    let api = Router::new()
        .nest("/api/v1", api::router(&state))
        .layer(middleware::from_fn_with_state(state.clone(), require_token));
    let mcp = Router::new()
        .nest_service("/mcp", mcp)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_mcp_auth,
        ));

    // Every response gets these unless it sets its own, as the OAuth pages
    // and media files do: no content sniffing, no framing by other sites,
    // and only the origin in the `Referer` of links to other sites. The
    // admin interface shows previews on its own origin and never frames
    // the site.
    let default_header =
        |name, value| SetResponseHeaderLayer::if_not_present(name, HeaderValue::from_static(value));
    Router::new()
        .merge(api)
        .merge(mcp)
        .merge(oauth::router())
        .merge(site::router(&state).layer(CompressionLayer::new()))
        .layer(default_header(header::X_CONTENT_TYPE_OPTIONS, "nosniff"))
        .layer(default_header(
            header::REFERRER_POLICY,
            "strict-origin-when-cross-origin",
        ))
        .layer(default_header(header::X_FRAME_OPTIONS, "SAMEORIGIN"))
        .layer(default_header(
            header::CONTENT_SECURITY_POLICY,
            "frame-ancestors 'self'",
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Alternative to `Authorization: Bearer`, for MCP clients that cannot set
/// the `Authorization` header themselves.
pub const TOKEN_HEADER: &str = "x-wordmaker-token";

fn presented_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        // The scheme is case-insensitive (RFC 9110, section 11.1).
        .filter(|(scheme, _)| scheme.to_lowercase() == "bearer")
        .map(|(_, token)| token.trim_start())
        .or_else(|| {
            headers
                .get(TOKEN_HEADER)
                .and_then(|value| value.to_str().ok())
        })
}

fn is_admin_token(state: &AppState, presented: &str) -> bool {
    presented.as_bytes().ct_eq(state.token.as_bytes()).into()
}

/// The address the request came from, if the server knows it.
pub fn peer(request: &Request) -> Option<std::net::IpAddr> {
    request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(peer)| peer.ip())
}

/// The answer while a client has presented too many wrong tokens.
pub fn too_many_guesses() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::RETRY_AFTER, "60")],
        Json(json!({ "error": "too many wrong tokens; try again later" })),
    )
        .into_response()
}

async fn require_token(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    let Some(presented) = presented_token(request.headers()) else {
        return Err(AppError::Unauthorized);
    };
    let peer = peer(&request);
    if !state.guesses.allowed(peer) {
        return Ok(too_many_guesses());
    }
    if is_admin_token(&state, presented) {
        return Ok(next.run(request).await);
    }
    state.guesses.failed(peer);
    Err(AppError::Unauthorized)
}

/// `/mcp` takes the admin token or an OAuth access token. Without either,
/// the 401 tells clients where to find out how to sign in.
async fn require_mcp_auth(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    let presented = presented_token(request.headers()).map(str::to_owned);
    if let Some(presented) = &presented {
        // OAuth access tokens are random and long, so they are checked
        // even while wrong tokens are throttled; signed-in clients keep
        // working when someone guesses at the admin token.
        if oauth::access_token_valid(&state, presented).await? {
            return Ok(next.run(request).await);
        }
        let peer = peer(&request);
        if !state.guesses.allowed(peer) {
            return Ok(too_many_guesses());
        }
        if is_admin_token(&state, presented) {
            return Ok(next.run(request).await);
        }
        state.guesses.failed(peer);
    }
    let mut response = AppError::Unauthorized.into_response();
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        oauth::challenge(&state.config, presented.is_some()),
    );
    Ok(response)
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{
            Method,
            Request,
            StatusCode,
        },
    };
    use http_body_util::BodyExt;
    use serde_json::{
        Value,
        json,
    };
    use tower::ServiceExt;

    use super::*;

    async fn app() -> Router {
        app_with(None).await
    }

    async fn app_with(analytics: Option<Analytics>) -> Router {
        app_until(analytics, CancellationToken::new()).await
    }

    async fn app_until(analytics: Option<Analytics>, shutdown: CancellationToken) -> Router {
        let config = Config {
            database_url: String::new(),
            media_dir: std::env::temp_dir().join("wordmaker-app-test"),
            themes_dir: std::env::temp_dir().join(format!(
                "wordmaker-app-test-themes-{}",
                uuid::Uuid::new_v4().simple()
            )),
            base_url: "http://localhost:3000".into(),
            max_upload_bytes: 1024,
        };
        let state = AppState {
            pool: crate::db::connect_memory().await,
            config: Arc::new(config),
            token: "secret".into(),
            themes: Arc::default(),
            images: Arc::default(),
            renders: Arc::default(),
            oauth_clients: Arc::default(),
            guesses: Arc::default(),
            analytics,
        };
        router(state, vec!["localhost".into()], shutdown)
    }

    async fn send(
        app: &Router,
        method: Method,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, String) {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", "Bearer secret");
        let body = match body {
            Some(json) => {
                request = request.header("content-type", "application/json");
                Body::from(json.to_string())
            }
            None => Body::empty(),
        };
        let response = app
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    async fn get_page(app: &Router, uri: &str) -> StatusCode {
        // As from a reverse proxy on the same host.
        let proxy = std::net::SocketAddr::from(([127, 0, 0, 1], 4000));
        let request = Request::get(uri)
            .extension(axum::extract::ConnectInfo(proxy))
            .header("x-forwarded-for", "198.51.100.9, 203.0.113.7")
            .header("user-agent", "Mozilla/5.0 (compatible; Googlebot/2.1)")
            .header("referer", "https://search.example/")
            .body(Body::empty())
            .unwrap();
        app.clone().oneshot(request).await.unwrap().status()
    }

    #[tokio::test]
    async fn records_page_views_only() {
        let (analytics, mut hits) = Analytics::channel(1);
        let app = app_with(Some(analytics)).await;
        send(
            &app,
            Method::POST,
            "/api/v1/pages",
            Some(json!({ "title": "Hello", "body": "Hi." })),
        )
        .await;
        send(&app, Method::POST, "/api/v1/pages/hello/publish", None).await;

        assert_eq!(get_page(&app, "/").await, StatusCode::OK);
        assert_eq!(get_page(&app, "/hello?utm_source=x").await, StatusCode::OK);
        assert_eq!(get_page(&app, "/missing").await, StatusCode::NOT_FOUND);
        assert_eq!(get_page(&app, "/_wm/style.css").await, StatusCode::OK);
        assert_eq!(
            get_page(&app, "/api/v1/pages").await,
            StatusCode::UNAUTHORIZED
        );
        send(&app, Method::GET, "/api/v1/pages", None).await;

        let mut recorded = Vec::new();
        while let Ok(hit) = hits.try_recv() {
            recorded.push(hit);
        }
        let seen: Vec<_> = recorded
            .iter()
            .map(|hit| (hit.slug.as_str(), hit.status))
            .collect();
        assert_eq!(seen, [("", 200), ("hello", 200), ("missing", 404)]);
        let hello = &recorded[1];
        assert_eq!(hello.query.as_deref(), Some("utm_source=x"));
        assert_eq!(hello.ip, Some("203.0.113.7".parse().unwrap()));
        assert_eq!(hello.referrer, "https://search.example/");
        assert!(hello.user_agent.contains("Googlebot"));
    }

    #[tokio::test]
    async fn pages_load_while_clickhouse_is_down() {
        #[derive(clap::Parser)]
        struct Args {
            #[command(flatten)]
            analytics: crate::analytics::AnalyticsArgs,
        }
        let args = <Args as clap::Parser>::parse_from([
            "wordmaker",
            "--clickhouse-url",
            "http://127.0.0.1:1",
        ]);
        let (analytics, sink) = args
            .analytics
            .start("http://localhost:3000", CancellationToken::new())
            .unwrap()
            .unwrap();
        let app = app_with(Some(analytics)).await;

        let pages = async {
            for _ in 0..50 {
                assert_eq!(get_page(&app, "/").await, StatusCode::OK);
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), pages)
            .await
            .expect("page views do not wait for ClickHouse");
        sink.abort();
    }

    #[tokio::test]
    async fn analytics_report_needs_analytics() {
        let (status, body) = send(&app().await, Method::GET, "/api/v1/analytics", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("WORDMAKER_CLICKHOUSE_URL"), "{body}");

        let (analytics, _hits) = Analytics::channel(1);
        let app = app_with(Some(analytics)).await;
        let (status, _) = send(&app, Method::GET, "/api/v1/analytics?days=0", None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = send(&app, Method::GET, "/api/v1/analytics?days=400", None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let response = app
            .oneshot(
                Request::get("/api/v1/analytics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    fn mcp_request(method: Method, session: Option<&str>, body: Option<Value>) -> Request<Body> {
        let mut request = Request::builder()
            .method(method)
            .uri("/mcp")
            .header("host", "localhost")
            .header("authorization", "Bearer secret")
            .header("accept", "application/json, text/event-stream");
        if let Some(session) = session {
            request = request.header("mcp-session-id", session);
        }
        match body {
            Some(json) => request
                .header("content-type", "application/json")
                .body(Body::from(json.to_string())),
            None => request.body(Body::empty()),
        }
        .unwrap()
    }

    #[tokio::test]
    async fn shutdown_ends_mcp_event_streams() {
        let shutdown = CancellationToken::new();
        let app = app_until(None, shutdown.clone()).await;
        let initialize = json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "1" }
            }
        });
        let response = app
            .clone()
            .oneshot(mcp_request(Method::POST, None, Some(initialize)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let session = response.headers()["mcp-session-id"]
            .to_str()
            .unwrap()
            .to_owned();
        response.into_body().collect().await.unwrap();
        let initialized = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        app.clone()
            .oneshot(mcp_request(Method::POST, Some(&session), Some(initialized)))
            .await
            .unwrap();

        // The stream a client keeps open for server messages.
        let response = app
            .oneshot(mcp_request(Method::GET, Some(&session), None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body();
        // Past the priming event it stays open, waiting for server messages.
        loop {
            match tokio::time::timeout(std::time::Duration::from_millis(300), body.frame()).await {
                Err(_) => break,
                Ok(Some(Ok(_))) => {}
                ended => panic!("the stream ended: {ended:?}"),
            }
        }

        shutdown.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(5), body.collect())
            .await
            .expect("the stream ends on shutdown")
            .ok();
    }

    #[tokio::test]
    async fn api_requires_token() {
        let app = app().await;
        let response = app
            .clone()
            .oneshot(Request::get("/api/v1/pages").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = app
            .oneshot(
                Request::get("/api/v1/pages")
                    .header("authorization", "Bearer wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wrong_tokens_are_throttled() {
        let app = app().await;
        let status = |path: &'static str, token: &'static str| {
            let app = app.clone();
            async move {
                app.oneshot(
                    Request::post(path)
                        .header("authorization", format!("Bearer {token}"))
                        .header("host", "localhost")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
                .status()
            }
        };
        // /api and /mcp share one allowance.
        for i in 0..20 {
            let path = if i % 2 == 0 { "/api/v1/pages" } else { "/mcp" };
            assert_eq!(status(path, "wrong").await, StatusCode::UNAUTHORIZED);
        }
        // Now even the right token is not checked.
        assert_eq!(
            status("/api/v1/pages", "secret").await,
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            status("/mcp", "secret").await,
            StatusCode::TOO_MANY_REQUESTS
        );
    }

    #[tokio::test]
    async fn bearer_scheme_is_case_insensitive() {
        let app = app().await;
        for value in ["bearer secret", "BEARER secret", "Bearer  secret"] {
            let response = app
                .clone()
                .oneshot(
                    Request::get("/api/v1/pages")
                        .header("authorization", value)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{value}");
        }
    }

    #[tokio::test]
    async fn token_header_is_accepted() {
        let app = app().await;
        for (token, expected) in [
            ("secret", StatusCode::OK),
            ("wrong", StatusCode::UNAUTHORIZED),
            ("Bearer secret", StatusCode::UNAUTHORIZED),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::get("/api/v1/pages")
                        .header(TOKEN_HEADER, token)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "{token}");
        }
    }

    #[tokio::test]
    async fn page_lifecycle_through_api_and_site() {
        let app = app().await;
        let (status, body) = send(
            &app,
            Method::POST,
            "/api/v1/pages",
            Some(json!({ "title": "Hello World", "body": "Some *bold* text." })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let page: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(page["slug"], "hello-world");
        assert_eq!(page["url"], "http://localhost:3000/hello-world");

        // Drafts are not public.
        let (status, _) = send(&app, Method::GET, "/hello-world", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, _) = send(
            &app,
            Method::POST,
            "/api/v1/pages/hello-world/publish",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, html) = send(&app, Method::GET, "/hello-world", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("<strong>bold</strong>"), "{html}");
        assert!(
            html.contains("<h1 class=\"wp-block-post-title\">Hello World</h1>"),
            "{html}"
        );
        assert!(html.contains("<style>"), "stylesheet inlined: {html}");

        let (_, index) = send(&app, Method::GET, "/", None).await;
        assert!(index.contains("href=\"/hello-world\""), "{index}");

        let (status, body) = send(
            &app,
            Method::PATCH,
            "/api/v1/pages/hello-world",
            Some(json!({ "body": "Changed." })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let (_, body) = send(
            &app,
            Method::GET,
            "/api/v1/pages/hello-world/revisions",
            None,
        )
        .await;
        let revisions: Vec<Value> = serde_json::from_str(&body).unwrap();
        assert_eq!(revisions.len(), 2);

        let (status, body) = send(
            &app,
            Method::POST,
            "/api/v1/pages",
            Some(json!({ "title": "Hello World" })),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");

        let (status, _) = send(&app, Method::DELETE, "/api/v1/pages/hello-world", None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn home_page_setting() {
        let app = app().await;
        send(
            &app,
            Method::POST,
            "/api/v1/pages",
            Some(json!({ "title": "Welcome", "body": "Welcome text.", "publish": true })),
        )
        .await;
        let (status, body) = send(
            &app,
            Method::PUT,
            "/api/v1/settings",
            Some(json!({ "site_title": "My Site", "home_slug": "welcome" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let (_, html) = send(&app, Method::GET, "/", None).await;
        assert!(html.contains("Welcome text."), "{html}");
        assert!(html.contains("<title>My Site</title>"), "{html}");
        assert!(html.contains("<html lang=\"en\">"), "{html}");
        assert!(html.contains("aria-label=\"Open menu\""), "{html}");
        assert!(html.contains("popovertarget=\"wm-menu\""), "{html}");
        // No tagline or footer text: the footer stays empty.
        assert!(
            !html[html.find("<footer").unwrap()..].contains("site-title"),
            "{html}"
        );

        let (status, body) = send(
            &app,
            Method::PUT,
            "/api/v1/settings",
            Some(json!({ "site_language": "de" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (_, html) = send(&app, Method::GET, "/", None).await;
        assert!(html.contains("<html lang=\"de\">"), "{html}");
        assert!(html.contains("aria-label=\"Menü öffnen\""), "{html}");

        let (status, _) = send(
            &app,
            Method::PUT,
            "/api/v1/settings",
            Some(json!({ "site_language": "de\"><script>" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn unpublished_home_page_falls_back_to_the_list() {
        let app = app().await;
        for (title, publish) in [("Welcome", true), ("About", true)] {
            send(
                &app,
                Method::POST,
                "/api/v1/pages",
                Some(json!({ "title": title, "body": "Welcome text.", "publish": publish })),
            )
            .await;
        }
        send(
            &app,
            Method::PUT,
            "/api/v1/settings",
            Some(json!({ "home_slug": "welcome" })),
        )
        .await;
        send(&app, Method::POST, "/api/v1/pages/welcome/unpublish", None).await;

        let (status, html) = send(&app, Method::GET, "/", None).await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(html.contains("wm-page-list"), "{html}");
        assert!(html.contains("href=\"/about\""), "{html}");
        let (_, xml) = send(&app, Method::GET, "/sitemap.xml", None).await;
        assert!(
            xml.contains("<loc>http://localhost:3000/about</loc>"),
            "{xml}"
        );
    }

    async fn get_response(app: &Router, uri: &str) -> Response {
        app.clone()
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn responses_carry_security_headers() {
        let app = app().await;
        for uri in ["/", "/missing", "/_wm/style.css", "/api/v1/pages"] {
            let response = get_response(&app, uri).await;
            let headers = response.headers();
            assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff", "{uri}");
            assert_eq!(
                headers[header::REFERRER_POLICY],
                "strict-origin-when-cross-origin",
                "{uri}"
            );
            assert_eq!(headers[header::X_FRAME_OPTIONS], "SAMEORIGIN", "{uri}");
            assert_eq!(
                headers[header::CONTENT_SECURITY_POLICY],
                "frame-ancestors 'self'",
                "{uri}"
            );
        }
    }

    #[tokio::test]
    async fn uploads_cannot_run_scripts_on_the_site() {
        let app = app().await;
        let boundary = "wm-boundary";
        let parts: String = ["x.html", "x.svg", "x.png", "x.pdf", "x.js"]
            .map(|name| {
                format!(
                    "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; \
                     filename=\"{name}\"\r\n\r\n<script>alert(1)</script>\r\n"
                )
            })
            .concat();
        let body = format!("{parts}--{boundary}--\r\n");
        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/media")
                    .header("authorization", "Bearer secret")
                    .header(
                        "content-type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let stored: Vec<Value> = serde_json::from_slice(&bytes).unwrap();

        let sandboxed = |headers: &HeaderMap| {
            headers[header::CONTENT_SECURITY_POLICY]
                .to_str()
                .unwrap()
                .starts_with("sandbox; default-src 'none'")
        };
        for media in &stored {
            let path = media["path"].as_str().unwrap();
            let response = get_response(&app, path).await;
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            let headers = response.headers();
            assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
            let download = headers.get(header::CONTENT_DISPOSITION).is_some();
            let extension = path.rsplit('.').next().unwrap();
            match extension {
                "html" | "js" => assert!(download && sandboxed(headers), "{path}"),
                "svg" | "png" => assert!(!download && sandboxed(headers), "{path}"),
                // A sandbox would keep the browser's viewer from showing it.
                "pdf" => assert!(!download && !sandboxed(headers), "{path}"),
                other => panic!("unexpected {other}"),
            }
            // Revalidating does not lift the policy.
            let response = app
                .clone()
                .oneshot(
                    Request::get(path)
                        .header(header::IF_MODIFIED_SINCE, "Fri, 01 Jan 2100 00:00:00 GMT")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_MODIFIED, "{path}");
            assert_eq!(sandboxed(response.headers()), extension != "pdf", "{path}");
        }
    }

    #[tokio::test]
    async fn rendered_pages_follow_edits_and_settings() {
        let app = app().await;
        send(
            &app,
            Method::POST,
            "/api/v1/pages",
            Some(json!({ "title": "News", "body": "Old *news*.", "publish": true })),
        )
        .await;
        let (_, html) = send(&app, Method::GET, "/news", None).await;
        assert!(html.contains("Old <strong>news</strong>."), "{html}");

        send(
            &app,
            Method::PATCH,
            "/api/v1/pages/news",
            Some(json!({ "title": "Latest", "body": "New *news*." })),
        )
        .await;
        send(
            &app,
            Method::PUT,
            "/api/v1/settings",
            Some(json!({ "footer_text": "Footer *one*." })),
        )
        .await;
        let (_, html) = send(&app, Method::GET, "/news", None).await;
        assert!(html.contains("New <strong>news</strong>."), "{html}");
        assert!(html.contains("Latest</h1>"), "{html}");
        assert!(html.contains("Footer <strong>one</strong>."), "{html}");

        send(
            &app,
            Method::PUT,
            "/api/v1/settings",
            Some(json!({ "footer_text": "Footer *two*." })),
        )
        .await;
        let (_, html) = send(&app, Method::GET, "/news", None).await;
        assert!(html.contains("Footer <strong>two</strong>."), "{html}");
        // Unpublished pages are not served from the cache.
        send(&app, Method::POST, "/api/v1/pages/news/unpublish", None).await;
        let (status, _) = send(&app, Method::GET, "/news", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn sitemap_lists_published_pages() {
        let app = app().await;
        for (title, publish) in [("Welcome", true), ("About", true), ("Secret", false)] {
            send(
                &app,
                Method::POST,
                "/api/v1/pages",
                Some(json!({ "title": title, "body": "Text.", "publish": publish })),
            )
            .await;
        }
        send(
            &app,
            Method::PUT,
            "/api/v1/settings",
            Some(json!({ "home_slug": "welcome" })),
        )
        .await;

        let (status, xml) = send(&app, Method::GET, "/sitemap.xml", None).await;
        assert_eq!(status, StatusCode::OK, "{xml}");
        assert!(xml.contains("<loc>http://localhost:3000/</loc>"), "{xml}");
        assert!(
            xml.contains("<loc>http://localhost:3000/about</loc>"),
            "{xml}"
        );
        assert!(!xml.contains("/welcome<"), "home is listed as /: {xml}");
        assert!(!xml.contains("/secret<"), "drafts stay out: {xml}");

        let (status, robots) = send(&app, Method::GET, "/robots.txt", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            robots.contains("Sitemap: http://localhost:3000/sitemap.xml"),
            "{robots}"
        );

        // The home page names `/` as canonical under either URL.
        let home = "<link rel=\"canonical\" href=\"http://localhost:3000/\">";
        for uri in ["/", "/welcome"] {
            let (_, html) = send(&app, Method::GET, uri, None).await;
            assert!(html.contains(home), "{uri}: {html}");
        }
        let (_, html) = send(&app, Method::GET, "/about", None).await;
        assert!(
            html.contains("<link rel=\"canonical\" href=\"http://localhost:3000/about\">"),
            "{html}"
        );
    }

    #[tokio::test]
    async fn drafts_stay_off_the_site_until_published() {
        let app = app().await;
        send(
            &app,
            Method::POST,
            "/api/v1/pages",
            Some(json!({ "title": "News", "body": "Old news.", "publish": true })),
        )
        .await;
        let (status, body) = send(
            &app,
            Method::PUT,
            "/api/v1/pages/news/draft",
            Some(json!({ "body": "Fresh news." })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let page: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(page["draft_body"], "Fresh news.");

        let (_, html) = send(&app, Method::GET, "/news", None).await;
        assert!(html.contains("Old news."), "{html}");
        let (_, list) = send(&app, Method::GET, "/api/v1/pages", None).await;
        assert!(list.contains("\"has_draft\":true"), "{list}");

        send(&app, Method::POST, "/api/v1/pages/news/publish", None).await;
        let (_, html) = send(&app, Method::GET, "/news", None).await;
        assert!(html.contains("Fresh news."), "{html}");

        send(
            &app,
            Method::PUT,
            "/api/v1/pages/news/draft",
            Some(json!({ "body": "Scrapped." })),
        )
        .await;
        let (status, body) = send(&app, Method::DELETE, "/api/v1/pages/news/draft", None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let page: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(page["draft_body"], Value::Null);
    }

    #[tokio::test]
    async fn preview_renders_unsaved_pages_in_the_site_frame() {
        let app = app().await;
        send(
            &app,
            Method::POST,
            "/api/v1/pages",
            Some(json!({ "title": "About", "publish": true })),
        )
        .await;
        let (status, html) = send(
            &app,
            Method::POST,
            "/api/v1/preview",
            Some(json!({ "title": "About <us>", "slug": "about", "body": "Not *saved* yet." })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(html.contains("<strong>saved</strong>"), "{html}");
        assert!(html.contains("About &lt;us&gt;</h1>"), "{html}");
        assert!(html.contains("<style>"), "stylesheet inlined: {html}");
        assert!(html.contains("current-menu-item"), "{html}");

        // Nothing was stored.
        let (_, body) = send(&app, Method::GET, "/api/v1/pages/about", None).await;
        let page: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(page["body"], "");
    }

    #[tokio::test]
    async fn navigation_order_and_visibility() {
        let app = app().await;
        for (title, order, in_nav) in [("Zeta", 1, true), ("Alpha", 2, true), ("Hidden", 0, false)]
        {
            send(
                &app,
                Method::POST,
                "/api/v1/pages",
                Some(json!({
                    "title": title, "publish": true, "nav_order": order, "in_nav": in_nav
                })),
            )
            .await;
        }
        let (_, html) = send(&app, Method::GET, "/zeta", None).await;
        let nav = &html[html.find("<nav").unwrap()..html.find("</nav>").unwrap()];
        let zeta = nav.find("Zeta").unwrap();
        let alpha = nav.find("Alpha").unwrap();
        assert!(zeta < alpha, "{nav}");
        assert!(!nav.contains("Hidden"), "{nav}");
        assert!(nav.contains("current-menu-item"), "{nav}");
    }

    fn theme_zip() -> Vec<u8> {
        use std::io::Write;

        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        let files: &[(&str, &str)] = &[
            (
                "mytheme/style.css",
                "/*\nTheme Name: My Theme\nVersion: 1.2\n*/\na { text-underline-offset: .1em; }\n",
            ),
            (
                "mytheme/theme.json",
                r##"{"version":3,"settings":{"color":{"defaultPalette":false,"palette":[
                    {"slug":"base","color":"#ffffff"},{"slug":"contrast","color":"#111111"}]},
                  "typography":{"fontFamilies":[{"slug":"body","fontFamily":"Body, sans-serif",
                    "fontFace":[{"fontFamily":"Body","src":["file:./assets/fonts/body.woff2"]}]}]}},
                  "styles":{"color":{"background":"var:preset|color|base","text":"var:preset|color|contrast"}}}"##,
            ),
            (
                "mytheme/styles/01-night.json",
                r##"{"title":"Night","settings":{"color":{"palette":[
                    {"slug":"base","color":"#000000"},{"slug":"contrast","color":"#eeeeee"}]}}}"##,
            ),
            (
                "mytheme/styles/sections/section-1.json",
                r#"{"slug":"section-1","blockTypes":["core/group"],
                     "styles":{"color":{"background":"var:preset|color|contrast"}}}"#,
            ),
            ("mytheme/assets/fonts/body.woff2", "font-bytes"),
            ("mytheme/functions.php", "<?php echo 'never extracted';"),
        ];
        for (name, content) in files {
            zip.start_file(*name, options).unwrap();
            zip.write_all(content.as_bytes()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    #[tokio::test]
    async fn theme_fonts_are_cached_for_good() {
        let app = app().await;
        let (status, body) = send_bytes(&app, "/api/v1/themes/upload", theme_zip()).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        send(
            &app,
            Method::PUT,
            "/api/v1/themes/active",
            Some(json!({ "theme": "mytheme" })),
        )
        .await;
        let (_, css) = send(&app, Method::GET, "/_wm/style.css", None).await;

        // Linked with the install's stamp, which changes on reinstall.
        let font_url = css
            .split("url('")
            .nth(1)
            .and_then(|rest| rest.split('\'').next())
            .unwrap();
        assert!(
            font_url.starts_with("/_wm/theme/mytheme@") && font_url.ends_with("/body.woff2"),
            "{font_url}"
        );
        let response = app
            .clone()
            .oneshot(Request::get(font_url).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers()[header::CACHE_CONTROL]
                .to_str()
                .unwrap()
                .contains("immutable")
        );
        let font = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(font, "font-bytes");

        // Any other stamp, such as a guess at the next one, is not cached.
        let response = app
            .clone()
            .oneshot(
                Request::get("/_wm/theme/mytheme@1/assets/fonts/body.woff2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-cache");
    }

    async fn send_bytes(app: &Router, uri: &str, body: Vec<u8>) -> (StatusCode, String) {
        let response = app
            .clone()
            .oneshot(
                Request::post(uri)
                    .header("authorization", "Bearer secret")
                    .header("content-type", "application/zip")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[tokio::test]
    async fn install_and_activate_theme() {
        let app = app().await;
        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/themes/upload")
                    .header("authorization", "Bearer secret")
                    .header("content-type", "application/zip")
                    .body(Body::from(theme_zip()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(status, StatusCode::CREATED, "{body:?}");
        let info: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(info["slug"], "mytheme");
        assert_eq!(info["name"], "My Theme");
        assert_eq!(info["version"], "1.2");
        // Section styles are block styles, not global variations.
        assert_eq!(
            info["variations"],
            json!([{ "id": "01-night", "title": "Night", "kind": "style" }])
        );

        let (status, body) = send(
            &app,
            Method::PUT,
            "/api/v1/themes/active",
            Some(json!({ "theme": "mytheme", "variations": ["nope"] })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

        let (status, body) = send(
            &app,
            Method::PUT,
            "/api/v1/themes/active",
            Some(json!({ "theme": "mytheme", "variations": ["01-night"] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let (status, css) = send(&app, Method::GET, "/_wm/style.css", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            css.contains("--wp--preset--color--base:#000000"),
            "variation applied"
        );
        assert!(css.contains(":root :where(.is-style-section-1)"));
        assert!(
            css.contains("text-underline-offset:.1em"),
            "style.css included"
        );
        assert!(
            css.contains("color-scheme:dark"),
            "dark background detected"
        );

        let (status, font) = send(
            &app,
            Method::GET,
            "/_wm/theme/mytheme/assets/fonts/body.woff2",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(font, "font-bytes");
        for path in ["functions.php", "theme.json", "../mytheme/style.css"] {
            let (status, _) = send(
                &app,
                Method::GET,
                &format!("/_wm/theme/mytheme/{path}"),
                None,
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        }

        let (status, _) = send(&app, Method::DELETE, "/api/v1/themes/mytheme", None).await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "active theme cannot be deleted"
        );
        send(
            &app,
            Method::PUT,
            "/api/v1/themes/active",
            Some(json!({ "theme": null })),
        )
        .await;
        let (status, _) = send(&app, Method::DELETE, "/api/v1/themes/mytheme", None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn edit_theme_files() {
        let app = app().await;
        send_bytes(&app, "/api/v1/themes/upload", theme_zip()).await;
        send(
            &app,
            Method::PUT,
            "/api/v1/themes/active",
            Some(json!({ "theme": "mytheme" })),
        )
        .await;

        let (status, body) = send(&app, Method::GET, "/api/v1/themes/mytheme", None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let details: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(details["name"], "My Theme");
        let paths: Vec<&str> = details["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["path"].as_str().unwrap())
            .collect();
        assert_eq!(
            paths,
            [
                "assets/fonts/body.woff2",
                "style.css",
                "styles/01-night.json",
                "styles/sections/section-1.json",
                "theme.json"
            ]
        );

        let (status, body) = send(
            &app,
            Method::GET,
            "/api/v1/themes/mytheme/files/style.css",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let file: Value = serde_json::from_str(&body).unwrap();
        let style = file["content"].as_str().unwrap();
        assert!(style.contains("Theme Name: My Theme"), "{style}");

        // The live stylesheet follows a change to style.css alone.
        let (_, css) = send(&app, Method::GET, "/_wm/style.css", None).await;
        assert!(!css.contains("letter-spacing:.2em"));
        let (status, body) = send(
            &app,
            Method::PUT,
            "/api/v1/themes/mytheme/files/style.css",
            Some(json!({ "content": format!("{style}h1 {{ letter-spacing: .2em; }}\n") })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (_, css) = send(&app, Method::GET, "/_wm/style.css", None).await;
        assert!(css.contains("letter-spacing:.2em"), "{css}");

        for (path, content) in [
            ("theme.json", "[]"),
            ("styles/broken.json", "{"),
            ("functions.php", "<?php"),
            (".hidden", "x"),
            ("styles", "x"),
        ] {
            let (status, body) = send(
                &app,
                Method::PUT,
                &format!("/api/v1/themes/mytheme/files/{path}"),
                Some(json!({ "content": content })),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
        }
        let (status, _) = send(
            &app,
            Method::GET,
            "/api/v1/themes/mytheme/files/missing.css",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = send(&app, Method::GET, "/api/v1/themes/nope", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn theme_css_cannot_end_the_style_element() {
        let app = app().await;
        send_bytes(&app, "/api/v1/themes/upload", theme_zip()).await;
        send(
            &app,
            Method::PUT,
            "/api/v1/themes/active",
            Some(json!({ "theme": "mytheme" })),
        )
        .await;
        send(
            &app,
            Method::PUT,
            "/api/v1/themes/mytheme/files/style.css",
            Some(
                json!({ "content": "a::after { content: '</STYLE><script>alert(1)</script>'; }" }),
            ),
        )
        .await;
        send(
            &app,
            Method::POST,
            "/api/v1/pages",
            Some(json!({ "title": "Hello", "publish": true })),
        )
        .await;
        let (status, html) = send(&app, Method::GET, "/hello", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(html.to_lowercase().matches("</style").count(), 1, "{html}");
        assert!(
            html.contains("<\\/STYLE><script>alert(1)<\\/script>"),
            "{html}"
        );
    }
}
