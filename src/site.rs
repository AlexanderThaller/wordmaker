use std::{fmt::Write, sync::LazyLock};

use axum::{
    Router,
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
};
use tower_http::services::ServeDir;

use crate::{
    app::AppState,
    domain::{
        pages::{self, PageSummary, Status},
        settings,
    },
    error::AppError,
    render,
};

const SITE_CSS: &str = r#"
.site-header { max-width: 62.5em; margin: 0 auto; padding: 1em 1em 0;
  display: flex; flex-wrap: wrap; gap: .5em 1.5em; align-items: baseline;
  border-bottom: 1px solid #dddddf; padding-bottom: .75em; }
.site-header .site-title { font-size: 1.4em; font-weight: 600; text-decoration: none; color: inherit; }
.site-header nav { display: flex; flex-wrap: wrap; gap: .25em 1em; }
.site-header nav a { text-decoration: none; }
.site-header nav a[aria-current] { font-weight: 600; }
.page-list { list-style: none; padding: 0; }
.page-list li { margin: .5em 0; font-size: 1.2em; }
"#;

static STYLESHEET: LazyLock<String> =
    LazyLock::new(|| adocers_html::default_stylesheet() + SITE_CSS);

pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/", get(home))
        .route("/{slug}", get(show))
        .nest_service("/media", ServeDir::new(&state.config.media_dir))
}

async fn home(State(state): State<AppState>) -> Result<Response, SiteError> {
    let settings = settings::get(&state.pool).await?;
    let published = pages::list(&state.pool, Some(Status::Published)).await?;

    if let Some(home) = &settings.home_slug {
        return render_page(&state, &settings.site_title, &published, home, true).await;
    }

    let mut body = String::from("<ul class=\"page-list\">\n");
    for page in &published {
        let _ = writeln!(
            body,
            "<li><a href=\"/{}\">{}</a></li>",
            page.slug,
            escape(&page.title)
        );
    }
    body.push_str("</ul>\n");
    Ok(layout(
        &settings.site_title,
        &settings.site_title,
        &published,
        None,
        &body,
    )
    .into_response())
}

async fn show(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Response, SiteError> {
    let settings = settings::get(&state.pool).await?;
    let published = pages::list(&state.pool, Some(Status::Published)).await?;
    render_page(&state, &settings.site_title, &published, &slug, false).await
}

async fn render_page(
    state: &AppState,
    site_title: &str,
    published: &[PageSummary],
    slug: &str,
    is_home: bool,
) -> Result<Response, SiteError> {
    let page = match pages::get(&state.pool, slug).await {
        Ok(page) if page.status == Status::Published => page,
        Ok(_) | Err(AppError::NotFound(_)) => return Err(SiteError::NotFound),
        Err(err) => return Err(err.into()),
    };
    let rendered = render::render_async(page.body).await?;
    let mut body = String::new();
    if !is_home {
        let _ = writeln!(
            body,
            "<div id=\"header\"><h1>{}</h1></div>",
            escape(&page.title)
        );
    }
    body.push_str(&rendered.html);
    let title = if is_home {
        site_title.to_owned()
    } else {
        format!("{} – {site_title}", page.title)
    };
    Ok(layout(&title, site_title, published, Some(&page.slug), &body).into_response())
}

fn layout(
    title: &str,
    site_title: &str,
    published: &[PageSummary],
    current: Option<&str>,
    content: &str,
) -> Html<String> {
    let mut body = String::new();
    let _ = write!(
        body,
        "<header class=\"site-header\"><a class=\"site-title\" href=\"/\">{}</a><nav>",
        escape(site_title)
    );
    for page in published {
        let aria = if current == Some(page.slug.as_str()) {
            " aria-current=\"page\""
        } else {
            ""
        };
        let _ = write!(
            body,
            "<a href=\"/{}\"{aria}>{}</a>",
            page.slug,
            escape(&page.title)
        );
    }
    body.push_str("</nav></header>\n<div id=\"content\">\n");
    body.push_str(content);
    body.push_str("\n</div>\n");

    Html(adocers_html::page(
        &adocers_html::Page {
            lang: "en",
            title,
            body_classes: "article",
            stylesheet: Some(&STYLESHEET),
            ..Default::default()
        },
        &body,
    ))
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

enum SiteError {
    NotFound,
    App(AppError),
}

impl From<AppError> for SiteError {
    fn from(err: AppError) -> Self {
        Self::App(err)
    }
}

impl IntoResponse for SiteError {
    fn into_response(self) -> Response {
        match self {
            Self::NotFound => (
                StatusCode::NOT_FOUND,
                Html("<!DOCTYPE html><title>Not found</title><h1>Page not found</h1>"),
            )
                .into_response(),
            Self::App(err) => {
                tracing::error!(error = %err, "failed to render page");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Html("<h1>Internal error</h1>"),
                )
                    .into_response()
            }
        }
    }
}
