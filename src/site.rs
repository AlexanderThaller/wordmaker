use std::{
    collections::HashMap,
    fmt::Write,
    net::SocketAddr,
    sync::Arc,
    time::Instant,
};

use axum::{
    Router,
    extract::{
        ConnectInfo,
        MatchedPath,
        Path,
        Request,
        State,
    },
    http::{
        HeaderValue,
        Method,
        StatusCode,
        header,
    },
    middleware::{
        self,
        Next,
    },
    response::{
        Html,
        IntoResponse,
        Response,
    },
    routing::get,
};
use sha2::{
    Digest,
    Sha256,
};
use tower::{
    Layer,
    ServiceExt,
};
use tower_http::{
    services::{
        ServeDir,
        ServeFile,
    },
    set_header::SetResponseHeaderLayer,
};

use crate::{
    analytics::Hit,
    app::AppState,
    domain::{
        pages::{
            self,
            Page,
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
    theme::Stylesheet,
};

/// Theme files that may be served; everything else in a theme stays private.
const THEME_ASSET_EXTENSIONS: &[&str] = &[
    "woff2", "woff", "ttf", "otf", "css", "png", "jpg", "jpeg", "webp", "avif", "gif", "svg", "ico",
];

pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/", get(home))
        .route("/{slug}", get(show))
        .route("/sitemap.xml", get(sitemap))
        .route("/robots.txt", get(robots))
        .route("/_wm/style.css", get(stylesheet))
        .route("/_wm/theme/{theme}/{*path}", get(theme_asset))
        .route("/_wm/img/{width}/{filename}", get(scaled_image))
        .nest_service(
            "/media",
            middleware::from_fn(media_file).layer(
                // Stored names carry a random prefix and are never reused, so
                // a file at a given URL never changes.
                SetResponseHeaderLayer::if_not_present(
                    header::CACHE_CONTROL,
                    HeaderValue::from_static(IMMUTABLE),
                )
                .layer(ServeDir::new(&state.config.media_dir)),
            ),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            record_page_view,
        ))
}

/// Routes whose `GET`s count as page views.
const PAGE_ROUTES: &[&str] = &["/", "/{slug}"];

/// Hands page views to analytics after the response is ready; never waits.
async fn record_page_view(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let Some(analytics) = state.analytics.as_ref().filter(|_| {
        request.method() == Method::GET
            && request
                .extensions()
                .get::<MatchedPath>()
                .is_some_and(|path| PAGE_ROUTES.contains(&path.as_str()))
    }) else {
        return next.run(request).await;
    };

    let start = Instant::now();
    let time = chrono::Utc::now();
    let uri = request.uri().clone();
    let headers = request.headers().clone();
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(peer)| *peer);
    let response = next.run(request).await;

    let header = |name| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };
    analytics.record(Hit {
        time,
        ip: analytics.client_ip(crate::analytics::forwarded_for(&headers).as_deref(), peer),
        slug: uri.path().trim_start_matches('/').to_owned(),
        path: uri.path().to_owned(),
        query: uri.query().map(str::to_owned),
        status: response.status().as_u16(),
        duration: start.elapsed(),
        user_agent: header(header::USER_AGENT),
        referrer: header(header::REFERER),
        accept_language: header(header::ACCEPT_LANGUAGE),
    });
    response
}

const IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// Uploads and theme files are served from the site's origin, where a
/// script would run with access to everything the site can do. Opened on
/// their own, none of them may run one: the sandbox gives the document an
/// origin of its own and turns scripts off. Embedded as images, fonts or
/// stylesheets they are unaffected, since this policy only applies to the
/// file as a document.
const FILE_POLICY: &str = "sandbox; default-src 'none'; img-src 'self' data:; style-src \
                           'unsafe-inline'; frame-ancestors 'self'";

/// Locks down the response for a media or theme file at `path`; see
/// [`FILE_POLICY`]. Types a browser shows without running anything of the
/// file's are shown inline; everything else (HTML, XML, scripts, unknown
/// types) is a download. Decided by the name, as the file's content type
/// is, so that a `304` without a content type gets the same headers.
fn harden_file<B>(response: &mut Response<B>, path: &str) {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let essence = mime.essence_str();
    let headers = response.headers_mut();
    // Browsers refuse to show a PDF in a sandbox; their viewers run PDF
    // scripts apart from the page's origin anyway.
    if essence != "application/pdf" {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(FILE_POLICY),
        );
    }
    let inline = matches!(essence, "application/pdf" | "text/css" | "text/plain")
        || ["image/", "font/", "audio/", "video/"]
            .iter()
            .any(|kind| essence.starts_with(kind));
    if !inline {
        headers.insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment"),
        );
    }
}

async fn media_file(request: Request, next: Next) -> Response {
    let path = request.uri().path().to_owned();
    let mut response = next.run(request).await;
    harden_file(&mut response, &path);
    response
}

/// Everything a page needs besides its own content.
struct Frame {
    settings: Settings,
    nav: Vec<PageSummary>,
    stylesheet: Arc<Stylesheet>,
    footer_html: Option<Arc<str>>,
}

async fn frame(state: &AppState, settings: Settings) -> Result<Frame, AppError> {
    let nav = pages::navigation(&state.pool).await?;
    let sheet = state
        .themes
        .stylesheet(&state.config.themes_dir, &settings)
        .await;
    let footer_html = match &settings.footer_text {
        Some(text) => {
            let key = render_key(&[b"footer", text.as_bytes()]);
            Some(
                state
                    .renders
                    .get_or_render(key, async {
                        Ok(render::render_async(text.clone()).await?.html)
                    })
                    .await?,
            )
        }
        None => None,
    };
    Ok(Frame {
        settings,
        nav,
        stylesheet: sheet,
        footer_html,
    })
}

/// The page at `slug`, if it is published.
async fn published_page(state: &AppState, slug: &str) -> Result<Option<Page>, AppError> {
    match pages::get(&state.pool, slug).await {
        Ok(page) if page.status == Status::Published => Ok(Some(page)),
        Ok(_) | Err(AppError::NotFound(_)) => Ok(None),
        Err(err) => Err(err),
    }
}

async fn home(State(state): State<AppState>) -> Result<Response, SiteError> {
    let settings = settings::get(&state.pool).await?;
    // Not published (yet): `/` lists the pages that are.
    let home = match &settings.home_slug {
        Some(slug) => published_page(&state, slug).await?,
        None => None,
    };
    let frame = frame(&state, settings).await?;
    if let Some(home) = home {
        return render_page(&state, &frame, home, true).await;
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
    let canonical = state.config.page_url("");
    Ok(layout(&frame, &title, None, None, Some(&canonical), &body).into_response())
}

async fn show(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Response, SiteError> {
    // Looked up first, so that a miss costs one query and no frame.
    let page = published_page(&state, &slug)
        .await?
        .ok_or(SiteError::NotFound)?;
    let frame = frame(&state, settings::get(&state.pool).await?).await?;
    render_page(&state, &frame, page, false).await
}

async fn render_page(
    state: &AppState,
    frame: &Frame,
    page: Page,
    is_home: bool,
) -> Result<Response, SiteError> {
    let layout_widths = frame.stylesheet.layout;
    let key = render_key(&[
        b"page",
        &layout_widths.content.to_le_bytes(),
        &layout_widths.wide.to_le_bytes(),
        &state.images.generation().to_le_bytes(),
        page.body.as_bytes(),
    ]);
    let html = state
        .renders
        .get_or_render(key, content_html(state, frame, page.body))
        .await?;
    let site_title = &frame.settings.site_title;
    let title = if is_home {
        site_title.clone()
    } else {
        format!("{} – {site_title}", page.title)
    };
    // The home page is also reachable under its slug; both name `/`.
    let canonical = state.config.page_url(
        if is_home || frame.settings.home_slug.as_deref() == Some(page.slug.as_str()) {
            ""
        } else {
            &page.slug
        },
    );
    Ok(layout(
        frame,
        &title,
        Some(&page.title),
        Some(&page.slug),
        Some(&canonical),
        &html,
    )
    .into_response())
}

/// Most rendered bodies and footers kept, and their total size: enough for
/// every page of a sizeable site, while edits leave old versions behind.
const RENDER_CACHE_ENTRIES: usize = 512;
const RENDER_CACHE_BYTES: usize = 64 << 20;

/// Rendered page bodies and footer texts, keyed by a hash of everything
/// they are made from, so an edit, a new footer or a theme with other
/// widths simply misses rather than needing to be invalidated. The least
/// recently used entries make room for new ones.
#[derive(Debug, Default)]
pub struct RenderCache {
    entries: std::sync::Mutex<RenderEntries>,
}

#[derive(Debug, Default)]
struct RenderEntries {
    map: HashMap<[u8; 32], (Arc<str>, u64)>,
    bytes: usize,
    clock: u64,
}

/// A cache key over `parts`, each length-prefixed so that no two different
/// lists hash the same input.
fn render_key(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    hasher.finalize().into()
}

impl RenderCache {
    fn entries(&self) -> std::sync::MutexGuard<'_, RenderEntries> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The HTML stored under `key`, rendering it with `render` first when
    /// there is none. Rendered without holding the lock; two requests that
    /// miss together both render.
    async fn get_or_render(
        &self,
        key: [u8; 32],
        render: impl Future<Output = Result<String, AppError>>,
    ) -> Result<Arc<str>, AppError> {
        if let Some(html) = self.get(&key) {
            return Ok(html);
        }
        let html: Arc<str> = render.await?.into();
        self.insert(key, html.clone());
        Ok(html)
    }

    fn get(&self, key: &[u8; 32]) -> Option<Arc<str>> {
        let mut entries = self.entries();
        entries.clock += 1;
        let now = entries.clock;
        let (html, used) = entries.map.get_mut(key)?;
        *used = now;
        Some(html.clone())
    }

    fn insert(&self, key: [u8; 32], html: Arc<str>) {
        // One huge page should not push out the rest of the site.
        if html.len() > RENDER_CACHE_BYTES / 8 {
            return;
        }
        let mut entries = self.entries();
        while !entries.map.is_empty()
            && (entries.map.len() >= RENDER_CACHE_ENTRIES
                || entries.bytes + html.len() > RENDER_CACHE_BYTES)
        {
            let oldest = entries
                .map
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(key, _)| *key);
            if let Some((evicted, _)) = oldest.and_then(|key| entries.map.remove(&key)) {
                entries.bytes -= evicted.len();
            }
        }
        entries.clock += 1;
        let now = entries.clock;
        entries.bytes += html.len();
        if let Some((replaced, _)) = entries.map.insert(key, (html, now)) {
            entries.bytes -= replaced.len();
        }
    }
}

/// Every published page, for search engines. The home page is listed as
/// `/`; the page it shows is not listed again under its own slug.
async fn sitemap(State(state): State<AppState>) -> Result<Response, SiteError> {
    let settings = settings::get(&state.pool).await?;
    let published = pages::list(&state.pool, Some(Status::Published)).await?;
    // `/` lists the pages while the home page is not published.
    let home_slug = settings
        .home_slug
        .as_deref()
        .filter(|&slug| published.iter().any(|page| page.slug == slug));
    let home_updated = match home_slug {
        Some(slug) => published
            .iter()
            .find(|page| page.slug == slug)
            .map(|page| page.updated_at),
        None => published.iter().map(|page| page.updated_at).max(),
    };

    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<urlset \
         xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n",
    );
    let mut push = |url: &str, updated: Option<chrono::DateTime<chrono::Utc>>| {
        let _ = write!(xml, "<url><loc>{}</loc>", escape(url));
        if let Some(updated) = updated {
            let _ = write!(xml, "<lastmod>{}</lastmod>", updated.format("%Y-%m-%d"));
        }
        xml.push_str("</url>\n");
    };
    push(&state.config.page_url(""), home_updated);
    for page in published
        .iter()
        .filter(|page| Some(page.slug.as_str()) != home_slug)
    {
        push(&state.config.page_url(&page.slug), Some(page.updated_at));
    }
    xml.push_str("</urlset>\n");
    Ok((
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/xml; charset=utf-8"),
        )],
        xml,
    )
        .into_response())
}

/// Lets crawlers in everywhere but the authenticated endpoints, and points
/// them at the sitemap.
async fn robots(State(state): State<AppState>) -> Response {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        )],
        format!(
            "User-agent: *\nDisallow: /api/\nDisallow: /mcp\nDisallow: /oauth/\n\nSitemap: {}\n",
            state.config.page_url("sitemap.xml")
        ),
    )
        .into_response()
}

/// A page that need not exist or be published yet, in the site's frame and
/// theme: what `body` would look like under `title` if it were published.
/// `slug` marks the navigation entry as current, as it would on the site.
pub async fn preview(
    state: &AppState,
    title: &str,
    slug: Option<&str>,
    body: String,
) -> Result<Html<String>, AppError> {
    let frame = frame(state, settings::get(&state.pool).await?).await?;
    // Not cached: previews change with every keystroke.
    let html = content_html(state, &frame, body).await?;
    let site_title = &frame.settings.site_title;
    Ok(layout(
        &frame,
        &format!("{title} – {site_title}"),
        Some(title),
        slug,
        None,
        &html,
    ))
}

/// A page body as the site serves it: rendered, with images pointed at
/// their scaled copies.
async fn content_html(state: &AppState, frame: &Frame, body: String) -> Result<String, AppError> {
    let rendered = render::render_async(body).await?;
    let images = state.images.clone();
    let media_dir = state.config.media_dir.clone();
    let layout = frame.stylesheet.layout;
    tokio::task::spawn_blocking(move || images.rewrite(&rendered.html, &media_dir, layout))
        .await
        .map_err(|err| AppError::Internal(format!("image rewrite failed: {err}")))
}

/// The page, laid out like Twenty Twenty-Five's `page` template so a
/// `WordPress` theme's styles apply to it.
fn layout(
    frame: &Frame,
    title: &str,
    heading: Option<&str>,
    current: Option<&str>,
    canonical: Option<&str>,
    content: &str,
) -> Html<String> {
    let language = &frame.settings.site_language;
    let mut html = String::with_capacity(content.len() + 4096);
    let _ = write!(
        html,
        "<!DOCTYPE html>\n<html lang=\"{}\">\n<head>\n<meta charset=\"utf-8\">\n<meta \
         name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>{}</title>\n",
        escape(language),
        escape(title)
    );
    if let Some(canonical) = canonical {
        let _ = writeln!(
            html,
            "<link rel=\"canonical\" href=\"{}\">",
            escape(canonical)
        );
    }
    if let Some(tagline) = &frame.settings.site_tagline {
        let _ = writeln!(
            html,
            "<meta name=\"description\" content=\"{}\">",
            escape(tagline)
        );
    }
    // Inline rather than linked: a stylesheet request blocks the first
    // paint, and this one is small once compressed. It changes with the
    // page, so a theme switch shows at once. Only `</` can end the element,
    // in any case (`</STYLE`), and `<\/` means the same in a CSS string,
    // comment or URL. A lone `<` stays: media queries use it as an operator.
    let _ = write!(
        html,
        "<style>{}</style>\n</head>\n<body>\n<div class=\"wp-site-blocks\">\n",
        frame.stylesheet.css.replace("</", "<\\/")
    );

    write_header(&mut html, frame, current);

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

    write_footer(&mut html, frame);
    html.push_str("</div>\n</body>\n</html>\n");
    Html(html)
}

/// Header: site title on the left, navigation on the right.
fn write_header(html: &mut String, frame: &Frame, current: Option<&str>) {
    let site_title = escape(&frame.settings.site_title);
    let labels = Labels::for_language(&frame.settings.site_language);
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
        // Below 600px the list moves into an overlay behind a menu button, as
        // WordPress's navigation block does. The popover attribute gives the
        // overlay its open/close behaviour, Escape included, without script.
        let _ = write!(
            html,
            "<nav class=\"wp-block-navigation is-responsive is-layout-flex\" \
             aria-label=\"{menu}\">\n<button type=\"button\" \
             class=\"wp-block-navigation__responsive-container-open\" popovertarget=\"wm-menu\" \
             aria-label=\"{open}\">{MENU_ICON}</button>\n<div \
             class=\"wp-block-navigation__responsive-container\" id=\"wm-menu\" popover>\n<button \
             type=\"button\" class=\"wp-block-navigation__responsive-container-close\" \
             popovertarget=\"wm-menu\" popovertargetaction=\"hide\" \
             aria-label=\"{close}\">{CLOSE_ICON}</button>\n<ul \
             class=\"wp-block-navigation__container is-layout-flex\">\n",
            menu = labels.menu,
            open = labels.open_menu,
            close = labels.close_menu,
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
        html.push_str("</ul>\n</div>\n</nav>\n");
    }
    html.push_str("</div>\n</div>\n</div>\n</header>\n");
}

/// Footer: site title, tagline and the footer text, when there is any;
/// otherwise just the spacing, like a site whose footer was emptied.
fn write_footer(html: &mut String, frame: &Frame) {
    let site_title = escape(&frame.settings.site_title);
    html.push_str(
        "<footer class=\"wp-block-template-part\">\n<div class=\"wp-block-group \
         has-global-padding is-layout-constrained\" \
         style=\"padding-top:var(--wp--preset--spacing--60, \
         2.5rem);padding-bottom:var(--wp--preset--spacing--50, 2rem)\">\n",
    );
    let has_footer = frame.settings.site_tagline.is_some() || frame.footer_html.is_some();
    if has_footer {
        let _ = write!(
            html,
            "<div class=\"wp-block-group alignwide is-layout-flow\">\n<p \
             class=\"wp-block-site-title\"><a href=\"/\" rel=\"home\">{site_title}</a></p>\n"
        );
    }
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
    if has_footer {
        html.push_str("</div>\n");
    }
    html.push_str("</div>\n</footer>\n");
}

/// The few words the page frame itself needs.
struct Labels {
    menu: &'static str,
    open_menu: &'static str,
    close_menu: &'static str,
}

impl Labels {
    fn for_language(language: &str) -> Self {
        if language == "de" || language.starts_with("de-") {
            Self {
                menu: "Menü",
                open_menu: "Menü öffnen",
                close_menu: "Menü schließen",
            }
        } else {
            Self {
                menu: "Menu",
                open_menu: "Open menu",
                close_menu: "Close menu",
            }
        }
    }
}

/// The icons `WordPress`'s navigation block uses.
const MENU_ICON: &str = "<svg width=\"24\" height=\"24\" xmlns=\"http://www.w3.org/2000/svg\" \
                         viewBox=\"0 0 24 24\" aria-hidden=\"true\" focusable=\"false\"><path \
                         d=\"M4 7.5h16v1.5H4z M4 15h16v1.5H4z\"></path></svg>";
const CLOSE_ICON: &str = "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 24 24\" \
                          width=\"24\" height=\"24\" aria-hidden=\"true\" \
                          focusable=\"false\"><path d=\"m13.06 12 6.47-6.47-1.06-1.06L12 10.94 \
                          5.53 4.47 4.47 5.53 10.94 12l-6.47 6.47 1.06 1.06L12 13.06l6.47 6.47 \
                          1.06-1.06L13.06 12Z\"></path></svg>";

/// The stylesheet on its own, for whatever wants it besides the pages,
/// which carry it inline. Revalidated each time; the `ETag` makes that cheap.
async fn stylesheet(
    State(state): State<AppState>,
    request: Request,
) -> Result<Response, SiteError> {
    let settings = settings::get(&state.pool).await?;
    let sheet = state
        .themes
        .stylesheet(&state.config.themes_dir, &settings)
        .await;
    let etag = format!("\"{}\"", sheet.version);
    let cache = [
        (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        (
            header::ETAG,
            HeaderValue::from_str(&etag).map_err(|err| AppError::Internal(err.to_string()))?,
        ),
    ];
    let unchanged = request
        .headers()
        .get(header::IF_NONE_MATCH)
        .is_some_and(|value| value.as_bytes() == etag.as_bytes());
    if unchanged {
        return Ok((StatusCode::NOT_MODIFIED, cache).into_response());
    }
    Ok((
        cache,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/css; charset=utf-8"),
        )],
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
        .is_some_and(|ext| THEME_ASSET_EXTENSIONS.contains(&ext.to_lowercase().as_str()));
    // `{slug}@{stamp}`, as the stylesheet links them, names one version of
    // the theme's files, which never changes; a bare slug is whatever is
    // there now. Any other stamp is not cached at all.
    let (theme, stamp) = match theme.split_once('@') {
        Some((slug, stamp)) => (slug, Some(stamp)),
        None => (theme.as_str(), None),
    };
    if crate::theme::validate_slug(theme).is_err() || !safe_path || !allowed {
        return StatusCode::NOT_FOUND.into_response();
    }
    let cache_control = match stamp {
        Some(stamp) if crate::theme::is_current_stamp(&state.config.themes_dir, theme, stamp) => {
            IMMUTABLE
        }
        Some(_) => "no-cache",
        None => "public, max-age=86400",
    };
    let file = state.config.themes_dir.join(theme).join(&path);
    match ServeFile::new(file).oneshot(request).await {
        Ok(mut response) => {
            harden_file(&mut response, &path);
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static(cache_control),
            );
            response.into_response()
        }
        Err(never) => match never {},
    }
}

async fn scaled_image(
    State(state): State<AppState>,
    Path((width, filename)): Path<(u32, String)>,
    request: Request,
) -> Response {
    let Some(file) = state
        .images
        .variant(&state.config.media_dir, width, &filename)
        .await
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match ServeFile::new(file).oneshot(request).await {
        Ok(mut response) => {
            harden_file(&mut response, &filename);
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static(IMMUTABLE));
            response.into_response()
        }
        Err(never) => match never {},
    }
}

pub fn escape(text: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: usize) -> [u8; 32] {
        render_key(&[&n.to_le_bytes()])
    }

    #[test]
    fn render_cache_keeps_recent_entries_within_bounds() {
        let cache = RenderCache::default();
        cache.insert(key(0), "first".into());
        for n in 1..RENDER_CACHE_ENTRIES * 2 {
            // Kept in use, so it is never the oldest.
            assert!(cache.get(&key(0)).is_some(), "{n}");
            cache.insert(key(n), "page".into());
        }
        assert_eq!(cache.entries().map.len(), RENDER_CACHE_ENTRIES);
        assert!(cache.get(&key(1)).is_none());
        assert!(cache.get(&key(RENDER_CACHE_ENTRIES * 2 - 1)).is_some());

        let big: Arc<str> = "x".repeat(RENDER_CACHE_BYTES / 8).into();
        for n in 0..10 {
            cache.insert(key(10_000 + n), big.clone());
        }
        let entries = cache.entries();
        assert!(entries.bytes <= RENDER_CACHE_BYTES, "{}", entries.bytes);
        assert_eq!(
            entries.bytes,
            entries
                .map
                .values()
                .map(|(html, _)| html.len())
                .sum::<usize>()
        );
    }

    #[test]
    fn render_keys_separate_their_parts() {
        assert_ne!(render_key(&[b"ab", b"c"]), render_key(&[b"a", b"bc"]));
    }
}
