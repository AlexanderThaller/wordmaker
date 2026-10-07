use axum::{
    Json,
    Router,
    extract::{
        DefaultBodyLimit,
        Multipart,
        Path,
        State,
    },
    http::StatusCode,
    routing::{
        delete,
        get,
    },
};

use crate::{
    app::AppState,
    domain::media::{
        self,
        MediaView,
    },
    error::{
        AppError,
        AppResult,
    },
};

pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/media",
            get(list)
                .post(upload)
                // Leave room for multipart framing around the file itself.
                .layer(DefaultBodyLimit::max(
                    state.config.max_upload_bytes + 64 * 1024,
                )),
        )
        .route("/media/{filename}", delete(remove))
}

async fn list(State(state): State<AppState>) -> AppResult<Json<Vec<MediaView>>> {
    let media = media::list(&state.pool).await?;
    Ok(Json(
        media
            .into_iter()
            .map(|m| m.into_view(&state.config))
            .collect(),
    ))
}

/// Accepts one or more `file` parts; returns the stored entries.
async fn upload(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> AppResult<(StatusCode, Json<Vec<MediaView>>)> {
    let mut stored = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|err| AppError::BadRequest(err.to_string()))?
    {
        let Some(name) = field.file_name().map(str::to_owned) else {
            continue;
        };
        let content_type = field.content_type().map(str::to_owned);
        let bytes = field
            .bytes()
            .await
            .map_err(|err| AppError::BadRequest(err.to_string()))?;
        let media = media::store(
            &state.pool,
            &state.config.media_dir,
            &name,
            content_type.as_deref(),
            &bytes,
            state.config.max_upload_bytes,
        )
        .await?;
        stored.push(media.into_view(&state.config));
    }
    if stored.is_empty() {
        return Err(AppError::BadRequest("no file parts in upload".into()));
    }
    Ok((StatusCode::CREATED, Json(stored)))
}

async fn remove(
    State(state): State<AppState>,
    Path(filename): Path<String>,
) -> AppResult<StatusCode> {
    media::delete(&state.pool, &state.config.media_dir, &filename).await?;
    Ok(StatusCode::NO_CONTENT)
}
