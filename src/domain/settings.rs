use schemars::JsonSchema;
use serde::{
    Deserialize,
    Serialize,
};
use sqlx::{
    SqliteConnection,
    SqlitePool,
};

use crate::{
    domain::pages,
    error::{
        AppError,
        AppResult,
    },
};

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Settings {
    /// Site name shown in the header and page titles.
    pub site_title: String,
    /// Language of the content, as an HTML `lang` value such as `en` or `de`.
    pub site_language: String,
    /// Short line shown under the site title in the footer.
    pub site_tagline: Option<String>,
    /// `AsciiDoc` shown in the footer.
    pub footer_text: Option<String>,
    /// Slug of the page shown at `/`. Without one, `/` lists all published
    /// pages.
    pub home_slug: Option<String>,
    /// Active theme, or none for the built-in look.
    pub theme: Option<String>,
    /// Style variations of the active theme, applied in order.
    pub theme_variations: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct SettingsUpdate {
    /// New site name.
    #[serde(default)]
    pub site_title: Option<String>,
    /// Language of the content, e.g. `en` or `de`.
    #[serde(default)]
    pub site_language: Option<String>,
    /// New tagline; an empty string removes it.
    #[serde(default)]
    pub site_tagline: Option<String>,
    /// New footer text in `AsciiDoc`; an empty string removes it.
    #[serde(default)]
    pub footer_text: Option<String>,
    /// Slug of the page to show at `/`; an empty string clears it.
    #[serde(default)]
    pub home_slug: Option<String>,
}

const SITE_TITLE: &str = "site_title";
const SITE_LANGUAGE: &str = "site_language";
const SITE_TAGLINE: &str = "site_tagline";
const FOOTER_TEXT: &str = "footer_text";
const HOME_SLUG: &str = "home_slug";
const THEME: &str = "theme";
const THEME_VARIATIONS: &str = "theme_variations";

pub async fn get(pool: &SqlitePool) -> AppResult<Settings> {
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT key, value FROM settings")
        .fetch_all(pool)
        .await?;
    let mut settings = Settings {
        site_title: "WordMaker".into(),
        site_language: "en".into(),
        site_tagline: None,
        footer_text: None,
        home_slug: None,
        theme: None,
        theme_variations: Vec::new(),
    };
    for (key, value) in rows {
        match key.as_str() {
            SITE_TITLE => settings.site_title = value,
            SITE_LANGUAGE => settings.site_language = value,
            SITE_TAGLINE => settings.site_tagline = Some(value),
            FOOTER_TEXT => settings.footer_text = Some(value),
            HOME_SLUG => settings.home_slug = Some(value),
            THEME => settings.theme = Some(value),
            THEME_VARIATIONS => {
                settings.theme_variations = serde_json::from_str(&value).unwrap_or_default();
            }
            _ => {}
        }
    }
    Ok(settings)
}

pub async fn update(pool: &SqlitePool, update: SettingsUpdate) -> AppResult<Settings> {
    let mut tx = pool.begin().await?;
    if let Some(title) = update.site_title {
        set(&mut tx, SITE_TITLE, Some(title.trim())).await?;
    }
    if let Some(language) = update.site_language {
        let language = language.trim();
        let valid = (2..=35).contains(&language.len())
            && language
                .split('-')
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric()));
        if !valid {
            return Err(AppError::BadRequest(format!(
                "invalid language '{language}': use a tag like 'en' or 'de-DE'"
            )));
        }
        set(&mut tx, SITE_LANGUAGE, Some(language)).await?;
    }
    if let Some(tagline) = update.site_tagline {
        set(&mut tx, SITE_TAGLINE, non_empty(tagline.trim())).await?;
    }
    if let Some(footer) = update.footer_text {
        set(&mut tx, FOOTER_TEXT, non_empty(footer.trim())).await?;
    }
    if let Some(slug) = update.home_slug {
        let slug = match non_empty(slug.trim()) {
            Some(slug) => Some(pages::validate_slug(slug)?),
            None => None,
        };
        set(&mut tx, HOME_SLUG, slug.as_deref()).await?;
    }
    tx.commit().await?;
    get(pool).await
}

/// Record the active theme. The caller checks that it exists.
pub async fn set_theme(
    pool: &SqlitePool,
    theme: Option<&str>,
    variations: &[String],
) -> AppResult<Settings> {
    let mut tx = pool.begin().await?;
    set(&mut tx, THEME, theme).await?;
    let variations = serde_json::to_string(variations).expect("strings serialize");
    set(
        &mut tx,
        THEME_VARIATIONS,
        theme.map(|_| variations.as_str()),
    )
    .await?;
    tx.commit().await?;
    get(pool).await
}

fn non_empty(value: &str) -> Option<&str> {
    (!value.is_empty()).then_some(value)
}

/// Store a value, or remove the key when `value` is `None`.
async fn set(conn: &mut SqliteConnection, key: &str, value: Option<&str>) -> AppResult<()> {
    match value {
        Some(value) => {
            sqlx::query(
                "INSERT INTO settings (key, value) VALUES (?, ?)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            )
            .bind(key)
            .bind(value)
            .execute(conn)
            .await?;
        }
        None => {
            sqlx::query("DELETE FROM settings WHERE key = ?")
                .bind(key)
                .execute(conn)
                .await?;
        }
    }
    Ok(())
}
