use std::sync::Arc;

use axum::{
    Router,
    extract::{
        Request,
        State,
    },
    http::header,
    middleware::{
        self,
        Next,
    },
    response::Response,
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig,
    StreamableHttpService,
    session::local::LocalSessionManager,
};
use sqlx::SqlitePool;
use subtle::ConstantTimeEq;
use tower_http::trace::TraceLayer;

use crate::{
    api,
    config::Config,
    error::AppError,
    mcp::WordMakerMcp,
    site,
    theme::ThemeCache,
};

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub config: Arc<Config>,
    pub token: Arc<str>,
    pub themes: Arc<ThemeCache>,
}

/// The whole application: public site, `/api/v1` and `/mcp`.
pub fn router(state: AppState, mcp_allowed_hosts: Vec<String>) -> Router {
    let mcp = {
        let pool = state.pool.clone();
        let config = state.config.clone();
        let mut mcp_config = StreamableHttpServerConfig::default();
        mcp_config.allowed_hosts = mcp_allowed_hosts;
        mcp_config.max_request_body_bytes = state.config.max_upload_bytes * 2;
        StreamableHttpService::new(
            move || Ok(WordMakerMcp::new(pool.clone(), config.clone())),
            LocalSessionManager::default().into(),
            mcp_config,
        )
    };

    let protected = Router::new()
        .nest("/api/v1", api::router(&state))
        .nest_service("/mcp", mcp)
        .layer(middleware::from_fn_with_state(state.clone(), require_token));

    Router::new()
        .merge(protected)
        .merge(site::router(&state))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn require_token(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default();
    if presented.as_bytes().ct_eq(state.token.as_bytes()).into() {
        Ok(next.run(request).await)
    } else {
        Err(AppError::Unauthorized)
    }
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
        };
        router(state, vec!["localhost".into()])
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
        assert!(html.contains("href=\"/_wm/style.css?v="), "{html}");

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
                r##"{"slug":"section-1","blockTypes":["core/group"],
                     "styles":{"color":{"background":"var:preset|color|contrast"}}}"##,
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
            css.contains("--wp--preset--color--base: #000000;"),
            "variation applied"
        );
        assert!(css.contains("url('/_wm/theme/mytheme/assets/fonts/body.woff2')"));
        assert!(css.contains(":root :where(.is-style-section-1)"));
        assert!(
            css.contains("text-underline-offset: .1em"),
            "style.css included"
        );
        assert!(
            css.contains("color-scheme: dark"),
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
}
