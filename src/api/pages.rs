use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::Deserialize;

use crate::{
    app::AppState,
    domain::pages::{
        self, NewPage, PageSummary, PageUpdate, PageView, Revision, RevisionSummary, Status,
    },
    error::AppResult,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/pages", get(list).post(create))
        .route("/pages/{slug}", get(show).patch(update).delete(delete))
        .route("/pages/{slug}/publish", post(publish))
        .route("/pages/{slug}/unpublish", post(unpublish))
        .route("/pages/{slug}/revisions", get(list_revisions))
        .route("/pages/{slug}/revisions/{revision}", get(show_revision))
        .route(
            "/pages/{slug}/revisions/{revision}/restore",
            post(restore_revision),
        )
}

#[derive(Deserialize)]
struct ListQuery {
    status: Option<Status>,
}

async fn list(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> AppResult<Json<Vec<PageSummary>>> {
    Ok(Json(pages::list(&state.pool, query.status).await?))
}

async fn create(
    State(state): State<AppState>,
    Json(new): Json<NewPage>,
) -> AppResult<(StatusCode, Json<PageView>)> {
    let page = pages::create(&state.pool, new).await?;
    Ok((StatusCode::CREATED, Json(page.into_view(&state.config))))
}

async fn show(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> AppResult<Json<PageView>> {
    let page = pages::get(&state.pool, &slug).await?;
    Ok(Json(page.into_view(&state.config)))
}

async fn update(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Json(update): Json<PageUpdate>,
) -> AppResult<Json<PageView>> {
    let page = pages::update(&state.pool, &slug, update).await?;
    Ok(Json(page.into_view(&state.config)))
}

async fn delete(State(state): State<AppState>, Path(slug): Path<String>) -> AppResult<StatusCode> {
    pages::delete(&state.pool, &slug).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn publish(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> AppResult<Json<PageView>> {
    let page = pages::set_status(&state.pool, &slug, Status::Published).await?;
    Ok(Json(page.into_view(&state.config)))
}

async fn unpublish(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> AppResult<Json<PageView>> {
    let page = pages::set_status(&state.pool, &slug, Status::Draft).await?;
    Ok(Json(page.into_view(&state.config)))
}

async fn list_revisions(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> AppResult<Json<Vec<RevisionSummary>>> {
    Ok(Json(pages::list_revisions(&state.pool, &slug).await?))
}

async fn show_revision(
    State(state): State<AppState>,
    Path((slug, revision)): Path<(String, i64)>,
) -> AppResult<Json<Revision>> {
    Ok(Json(
        pages::get_revision(&state.pool, &slug, revision).await?,
    ))
}

async fn restore_revision(
    State(state): State<AppState>,
    Path((slug, revision)): Path<(String, i64)>,
) -> AppResult<Json<PageView>> {
    let page = pages::restore_revision(&state.pool, &slug, revision).await?;
    Ok(Json(page.into_view(&state.config)))
}
