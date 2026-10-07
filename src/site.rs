use std::fmt::Write;

use axum::{
    Router,
    extract::{
        Path,
        Request,
        State,
    },
    http::{
        HeaderValue,
        StatusCode,
        header,
    },
    response::{
        Html,
        IntoResponse,
        Response,
    },
    routing::get,
};
use tower::ServiceExt;
use tower_http::services::{
    ServeDir,
    ServeFile,
};

use crate::{
    app::AppState,
    domain::{
        pages::{
            self,
            PageSummary,
            Status,
        },
        settings::{
            self,
            Settings,
        },
    },
    error::AppError,
    render,
};

/// Theme files that may be served; everything else in a theme stays private.
const THEME_ASSET_EXTENSIONS: &[&str] = &[
    "woff2", "woff", "ttf", "otf", "css", "png", "jpg", "jpeg", "webp", "avif", "gif", "svg", "ico",
];

pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/", get(home))
        .route("/{slug}", get(show))
        .route("/_wm/style.css", get(stylesheet))
        .route("/_wm/theme/{theme}/{*path}", get(theme_asset))
        .nest_service("/media", ServeDir::new(&state.config.media_dir))
}

/// Everything a page needs besides its own content.
struct Frame {
    settings: Settings,
    nav: Vec<PageSummary>,
    stylesheet_version: String,
    footer_html: Option<String>,
}

async fn frame(state: &AppState) -> Result<Frame, AppError> {
    let settings = settings::get(&state.pool).await?;
    let nav = pages::navigation(&state.pool).await?;
    let sheet = state
        .themes
        .stylesheet(&state.config.themes_dir, &settings)
        .await;
    let footer_html = match &settings.footer_text {
        Some(text) => Some(render::render_async(text.clone()).await?.html),
        None => None,
    };
    Ok(Frame {
        settings,
        nav,
        stylesheet_version: sheet.version.clone(),
        footer_html,
    })
}

async fn home(State(state): State<AppState>) -> Result<Response, SiteError> {
    let frame = frame(&state).await?;
    if let Some(home) = frame.settings.home_slug.clone() {
        return render_page(&state, &frame, &home, true).await;
    }

    let published = pages::list(&state.pool, Some(Status::Published)).await?;
    let mut body = String::from("<ul class=\"wm-page-list\">\n");
    for page in &published {
        let _ = writeln!(
            body,
            "<li class=\"has-large-font-size\"><a href=\"/{}\">{}</a></li>",
            page.slug,
            escape(&page.title)
        );
    }
    body.push_str("</ul>\n");
    let title = frame.settings.site_title.clone();
    Ok(layout(&frame, &title, None, None, &body).into_response())
}

async fn show(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Response, SiteError> {
    let frame = frame(&state).await?;
    render_page(&state, &frame, &slug, false).await
}

async fn render_page(
    state: &AppState,
    frame: &Frame,
    slug: &str,
    is_home: bool,
) -> Result<Response, SiteError> {
    let page = match pages::get(&state.pool, slug).await {
        Ok(page) if page.status == Status::Published => page,
        Ok(_) | Err(AppError::NotFound(_)) => return Err(SiteError::NotFound),
        Err(err) => return Err(err.into()),
    };
    let rendered = render::render_async(page.body).await?;
    let site_title = &frame.settings.site_title;
    let title = if is_home {
        site_title.clone()
    } else {
        format!("{} – {site_title}", page.title)
    };
    let heading = (!is_home).then_some(page.title.as_str());
    Ok(layout(frame, &title, heading, Some(&page.slug), &rendered.html).into_response())
}

/// The page, laid out like Twenty Twenty-Five's `page` template so a
/// WordPress theme's styles apply to it.
fn layout(
    frame: &Frame,
    title: &str,
    heading: Option<&str>,
    current: Option<&str>,
    content: &str,
) -> Html<String> {
    let site_title = escape(&frame.settings.site_title);
    let mut html = String::with_capacity(content.len() + 4096);
    let _ = write!(
        html,
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<meta \
         name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>{}</title>\n",
        escape(title)
    );
    if let Some(tagline) = &frame.settings.site_tagline {
        let _ = writeln!(
            html,
            "<meta name=\"description\" content=\"{}\">",
            escape(tagline)
        );
    }
    let _ = write!(
        html,
        "<link rel=\"stylesheet\" href=\"/_wm/style.css?v={}\">\n</head>\n<body>\n<div \
         class=\"wp-site-blocks\">\n",
        frame.stylesheet_version
    );

    // Header: site title on the left, navigation on the right.
    let _ = write!(
        html,
        "<header class=\"wp-block-template-part\">\n<div class=\"wp-block-group alignfull \
         is-layout-flow\">\n<div class=\"wp-block-group has-global-padding \
         is-layout-constrained\">\n<div class=\"wp-block-group alignwide is-layout-flex \
         wm-header-row\" style=\"padding-top:var(--wp--preset--spacing--30, \
         1rem);padding-bottom:var(--wp--preset--spacing--30, 1rem)\">\n<p \
         class=\"wp-block-site-title\"><a href=\"/\" rel=\"home\">{site_title}</a></p>\n"
    );
    if !frame.nav.is_empty() {
        html.push_str(
            "<nav class=\"wp-block-navigation is-layout-flex\" aria-label=\"Main\">\n<ul \
             class=\"wp-block-navigation__container is-layout-flex\">\n",
        );
        for page in &frame.nav {
            let is_current = current == Some(page.slug.as_str());
            let _ = writeln!(
                html,
                "<li class=\"wp-block-navigation-item{}\"><a \
                 class=\"wp-block-navigation-item__content\" href=\"/{}\"{}><span \
                 class=\"wp-block-navigation-item__label\">{}</span></a></li>",
                if is_current { " current-menu-item" } else { "" },
                page.slug,
                if is_current {
                    " aria-current=\"page\""
                } else {
                    ""
                },
                escape(&page.title)
            );
        }
        html.push_str("</ul>\n</nav>\n");
    }
    html.push_str("</div>\n</div>\n</div>\n</header>\n");

    // Main: title and content, constrained to the theme's content width.
    html.push_str(
        "<main class=\"wp-block-group has-global-padding is-layout-constrained\" \
         style=\"margin-top:var(--wp--preset--spacing--60, 2.5rem)\">\n<div \
         class=\"wp-block-group alignfull has-global-padding is-layout-constrained\" \
         style=\"padding-top:var(--wp--preset--spacing--60, \
         2.5rem);padding-bottom:var(--wp--preset--spacing--60, 2.5rem)\">\n",
    );
    if let Some(heading) = heading {
        let _ = writeln!(
            html,
            "<h1 class=\"wp-block-post-title\">{}</h1>",
            escape(heading)
        );
    }
    html.push_str(
        "<div class=\"entry-content alignfull wp-block-post-content has-global-padding \
         is-layout-constrained wm-adoc\">\n",
    );
    html.push_str(content);
    html.push_str("\n</div>\n</div>\n</main>\n");

    // Footer: site title, tagline and the footer text.
    let _ = write!(
        html,
        "<footer class=\"wp-block-template-part\">\n<div class=\"wp-block-group \
         has-global-padding is-layout-constrained\" \
         style=\"padding-top:var(--wp--preset--spacing--60, \
         2.5rem);padding-bottom:var(--wp--preset--spacing--50, 2rem)\">\n<div \
         class=\"wp-block-group alignwide is-layout-flow\">\n<p class=\"wp-block-site-title\"><a \
         href=\"/\" rel=\"home\">{site_title}</a></p>\n"
    );
    if let Some(tagline) = &frame.settings.site_tagline {
        let _ = writeln!(
            html,
            "<p class=\"wp-block-site-tagline\">{}</p>",
            escape(tagline)
        );
    }
    if let Some(footer) = &frame.footer_html {
        let _ = writeln!(
            html,
            "<div class=\"wm-footer-text wm-adoc has-small-font-size\">{footer}</div>"
        );
    }
    html.push_str("</div>\n</div>\n</footer>\n</div>\n</body>\n</html>\n");
    Html(html)
}

async fn stylesheet(State(state): State<AppState>) -> Result<Response, SiteError> {
    let settings = settings::get(&state.pool).await?;
    let sheet = state
        .themes
        .stylesheet(&state.config.themes_dir, &settings)
        .await;
    Ok((
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/css; charset=utf-8"),
            ),
            // The URL carries the version, so it can be cached for a long time.
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=31536000"),
            ),
        ],
        sheet.css.clone(),
    )
        .into_response())
}

async fn theme_asset(
    State(state): State<AppState>,
    Path((theme, path)): Path<(String, String)>,
    request: Request,
) -> Response {
    let safe_path = std::path::Path::new(&path)
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)));
    let allowed = std::path::Path::new(&path)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| THEME_ASSET_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()));
    if crate::theme::validate_slug(&theme).is_err() || !safe_path || !allowed {
        return StatusCode::NOT_FOUND.into_response();
    }
    let file = state.config.themes_dir.join(&theme).join(&path);
    match ServeFile::new(file).oneshot(request).await {
        Ok(mut response) => {
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=86400"),
            );
            response.into_response()
        }
        Err(never) => match never {},
    }
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
