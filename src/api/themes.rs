use axum::{
    Json,
    Router,
    body::Bytes,
    extract::{
        DefaultBodyLimit,
        Path,
        Query,
        State,
    },
    http::StatusCode,
    routing::{
        get,
        post,
        put,
    },
};
use serde::Deserialize;

use crate::{
    app::AppState,
    domain::settings::Settings,
    error::{
        AppError,
        AppResult,
    },
    theme::{
        self,
        ThemeDetails,
        ThemeFile,
        ThemeFileEntry,
        ThemeInfo,
    },
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/themes", get(list))
        .route(
            "/themes/upload",
            post(upload).layer(DefaultBodyLimit::max(100 * 1024 * 1024)),
        )
        .route("/themes/install", post(install))
        .route("/themes/active", put(activate))
        .route("/themes/{slug}", get(details).delete(remove))
        .route(
            "/themes/{slug}/files/{*path}",
            get(read_file).put(write_file),
        )
}

async fn list(State(state): State<AppState>) -> AppResult<Json<Vec<ThemeInfo>>> {
    Ok(Json(theme::list(&state.config.themes_dir).await?))
}

#[derive(Deserialize)]
struct UploadQuery {
    slug: Option<String>,
}

/// Body: the theme zip, as `application/zip`.
async fn upload(
    State(state): State<AppState>,
    Query(query): Query<UploadQuery>,
    body: Bytes,
) -> AppResult<(StatusCode, Json<ThemeInfo>)> {
    let info = theme::install_zip(
        &state.config.themes_dir,
        body.to_vec(),
        query.slug.as_deref(),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(info)))
}

#[derive(Deserialize)]
struct InstallRequest {
    /// URL of a theme zip.
    url: Option<String>,
    /// Theme in the wordpress.org directory, e.g. `twentytwentyfive`.
    wordpress_org: Option<String>,
    /// Version for `wordpress_org`; latest when omitted.
    version: Option<String>,
    /// Name to install under; defaults to the zip's folder name.
    slug: Option<String>,
}

async fn install(
    State(state): State<AppState>,
    Json(request): Json<InstallRequest>,
) -> AppResult<(StatusCode, Json<ThemeInfo>)> {
    let (url, slug) = match (&request.url, &request.wordpress_org) {
        (Some(url), None) => (url.clone(), request.slug.clone()),
        (None, Some(name)) => (
            theme::wordpress_org_url(name, request.version.as_deref())?,
            request.slug.clone().or_else(|| Some(name.clone())),
        ),
        _ => {
            return Err(AppError::BadRequest(
                "give exactly one of `url` or `wordpress_org`".into(),
            ));
        }
    };
    let info = theme::install_from_url(&state.config.themes_dir, &url, slug.as_deref()).await?;
    Ok((StatusCode::CREATED, Json(info)))
}

#[derive(Deserialize)]
struct ActivateRequest {
    /// Theme slug, or null for the built-in look.
    theme: Option<String>,
    #[serde(default)]
    variations: Vec<String>,
}

async fn activate(
    State(state): State<AppState>,
    Json(request): Json<ActivateRequest>,
) -> AppResult<Json<Settings>> {
    let settings = theme::activate(
        &state.pool,
        &state.config.themes_dir,
        request.theme.as_deref(),
        request.variations,
    )
    .await?;
    Ok(Json(settings))
}

async fn remove(State(state): State<AppState>, Path(slug): Path<String>) -> AppResult<StatusCode> {
    theme::delete(&state.pool, &state.config.themes_dir, &slug).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn details(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> AppResult<Json<ThemeDetails>> {
    Ok(Json(theme::details(&state.config.themes_dir, &slug).await?))
}

async fn read_file(
    State(state): State<AppState>,
    Path((slug, path)): Path<(String, String)>,
) -> AppResult<Json<ThemeFile>> {
    Ok(Json(
        theme::read_file(&state.config.themes_dir, &slug, &path).await?,
    ))
}

#[derive(Deserialize)]
struct WriteFileRequest {
    content: String,
}

async fn write_file(
    State(state): State<AppState>,
    Path((slug, path)): Path<(String, String)>,
    Json(request): Json<WriteFileRequest>,
) -> AppResult<Json<ThemeFileEntry>> {
    Ok(Json(
        theme::write_file(&state.config.themes_dir, &slug, &path, request.content).await?,
    ))
}
