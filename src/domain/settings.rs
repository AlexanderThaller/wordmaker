use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::{domain::pages, error::AppResult};

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Settings {
    /// Site name shown in the header and page titles.
    pub site_title: String,
    /// Slug of the page shown at `/`. Without one, `/` lists all published pages.
    pub home_slug: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct SettingsUpdate {
    /// New site name.
    #[serde(default)]
    pub site_title: Option<String>,
    /// Slug of the page to show at `/`; an empty string clears it.
    #[serde(default)]
    pub home_slug: Option<String>,
}

const SITE_TITLE: &str = "site_title";
const HOME_SLUG: &str = "home_slug";

pub async fn get(pool: &SqlitePool) -> AppResult<Settings> {
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT key, value FROM settings")
        .fetch_all(pool)
        .await?;
    let mut settings = Settings {
        site_title: "WordMaker".into(),
        home_slug: None,
    };
    for (key, value) in rows {
        match key.as_str() {
            SITE_TITLE => settings.site_title = value,
            HOME_SLUG => settings.home_slug = Some(value),
            _ => {}
        }
    }
    Ok(settings)
}

pub async fn update(pool: &SqlitePool, update: SettingsUpdate) -> AppResult<Settings> {
    let mut tx = pool.begin().await?;
    if let Some(title) = update.site_title {
        set(&mut tx, SITE_TITLE, title.trim()).await?;
    }
    match update.home_slug.as_deref().map(str::trim) {
        Some("") => {
            sqlx::query("DELETE FROM settings WHERE key = ?")
                .bind(HOME_SLUG)
                .execute(&mut *tx)
                .await?;
        }
        Some(slug) => set(&mut tx, HOME_SLUG, &pages::validate_slug(slug)?).await?,
        None => {}
    }
    tx.commit().await?;
    get(pool).await
}

async fn set(conn: &mut sqlx::SqliteConnection, key: &str, value: &str) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO settings (key, value) VALUES (?, ?)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(conn)
    .await?;
    Ok(())
}
