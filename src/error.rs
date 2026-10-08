use axum::{
    Json,
    http::StatusCode,
    response::{
        IntoResponse,
        Response,
    },
};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{0} not found")]
    NotFound(String),
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Conflict(String),
    #[error("missing or invalid API token")]
    Unauthorized,
    #[error("database error: {0}")]
    Database(sqlx::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("internal error: {0}")]
    Internal(String),
    /// A feature that is not set up on this server.
    #[error("{0}")]
    Off(String),
    /// A service this request depends on did not answer.
    #[error("{0}")]
    Unavailable(String),
}

pub type AppResult<T> = Result<T, AppError>;

impl From<sqlx::Error> for AppError {
    fn from(err: sqlx::Error) -> Self {
        match &err {
            sqlx::Error::Database(db) if db.is_unique_violation() => {
                Self::Conflict("an entry with this name already exists".into())
            }
            _ => Self::Database(err),
        }
    }
}

impl AppError {
    pub fn status(&self) -> StatusCode {
        match self {
            Self::NotFound(_) | Self::Off(_) => StatusCode::NOT_FOUND,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Database(_) | Self::Io(_) | Self::Internal(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status();
        if status.is_server_error() {
            tracing::error!(error = %self, "request failed");
        }
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}
