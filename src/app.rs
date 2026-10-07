use std::sync::Arc;

use axum::{
    Router,
    extract::{Request, State},
    http::header,
    middleware::{self, Next},
    response::Response,
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use sqlx::SqlitePool;
use subtle::ConstantTimeEq;
use tower_http::trace::TraceLayer;

use crate::{api, config::Config, error::AppError, mcp::WordMakerMcp, site};

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub config: Arc<Config>,
    pub token: Arc<str>,
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
        http::{Method, Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use super::*;

    async fn app() -> Router {
        let config = Config {
            database_url: String::new(),
            media_dir: std::env::temp_dir().join("wordmaker-app-test"),
            base_url: "http://localhost:3000".into(),
            max_upload_bytes: 1024,
        };
        let state = AppState {
            pool: crate::db::connect_memory().await,
            config: Arc::new(config),
            token: "secret".into(),
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
        assert!(html.contains("<h1>Hello World</h1>"), "{html}");

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
}
