use axum::{
    Json,
    Router,
    extract::{
        Query,
        State,
    },
    routing::get,
};
use serde::Deserialize;

use crate::{
    analytics::report::Report,
    app::AppState,
    error::{
        AppError,
        AppResult,
    },
};

/// Longest period a report covers.
const MAX_DAYS: u32 = 366;

pub fn router() -> Router<AppState> {
    Router::new().route("/analytics", get(report))
}

#[derive(Deserialize)]
struct ReportQuery {
    /// Days back from today, today included.
    #[serde(default = "default_days")]
    days: u32,
}

fn default_days() -> u32 {
    30
}

async fn report(
    State(state): State<AppState>,
    Query(query): Query<ReportQuery>,
) -> AppResult<Json<Report>> {
    let Some(analytics) = &state.analytics else {
        return Err(AppError::Off(
            "analytics is off: set WORDMAKER_CLICKHOUSE_URL to turn it on".into(),
        ));
    };
    if !(1..=MAX_DAYS).contains(&query.days) {
        return Err(AppError::BadRequest(format!(
            "days must be between 1 and {MAX_DAYS}"
        )));
    }
    let report = analytics
        .report(query.days)
        .await
        .map_err(|err| AppError::Unavailable(format!("cannot read analytics: {err:#}")))?;
    Ok(Json(report))
}
