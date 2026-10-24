use std::sync::Arc;

use axum::{
    Extension,
    Json,
    Router,
    extract::{
        Request,
        State,
    },
    http::{
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
    routing::{
        any,
        get,
    },
};
use serde_json::json;
use tower_http::trace::TraceLayer;

use crate::{
    auth::{
        self,
        Auth,
        User,
    },
    proxy::{
        self,
        Upstream,
    },
};

/// Header the page sends with every API call. A form or image on another
/// site cannot add it, and a script there cannot either without a CORS
/// preflight this server never grants, so it stops cross-site requests from
/// using the token this server holds.
pub const API_GUARD_HEADER: &str = "x-wordmaker-admin";

/// What the interface's own pages may load. Script only from here (and
/// the editor's wasm); styles, images, fonts and media more widely, since
/// the preview shows pages as the site would, and its frame inherits this.
const ADMIN_CSP: &str = "default-src 'self'; \
    script-src 'self' 'wasm-unsafe-eval'; \
    style-src 'self' 'unsafe-inline' https:; \
    img-src * data: blob:; \
    font-src * data:; \
    media-src * data: blob:; \
    frame-src 'self' https:; \
    connect-src 'self'; \
    object-src 'none'; \
    base-uri 'self'; \
    form-action 'self'; \
    frame-ancestors 'self'";

/// Files passed on from the site's `/_wm` and `/media` may be anything a
/// theme or an upload holds, SVG and HTML included. Opened on their own on
/// this origin they could script the API with the token, so they are shown
/// sandboxed, without script. Loading them into the interface's pages as
/// images, styles or fonts is not affected.
const PUBLIC_CSP: &str = "default-src 'none'; \
    img-src 'self' data:; \
    style-src 'self' 'unsafe-inline'; \
    font-src 'self'; \
    sandbox";

#[derive(Debug, Clone)]
pub struct Admin {
    pub upstream: Upstream,
    /// `Host` names this interface answers to; anything else may be a DNS
    /// rebinding attempt.
    pub allowed_hosts: Arc<[String]>,
    /// Sign-in through an OIDC provider; without it, whoever
    /// reaches the interface may use it.
    pub auth: Option<Auth>,
}

/// One file of the page itself, built into the binary.
struct Asset {
    path: &'static str,
    content_type: &'static str,
    body: &'static [u8],
}

const ASSETS: &[Asset] = &[
    Asset {
        path: "/",
        content_type: "text/html; charset=utf-8",
        body: include_bytes!("../assets/index.html"),
    },
    Asset {
        path: "/app.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_bytes!("../assets/app.js"),
    },
    Asset {
        path: "/app.css",
        content_type: "text/css; charset=utf-8",
        body: include_bytes!("../assets/app.css"),
    },
    Asset {
        path: "/vendor/asciidoc_editor.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_bytes!("../vendor/asciidoc-editor/asciidoc_editor.js"),
    },
    Asset {
        path: "/vendor/asciidoc_editor_bg.wasm",
        content_type: "application/wasm",
        body: include_bytes!("../vendor/asciidoc-editor/asciidoc_editor_bg.wasm"),
    },
    Asset {
        path: "/vendor/world.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_bytes!("../vendor/world/world.js"),
    },
];

pub fn router(admin: Admin) -> Router {
    let mut router = Router::new();
    for asset in ASSETS {
        router = router.route(asset.path, get(move || async move { serve_asset(asset) }));
    }
    router
        .route("/config.json", get(config))
        .route("/api/v1/{*path}", any(api))
        // Public parts of the site, so previews and the editor show the
        // theme's styles and fonts and the uploaded images.
        .route("/_wm/{*path}", get(public))
        .route("/media/{*path}", get(public))
        .route_layer(middleware::from_fn_with_state(
            admin.clone(),
            auth::require_session,
        ))
        .merge(auth::router())
        .layer(middleware::map_response(security_headers))
        .layer(middleware::from_fn_with_state(admin.clone(), check_host))
        .layer(TraceLayer::new_for_http())
        .with_state(admin)
}

fn serve_asset(asset: &'static Asset) -> Response {
    (
        [
            (header::CONTENT_TYPE, asset.content_type),
            // Small and built into the binary: revalidating costs little and
            // an upgrade is seen at once.
            (header::CACHE_CONTROL, "no-cache"),
        ],
        asset.body,
    )
        .into_response()
}

/// What the page needs to know about the server it edits.
async fn config(
    State(admin): State<Admin>,
    user: Option<Extension<User>>,
) -> Json<serde_json::Value> {
    Json(json!({
        "site_url": admin.upstream.base_url(),
        "version": env!("CARGO_PKG_VERSION"),
        "user": user.map(|Extension(User(email))| email),
    }))
}

async fn api(State(admin): State<Admin>, request: Request) -> Response {
    if !request.headers().contains_key(API_GUARD_HEADER) {
        return proxy::error(
            StatusCode::FORBIDDEN,
            &format!("API requests need the {API_GUARD_HEADER} header"),
        );
    }
    admin.upstream.forward(request, "/api/v1/", true).await
}

async fn public(State(admin): State<Admin>, request: Request) -> Response {
    let prefix = if request.uri().path().starts_with("/_wm/") {
        "/_wm/"
    } else {
        "/media/"
    };
    let mut response = admin.upstream.forward(request, prefix, false).await;
    // A sandbox keeps browsers from showing PDFs at all, and their viewers
    // run no script of this origin.
    let pdf = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim_start().starts_with("application/pdf"));
    if !pdf {
        response.headers_mut().insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(PUBLIC_CSP),
        );
    }
    response
}

async fn check_host(State(admin): State<Admin>, request: Request, next: Next) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(hostname);
    match host {
        Some(host) if admin.allowed_hosts.iter().any(|allowed| allowed == host) => {
            next.run(request).await
        }
        _ => proxy::error(
            StatusCode::FORBIDDEN,
            "host not allowed; see WORDMAKER_ADMIN_ALLOWED_HOSTS",
        ),
    }
}

async fn security_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    if !headers.contains_key(header::CONTENT_SECURITY_POLICY) {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(ADMIN_CSP),
        );
    }
    headers.insert(
        header::X_FRAME_OPTIONS,
        HeaderValue::from_static("SAMEORIGIN"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    response
}

/// `example.com:3001` yields `example.com`, `[::1]:3001` yields `::1`.
fn hostname(authority: &str) -> &str {
    if let Some(bracketed) = authority.strip_prefix('[') {
        return bracketed.split(']').next().unwrap_or_default();
    }
    authority
        .rsplit_once(':')
        .map_or(authority, |(host, _)| host)
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{
            HeaderMap,
            Method,
        },
        routing::post,
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    /// A stand-in `WordMaker` server that reports what it was sent.
    async fn fake_server() -> String {
        async fn echo(
            method: Method,
            headers: HeaderMap,
            request: Request,
        ) -> Json<serde_json::Value> {
            let uri = request.uri().to_string();
            let body = axum::body::to_bytes(request.into_body(), usize::MAX)
                .await
                .unwrap();
            Json(json!({
                "method": method.as_str(),
                "uri": uri,
                "authorization": headers.get(header::AUTHORIZATION).map(|v| v.to_str().unwrap()),
                "cookie": headers.get(header::COOKIE).map(|v| v.to_str().unwrap()),
                "body": String::from_utf8(body.to_vec()).unwrap(),
            }))
        }

        let server = Router::new()
            .route("/api/v1/{*path}", any(echo))
            .route("/media/{*path}", get(echo))
            .route("/_wm/{*path}", post(echo));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, server).await.unwrap() });
        format!("http://{address}")
    }

    fn admin(base_url: &str) -> Router {
        router(Admin {
            upstream: Upstream::new(base_url, "secret".into()).unwrap(),
            allowed_hosts: vec!["localhost".to_owned(), "::1".to_owned()].into(),
            auth: None,
        })
    }

    async fn send(app: &Router, request: Request) -> (StatusCode, HeaderMap, String) {
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, headers, String::from_utf8_lossy(&body).into_owned())
    }

    fn request(method: Method, uri: &str) -> axum::http::request::Builder {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, "localhost:3001")
    }

    #[tokio::test]
    async fn serves_the_page_and_the_editor() {
        let app = admin("http://127.0.0.1:9");
        let (status, headers, body) =
            send(&app, request(Method::GET, "/").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("<script type=\"module\" src=\"/app.js\">"),
            "{body}"
        );
        assert_eq!(headers[header::X_FRAME_OPTIONS], "SAMEORIGIN");
        let csp = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
        assert!(
            csp.contains("script-src 'self' 'wasm-unsafe-eval';"),
            "{csp}"
        );
        assert!(!csp.contains("sandbox"), "{csp}");

        let (status, headers, _) = send(
            &app,
            request(Method::GET, "/vendor/asciidoc_editor_bg.wasm")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "application/wasm");
    }

    #[tokio::test]
    async fn rejects_unknown_hosts() {
        let app = admin("http://127.0.0.1:9");
        for host in [
            "evil.example",
            "evil.example:3001",
            "localhost.evil.example",
        ] {
            let (status, ..) = send(
                &app,
                Request::get("/")
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{host}");
        }
        let (status, ..) = send(
            &app,
            Request::get("/")
                .header(header::HOST, "[::1]:3001")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn api_calls_carry_the_token() {
        let app = admin(&fake_server().await);

        let (status, _, body) = send(
            &app,
            request(Method::PATCH, "/api/v1/pages/about?x=1")
                .header(API_GUARD_HEADER, "1")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, "session=private")
                .body(Body::from(r#"{"title":"About"}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let seen: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(seen["method"], "PATCH");
        assert_eq!(seen["uri"], "/api/v1/pages/about?x=1");
        assert_eq!(seen["authorization"], "Bearer secret");
        assert_eq!(seen["cookie"], serde_json::Value::Null);
        assert_eq!(seen["body"], r#"{"title":"About"}"#);
    }

    #[tokio::test]
    async fn api_calls_need_the_guard_header() {
        let app = admin(&fake_server().await);
        let (status, _, body) = send(
            &app,
            request(Method::POST, "/api/v1/pages/about/publish")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    }

    #[tokio::test]
    async fn public_files_go_without_the_token() {
        let app = admin(&fake_server().await);
        let (status, headers, body) = send(
            &app,
            request(Method::GET, "/media/logo.png")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let seen: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(seen["authorization"], serde_json::Value::Null);
        // Opened on their own, the site's files cannot script this origin.
        let csp = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
        assert!(csp.starts_with("default-src 'none';"), "{csp}");
        assert!(csp.ends_with("sandbox"), "{csp}");
        assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");

        // Only reading is passed on.
        let (status, ..) = send(
            &app,
            request(Method::POST, "/_wm/style.css")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn paths_cannot_step_out_of_their_prefix() {
        let app = admin(&fake_server().await);
        for uri in [
            "/api/v1/../../x",
            "/api/v1/./pages",
            "/api/v1/%2e%2e/%2E%2E/x",
            "/api/v1/pages%2f..%2fx",
            "/api/v1/..%5cx",
            "/api/v1/..\\x",
            "/media/../about",
            "/_wm/..",
        ] {
            let (status, _, body) = send(
                &app,
                request(Method::GET, uri)
                    .header(API_GUARD_HEADER, "1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body}");
        }

        // Dots inside a name are fine.
        let (status, _, body) = send(
            &app,
            request(Method::GET, "/media/logo.v2..png")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    #[tokio::test]
    async fn unreachable_server_is_a_bad_gateway() {
        // Nothing listens on the discard port.
        let app = admin("http://127.0.0.1:9");
        let (status, _, body) = send(
            &app,
            request(Method::GET, "/api/v1/pages")
                .header(API_GUARD_HEADER, "1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(body.contains("cannot reach the WordMaker server"), "{body}");
    }

    #[test]
    fn hostnames() {
        assert_eq!(hostname("localhost:3001"), "localhost");
        assert_eq!(hostname("localhost"), "localhost");
        assert_eq!(hostname("[::1]:3001"), "::1");
        assert_eq!(hostname("[::1]"), "::1");
    }
}
