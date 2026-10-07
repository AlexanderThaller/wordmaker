use std::path::Path;

use chrono::{
    DateTime,
    Utc,
};
use schemars::JsonSchema;
use serde::Serialize;
use sqlx::SqlitePool;

use crate::error::{
    AppError,
    AppResult,
};

#[derive(Debug, Clone, Serialize, JsonSchema, sqlx::FromRow)]
pub struct Media {
    pub id: i64,
    /// Stored file name; the file is served at `/media/<filename>`.
    pub filename: String,
    pub original_name: String,
    pub content_type: String,
    pub size: i64,
    pub created_at: DateTime<Utc>,
}

impl Media {
    /// Site-relative path, suitable for `image::` macros in `AsciiDoc`.
    pub fn path(&self) -> String {
        format!("/media/{}", self.filename)
    }
}

/// Store an uploaded file on disk and record it.
pub async fn store(
    pool: &SqlitePool,
    media_dir: &Path,
    original_name: &str,
    content_type: Option<&str>,
    bytes: &[u8],
    max_bytes: usize,
) -> AppResult<Media> {
    if bytes.is_empty() {
        return Err(AppError::BadRequest("uploaded file is empty".into()));
    }
    if bytes.len() > max_bytes {
        return Err(AppError::BadRequest(format!(
            "file is {} bytes, the limit is {max_bytes}",
            bytes.len()
        )));
    }

    let original_name = original_name.trim();
    let id = uuid::Uuid::new_v4().simple().to_string();
    let filename = format!("{}-{}", &id[..8], sanitize_filename(original_name));
    let content_type = content_type
        .filter(|ct| !ct.is_empty() && *ct != "application/octet-stream")
        .map_or_else(
            || {
                mime_guess::from_path(&filename)
                    .first_or_octet_stream()
                    .to_string()
            },
            str::to_owned,
        );

    tokio::fs::create_dir_all(media_dir).await?;
    let path = media_dir.join(&filename);
    tokio::fs::write(&path, bytes).await?;

    let inserted = sqlx::query_as(
        "INSERT INTO media (filename, original_name, content_type, size, created_at)
         VALUES (?, ?, ?, ?, ?)
         RETURNING id, filename, original_name, content_type, size, created_at",
    )
    .bind(&filename)
    .bind(original_name)
    .bind(&content_type)
    .bind(i64::try_from(bytes.len()).unwrap_or(i64::MAX))
    .bind(Utc::now())
    .fetch_one(pool)
    .await;

    match inserted {
        Ok(media) => Ok(media),
        Err(err) => {
            let _ = tokio::fs::remove_file(&path).await;
            Err(err.into())
        }
    }
}

pub async fn list(pool: &SqlitePool) -> AppResult<Vec<Media>> {
    let media = sqlx::query_as(
        "SELECT id, filename, original_name, content_type, size, created_at
         FROM media ORDER BY created_at DESC",
    )
    .fetch_all(pool)
    .await?;
    Ok(media)
}

pub async fn delete(pool: &SqlitePool, media_dir: &Path, filename: &str) -> AppResult<()> {
    let result = sqlx::query("DELETE FROM media WHERE filename = ?")
        .bind(filename)
        .execute(pool)
        .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("media '{filename}'")));
    }
    // The name came out of the database, so it is one we sanitized ourselves.
    match tokio::fs::remove_file(media_dir.join(filename)).await {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err.into()),
        _ => Ok(()),
    }
}

/// Reduce a client supplied name to something safe to use as a path segment.
fn sanitize_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let mut out = String::with_capacity(base.len());
    for c in base.chars() {
        match c {
            c if c.is_ascii_alphanumeric() || c == '.' || c == '_' => {
                out.push(c.to_ascii_lowercase());
            }
            _ if !out.ends_with('-') => out.push('-'),
            _ => {}
        }
    }
    let out = out.trim_matches(['-', '.']);
    if out.is_empty() {
        "file".to_owned()
    } else {
        out.chars().take(80).collect()
    }
}

/// A media entry as returned to API and MCP clients.
#[derive(Debug, Serialize)]
pub struct MediaView {
    #[serde(flatten)]
    pub media: Media,
    /// Site-relative path to use in `AsciiDoc`, e.g. `image::/media/x.png[]`.
    pub path: String,
    pub url: String,
}

impl Media {
    pub fn into_view(self, config: &crate::config::Config) -> MediaView {
        MediaView {
            path: self.path(),
            url: config.media_url(&self.filename),
            media: self,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::connect_memory;

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("My Photo (1).JPG"), "my-photo-1-.jpg");
        assert_eq!(sanitize_filename(".."), "file");
    }

    #[tokio::test]
    async fn store_list_delete() {
        let pool = connect_memory().await;
        let dir = std::env::temp_dir().join(format!("wordmaker-test-{}", uuid::Uuid::new_v4()));

        let media = store(&pool, &dir, "logo.png", None, b"png-bytes", 1024)
            .await
            .unwrap();
        assert_eq!(media.content_type, "image/png");
        assert!(dir.join(&media.filename).exists());
        assert_eq!(list(&pool).await.unwrap().len(), 1);

        assert!(
            store(&pool, &dir, "big.bin", None, &[0; 2048], 1024)
                .await
                .is_err()
        );

        delete(&pool, &dir, &media.filename).await.unwrap();
        assert!(!dir.join(&media.filename).exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
