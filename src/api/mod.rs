mod media;
mod pages;
mod themes;

use axum::{
    Json,
    Router,
    extract::State,
    routing::{
        get,
        post,
    },
};
use serde::Deserialize;

use crate::{
    app::AppState,
    domain::settings::{
        self,
        Settings,
        SettingsUpdate,
    },
    error::AppResult,
    render::{
        self,
        Rendered,
    },
};

pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .merge(pages::router())
        .merge(media::router(state))
        .merge(themes::router())
        .route("/render", post(render_preview))
        .route("/settings", get(get_settings).put(put_settings))
}

#[derive(Deserialize)]
struct RenderRequest {
    body: String,
}

async fn render_preview(Json(request): Json<RenderRequest>) -> AppResult<Json<Rendered>> {
    Ok(Json(render::render_async(request.body).await?))
}

async fn get_settings(State(state): State<AppState>) -> AppResult<Json<Settings>> {
    Ok(Json(settings::get(&state.pool).await?))
}

async fn put_settings(
    State(state): State<AppState>,
    Json(update): Json<SettingsUpdate>,
) -> AppResult<Json<Settings>> {
    Ok(Json(settings::update(&state.pool, update).await?))
}
