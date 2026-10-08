use chrono::{
    DateTime,
    Utc,
};
use schemars::JsonSchema;
use serde::{
    Deserialize,
    Serialize,
};
use sqlx::{
    SqliteConnection,
    SqlitePool,
};

use crate::error::{
    AppError,
    AppResult,
};

/// Slugs that would collide with the site's own routes.
const RESERVED_SLUGS: &[&str] = &["api", "mcp", "media", "favicon-ico", "robots-txt"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(rename_all = "lowercase")]
pub enum Status {
    Draft,
    Published,
}

#[derive(Debug, Clone, Serialize, JsonSchema, sqlx::FromRow)]
pub struct Page {
    pub id: i64,
    pub slug: String,
    pub title: String,
    /// `AsciiDoc` source.
    pub body: String,
    pub status: Status,
    /// Whether the page is linked from the site navigation.
    pub in_nav: bool,
    /// Position in the navigation; lower comes first, ties sort by title.
    pub nav_order: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub published_at: Option<DateTime<Utc>>,
    /// Title of the pending draft: changes saved but not yet published.
    pub draft_title: Option<String>,
    /// `AsciiDoc` body of the pending draft. The site keeps showing `body`
    /// until the page is published again.
    pub draft_body: Option<String>,
    pub draft_updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, sqlx::FromRow)]
pub struct PageSummary {
    pub id: i64,
    pub slug: String,
    pub title: String,
    pub status: Status,
    pub in_nav: bool,
    pub nav_order: i64,
    pub updated_at: DateTime<Utc>,
    pub published_at: Option<DateTime<Utc>>,
    /// Whether there are saved changes that are not published yet.
    pub has_draft: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema, sqlx::FromRow)]
#[expect(
    clippy::struct_field_names,
    reason = "`revision` is the column and the API field clients know"
)]
pub struct Revision {
    pub revision: i64,
    pub title: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, sqlx::FromRow)]
pub struct RevisionSummary {
    pub revision: i64,
    pub title: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct NewPage {
    /// Page title, shown as the page heading.
    pub title: String,
    /// URL slug (lowercase letters, digits and dashes). Derived from the title
    /// when omitted.
    #[serde(default)]
    pub slug: Option<String>,
    /// Page content in `AsciiDoc`. Do not repeat the title as a `= Title` line.
    #[serde(default)]
    pub body: String,
    /// Publish immediately instead of creating a draft.
    #[serde(default)]
    pub publish: bool,
    /// Link the page from the site navigation (default true).
    #[serde(default)]
    pub in_nav: Option<bool>,
    /// Position in the navigation; lower comes first (default 0).
    #[serde(default)]
    pub nav_order: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct PageUpdate {
    /// New title.
    #[serde(default)]
    pub title: Option<String>,
    /// New slug; the page moves to the new URL.
    #[serde(default)]
    pub slug: Option<String>,
    /// New `AsciiDoc` body, replacing the old one entirely.
    #[serde(default)]
    pub body: Option<String>,
    /// Whether the page is linked from the site navigation.
    #[serde(default)]
    pub in_nav: Option<bool>,
    /// Position in the navigation; lower comes first.
    #[serde(default)]
    pub nav_order: Option<i64>,
}

/// Changes to keep without publishing them.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct DraftUpdate {
    /// New title; omitted, the draft's (or the page's) title stays.
    #[serde(default)]
    pub title: Option<String>,
    /// New `AsciiDoc` body, replacing the whole body; omitted, the draft's
    /// (or the page's) body stays.
    #[serde(default)]
    pub body: Option<String>,
}

const PAGE_COLUMNS: &str = "id, slug, title, body, status, in_nav, nav_order, created_at, \
                            updated_at, published_at, draft_title, draft_body, draft_updated_at";

const SUMMARY_COLUMNS: &str = "id, slug, title, status, in_nav, nav_order, updated_at, \
                               published_at, draft_body IS NOT NULL AS has_draft";

pub async fn list(pool: &SqlitePool, status: Option<Status>) -> AppResult<Vec<PageSummary>> {
    let pages = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {SUMMARY_COLUMNS} FROM pages
         WHERE ?1 IS NULL OR status = ?1
         ORDER BY title COLLATE NOCASE"
    )))
    .bind(status)
    .fetch_all(pool)
    .await?;
    Ok(pages)
}

/// Published pages that belong in the site navigation, in order.
pub async fn navigation(pool: &SqlitePool) -> AppResult<Vec<PageSummary>> {
    let pages = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {SUMMARY_COLUMNS} FROM pages
         WHERE status = 'published' AND in_nav
         ORDER BY nav_order, title COLLATE NOCASE"
    )))
    .fetch_all(pool)
    .await?;
    Ok(pages)
}

pub async fn get(pool: &SqlitePool, slug: &str) -> AppResult<Page> {
    let mut conn = pool.acquire().await?;
    fetch(&mut conn, slug).await
}

async fn fetch(conn: &mut SqliteConnection, slug: &str) -> AppResult<Page> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {PAGE_COLUMNS} FROM pages WHERE slug = ?"
    )))
    .bind(slug)
    .fetch_optional(conn)
    .await?
    .ok_or_else(|| AppError::NotFound(format!("page '{slug}'")))
}

pub async fn create(pool: &SqlitePool, new: NewPage) -> AppResult<Page> {
    let title = validate_title(&new.title)?;
    let slug = match new.slug.as_deref() {
        Some(slug) => validate_slug(slug)?,
        None => validate_slug(&slugify(title))?,
    };
    let now = Utc::now();
    let (status, published_at) = if new.publish {
        (Status::Published, Some(now))
    } else {
        (Status::Draft, None)
    };

    let mut tx = pool.begin().await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO pages (slug, title, body, status, in_nav, nav_order, created_at, updated_at,
                            published_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(&slug)
    .bind(title)
    .bind(&new.body)
    .bind(status)
    .bind(new.in_nav.unwrap_or(true))
    .bind(new.nav_order.unwrap_or(0))
    .bind(now)
    .bind(now)
    .bind(published_at)
    .fetch_one(&mut *tx)
    .await
    .map_err(|err| slug_conflict(err, &slug))?;
    record_revision(&mut tx, id, title, &new.body, now).await?;
    let page = fetch(&mut tx, &slug).await?;
    tx.commit().await?;
    Ok(page)
}

pub async fn update(pool: &SqlitePool, slug: &str, update: PageUpdate) -> AppResult<Page> {
    let mut tx = pool.begin().await?;
    let page = fetch(&mut tx, slug).await?;

    let title = match update.title.as_deref() {
        Some(title) => validate_title(title)?.to_owned(),
        None => page.title.clone(),
    };
    let new_slug = match update.slug.as_deref() {
        Some(slug) => validate_slug(slug)?,
        None => page.slug.clone(),
    };
    let body = update.body.unwrap_or_else(|| page.body.clone());
    let now = Utc::now();

    sqlx::query(
        "UPDATE pages SET slug = ?, title = ?, body = ?, in_nav = ?, nav_order = ?, updated_at = ?
         WHERE id = ?",
    )
    .bind(&new_slug)
    .bind(&title)
    .bind(&body)
    .bind(update.in_nav.unwrap_or(page.in_nav))
    .bind(update.nav_order.unwrap_or(page.nav_order))
    .bind(now)
    .bind(page.id)
    .execute(&mut *tx)
    .await
    .map_err(|err| slug_conflict(err, &new_slug))?;

    if title != page.title || body != page.body {
        record_revision(&mut tx, page.id, &title, &body, now).await?;
    }

    let page = fetch(&mut tx, &new_slug).await?;
    tx.commit().await?;
    Ok(page)
}

pub async fn delete(pool: &SqlitePool, slug: &str) -> AppResult<()> {
    let result = sqlx::query("DELETE FROM pages WHERE slug = ?")
        .bind(slug)
        .execute(pool)
        .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("page '{slug}'")));
    }
    Ok(())
}

/// Publishing also makes a pending draft the live version of the page.
pub async fn set_status(pool: &SqlitePool, slug: &str, status: Status) -> AppResult<Page> {
    let mut tx = pool.begin().await?;
    let now = Utc::now();
    if status == Status::Published {
        apply_draft(&mut tx, slug, now).await?;
    }
    let result = sqlx::query(
        "UPDATE pages SET status = ?1, updated_at = ?2,
             published_at = CASE WHEN ?1 = 'published' THEN COALESCE(published_at, ?2)
                                 ELSE published_at END
         WHERE slug = ?3",
    )
    .bind(status)
    .bind(now)
    .bind(slug)
    .execute(&mut *tx)
    .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("page '{slug}'")));
    }
    let page = fetch(&mut tx, slug).await?;
    tx.commit().await?;
    Ok(page)
}

/// Keeps changes to the title and body without touching what the site
/// shows. Changes that leave the page as published remove the draft.
pub async fn save_draft(pool: &SqlitePool, slug: &str, update: DraftUpdate) -> AppResult<Page> {
    let mut tx = pool.begin().await?;
    let page = fetch(&mut tx, slug).await?;
    let title = match update.title.as_deref() {
        Some(title) => validate_title(title)?.to_owned(),
        None => page
            .draft_title
            .clone()
            .unwrap_or_else(|| page.title.clone()),
    };
    let body = update
        .body
        .or_else(|| page.draft_body.clone())
        .unwrap_or_else(|| page.body.clone());

    if title == page.title && body == page.body {
        clear_draft(&mut tx, page.id).await?;
    } else {
        sqlx::query(
            "UPDATE pages SET draft_title = ?, draft_body = ?, draft_updated_at = ? WHERE id = ?",
        )
        .bind(&title)
        .bind(&body)
        .bind(Utc::now())
        .bind(page.id)
        .execute(&mut *tx)
        .await?;
    }
    let page = fetch(&mut tx, slug).await?;
    tx.commit().await?;
    Ok(page)
}

/// Throws away the pending draft, if there is one.
pub async fn discard_draft(pool: &SqlitePool, slug: &str) -> AppResult<Page> {
    let mut tx = pool.begin().await?;
    let page = fetch(&mut tx, slug).await?;
    clear_draft(&mut tx, page.id).await?;
    let page = fetch(&mut tx, slug).await?;
    tx.commit().await?;
    Ok(page)
}

/// Makes the pending draft the page's title and body, as a new revision.
async fn apply_draft(conn: &mut SqliteConnection, slug: &str, now: DateTime<Utc>) -> AppResult<()> {
    let page = fetch(conn, slug).await?;
    let (Some(title), Some(body)) = (page.draft_title, page.draft_body) else {
        return Ok(());
    };
    sqlx::query("UPDATE pages SET title = ?, body = ?, updated_at = ? WHERE id = ?")
        .bind(&title)
        .bind(&body)
        .bind(now)
        .bind(page.id)
        .execute(&mut *conn)
        .await?;
    clear_draft(conn, page.id).await?;
    if title != page.title || body != page.body {
        record_revision(conn, page.id, &title, &body, now).await?;
    }
    Ok(())
}

async fn clear_draft(conn: &mut SqliteConnection, page_id: i64) -> AppResult<()> {
    sqlx::query(
        "UPDATE pages SET draft_title = NULL, draft_body = NULL, draft_updated_at = NULL
         WHERE id = ?",
    )
    .bind(page_id)
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn list_revisions(pool: &SqlitePool, slug: &str) -> AppResult<Vec<RevisionSummary>> {
    let page = get(pool, slug).await?;
    let revisions = sqlx::query_as(
        "SELECT revision, title, created_at FROM page_revisions
         WHERE page_id = ? ORDER BY revision DESC",
    )
    .bind(page.id)
    .fetch_all(pool)
    .await?;
    Ok(revisions)
}

pub async fn get_revision(pool: &SqlitePool, slug: &str, revision: i64) -> AppResult<Revision> {
    let page = get(pool, slug).await?;
    sqlx::query_as(
        "SELECT revision, title, body, created_at FROM page_revisions
         WHERE page_id = ? AND revision = ?",
    )
    .bind(page.id)
    .bind(revision)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound(format!("revision {revision} of page '{slug}'")))
}

/// Make an old revision current again. This adds a new revision rather than
/// discarding the ones after it.
pub async fn restore_revision(pool: &SqlitePool, slug: &str, revision: i64) -> AppResult<Page> {
    let old = get_revision(pool, slug, revision).await?;
    update(
        pool,
        slug,
        PageUpdate {
            title: Some(old.title),
            body: Some(old.body),
            ..Default::default()
        },
    )
    .await
}

async fn record_revision(
    conn: &mut SqliteConnection,
    page_id: i64,
    title: &str,
    body: &str,
    at: DateTime<Utc>,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO page_revisions (page_id, revision, title, body, created_at)
         VALUES (?1, (SELECT COALESCE(MAX(revision), 0) + 1 FROM page_revisions WHERE page_id = \
         ?1),
                 ?2, ?3, ?4)",
    )
    .bind(page_id)
    .bind(title)
    .bind(body)
    .bind(at)
    .execute(conn)
    .await?;
    Ok(())
}

fn slug_conflict(err: sqlx::Error, slug: &str) -> AppError {
    match AppError::from(err) {
        AppError::Conflict(_) => {
            AppError::Conflict(format!("a page with slug '{slug}' already exists"))
        }
        other => other,
    }
}

fn validate_title(title: &str) -> AppResult<&str> {
    let title = title.trim();
    if title.is_empty() {
        return Err(AppError::BadRequest("title must not be empty".into()));
    }
    Ok(title)
}

pub fn validate_slug(slug: &str) -> AppResult<String> {
    let valid = !slug.is_empty()
        && slug.len() <= 100
        && slug.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        });
    if !valid {
        return Err(AppError::BadRequest(format!(
            "invalid slug '{slug}': use lowercase letters and digits separated by single dashes"
        )));
    }
    if RESERVED_SLUGS.contains(&slug) {
        return Err(AppError::BadRequest(format!("slug '{slug}' is reserved")));
    }
    Ok(slug.to_owned())
}

/// Turn a title into a slug: "Über uns!" becomes "ueber-uns".
pub fn slugify(title: &str) -> String {
    let mut slug = String::with_capacity(title.len());
    for c in title.chars() {
        match c {
            'ä' | 'Ä' => slug.push_str("ae"),
            'ö' | 'Ö' => slug.push_str("oe"),
            'ü' | 'Ü' => slug.push_str("ue"),
            'ß' => slug.push_str("ss"),
            c if c.is_ascii_alphanumeric() => slug.push(c.to_ascii_lowercase()),
            _ if !slug.is_empty() && !slug.ends_with('-') => slug.push('-'),
            _ => {}
        }
    }
    // Only ASCII is pushed, so truncating at a byte index is safe.
    slug.truncate(100);
    slug.trim_end_matches('-').to_owned()
}

/// A page as returned to API and MCP clients, with its public URL.
#[derive(Debug, Serialize)]
pub struct PageView {
    #[serde(flatten)]
    pub page: Page,
    pub url: String,
}

impl Page {
    pub fn into_view(self, config: &crate::config::Config) -> PageView {
        let url = config.page_url(&self.slug);
        PageView { page: self, url }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::connect_memory;

    fn new_page(title: &str, body: &str) -> NewPage {
        NewPage {
            title: title.into(),
            slug: None,
            body: body.into(),
            publish: false,
            in_nav: None,
            nav_order: None,
        }
    }

    #[test]
    fn slugify_titles() {
        assert_eq!(slugify("Hello, World!"), "hello-world");
        assert_eq!(slugify("  Über uns  "), "ueber-uns");
        assert_eq!(slugify("Rust 2026 -- news"), "rust-2026-news");
    }

    #[test]
    fn slug_validation() {
        assert!(validate_slug("about-us").is_ok());
        assert!(validate_slug("About").is_err());
        assert!(validate_slug("a--b").is_err());
        assert!(validate_slug("-a").is_err());
        assert!(validate_slug("api").is_err());
    }

    #[tokio::test]
    async fn create_update_and_restore() {
        let pool = connect_memory().await;
        let page = create(&pool, new_page("About Us", "first")).await.unwrap();
        assert_eq!(page.slug, "about-us");
        assert_eq!(page.status, Status::Draft);

        let page = update(
            &pool,
            "about-us",
            PageUpdate {
                body: Some("second".into()),
                slug: Some("about".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(page.slug, "about");
        assert!(get(&pool, "about-us").await.is_err());

        let revisions = list_revisions(&pool, "about").await.unwrap();
        assert_eq!(revisions.len(), 2);
        assert_eq!(revisions[0].revision, 2);

        let page = restore_revision(&pool, "about", 1).await.unwrap();
        assert_eq!(page.body, "first");
        assert_eq!(list_revisions(&pool, "about").await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn drafts_wait_for_publishing() {
        let pool = connect_memory().await;
        create(&pool, new_page("Home", "live")).await.unwrap();
        set_status(&pool, "home", Status::Published).await.unwrap();

        let page = save_draft(
            &pool,
            "home",
            DraftUpdate {
                body: Some("next".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(page.body, "live", "the site keeps the published body");
        assert_eq!(page.draft_body.as_deref(), Some("next"));
        assert_eq!(page.draft_title.as_deref(), Some("Home"));
        assert!(list(&pool, None).await.unwrap()[0].has_draft);
        assert_eq!(list_revisions(&pool, "home").await.unwrap().len(), 1);

        // A title alone keeps the drafted body.
        let page = save_draft(
            &pool,
            "home",
            DraftUpdate {
                title: Some("Start".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(page.draft_body.as_deref(), Some("next"));

        let page = set_status(&pool, "home", Status::Published).await.unwrap();
        assert_eq!((page.title.as_str(), page.body.as_str()), ("Start", "next"));
        assert_eq!(page.draft_body, None);
        assert!(!list(&pool, None).await.unwrap()[0].has_draft);
        assert_eq!(list_revisions(&pool, "home").await.unwrap().len(), 2);

        // Drafting what is already published leaves no draft behind.
        let page = save_draft(
            &pool,
            "home",
            DraftUpdate {
                body: Some("next".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(page.draft_body, None);

        save_draft(
            &pool,
            "home",
            DraftUpdate {
                body: Some("scrapped".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let page = discard_draft(&pool, "home").await.unwrap();
        assert_eq!((page.body.as_str(), page.draft_body), ("next", None));
    }

    #[tokio::test]
    async fn publishing_and_conflicts() {
        let pool = connect_memory().await;
        create(&pool, new_page("Home", "")).await.unwrap();
        assert!(matches!(
            create(&pool, new_page("Home", "")).await,
            Err(AppError::Conflict(_))
        ));

        let page = set_status(&pool, "home", Status::Published).await.unwrap();
        assert_eq!(page.status, Status::Published);
        assert!(page.published_at.is_some());

        assert_eq!(list(&pool, Some(Status::Published)).await.unwrap().len(), 1);
        assert_eq!(list(&pool, Some(Status::Draft)).await.unwrap().len(), 0);

        delete(&pool, "home").await.unwrap();
        assert!(matches!(
            delete(&pool, "home").await,
            Err(AppError::NotFound(_))
        ));
    }
}
