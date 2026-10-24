//! `WordPress` block themes: installing them, listing their style variations,
//! and turning the active one into the site's stylesheet.

pub mod css;
mod site_css;

use std::{
    collections::hash_map::DefaultHasher,
    hash::{
        Hash,
        Hasher,
    },
    io::Read,
    net::{
        IpAddr,
        SocketAddr,
    },
    path::{
        Component,
        Path,
        PathBuf,
    },
    sync::{
        Arc,
        Mutex,
        PoisonError,
    },
    time::{
        Duration,
        SystemTime,
        UNIX_EPOCH,
    },
};

use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;
use sqlx::SqlitePool;
use url::{
    Host,
    Url,
};

use crate::{
    domain::settings::{
        self,
        Settings,
    },
    error::{
        AppError,
        AppResult,
    },
    images::Layout,
};

const DEFAULT_THEME: &str = include_str!("default.json");
const MAX_DOWNLOAD_BYTES: usize = 100 * 1024 * 1024;
const MAX_EXTRACTED_BYTES: u64 = 250 * 1024 * 1024;
const MAX_FILES: usize = 10_000;
const MAX_REDIRECTS: usize = 5;
const DNS_TIMEOUT: Duration = Duration::from_secs(10);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// Held while themes are installed, deleted, written to or activated, so
/// that, say, a delete cannot remove a theme that is being activated.
static MUTATIONS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ThemeInfo {
    /// Directory name, used to activate the theme.
    pub slug: String,
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    /// Style variations that can be passed to `activate`.
    pub variations: Vec<VariationInfo>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct VariationInfo {
    /// Identifier to pass when activating, e.g. `01-evening` or
    /// `colors/03-dusk`.
    pub id: String,
    pub title: String,
    /// `style` changes everything, `colors` and `typography` only that part;
    /// several can be combined.
    pub kind: String,
}

// ---------------------------------------------------------------------------
// Listing and activating
// ---------------------------------------------------------------------------

pub async fn list(themes_dir: &Path) -> AppResult<Vec<ThemeInfo>> {
    let dir = themes_dir.to_owned();
    blocking(move || {
        let mut themes = Vec::new();
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(themes),
            Err(err) => return Err(err.into()),
        };
        for entry in entries {
            let entry = entry?;
            let Some(slug) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if validate_slug(&slug).is_ok() && entry.path().join("theme.json").is_file() {
                themes.push(read_info(&dir, &slug)?);
            }
        }
        themes.sort_by(|a, b| a.slug.cmp(&b.slug));
        Ok(themes)
    })
    .await
}

pub async fn info(themes_dir: &Path, slug: &str) -> AppResult<ThemeInfo> {
    validate_slug(slug)?;
    let dir = themes_dir.to_owned();
    let slug = slug.to_owned();
    blocking(move || read_info(&dir, &slug)).await
}

/// Make `theme` the active theme with the given style variations, or switch
/// back to the built-in look with `None`.
pub async fn activate(
    pool: &SqlitePool,
    themes_dir: &Path,
    theme: Option<&str>,
    variations: Vec<String>,
) -> AppResult<Settings> {
    let Some(theme) = theme.filter(|t| !t.is_empty()) else {
        return settings::set_theme(pool, None, &[]).await;
    };
    let _lock = MUTATIONS.lock().await;
    let info = info(themes_dir, theme).await?;
    for id in &variations {
        if !info.variations.iter().any(|v| &v.id == id) {
            return Err(AppError::BadRequest(format!(
                "theme '{theme}' has no style variation '{id}'; available: {}",
                info.variations
                    .iter()
                    .map(|v| v.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    settings::set_theme(pool, Some(theme), &variations).await
}

pub async fn delete(pool: &SqlitePool, themes_dir: &Path, slug: &str) -> AppResult<()> {
    validate_slug(slug)?;
    let _lock = MUTATIONS.lock().await;
    if settings::get(pool).await?.theme.as_deref() == Some(slug) {
        return Err(AppError::Conflict(format!(
            "theme '{slug}' is active; activate another theme first"
        )));
    }
    let path = themes_dir.join(slug);
    if !path.join("theme.json").is_file() {
        return Err(AppError::NotFound(format!("theme '{slug}'")));
    }
    tokio::fs::remove_dir_all(path).await?;
    Ok(())
}

fn read_info(themes_dir: &Path, slug: &str) -> AppResult<ThemeInfo> {
    let dir = themes_dir.join(slug);
    if !dir.join("theme.json").is_file() {
        return Err(AppError::NotFound(format!("theme '{slug}'")));
    }
    let header = std::fs::read_to_string(dir.join("style.css")).unwrap_or_default();
    let field = |name: &str| style_header(&header, name);
    let mut variations = Vec::new();
    collect_variations(&dir.join("styles"), "", &mut variations)?;
    variations.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(ThemeInfo {
        slug: slug.to_owned(),
        name: field("Theme Name").unwrap_or_else(|| slug.to_owned()),
        version: field("Version"),
        description: field("Description"),
        variations,
    })
}

/// Global style variations: every JSON file below `styles/` that is not a
/// block style (those carry `blockTypes`).
fn collect_variations(dir: &Path, prefix: &str, out: &mut Vec<VariationInfo>) -> AppResult<()> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if path.is_dir() && prefix.is_empty() {
            collect_variations(&path, &name, out)?;
            continue;
        }
        let Some(stem) = name.strip_suffix(".json") else {
            continue;
        };
        let Ok(json) = read_json(&path) else { continue };
        if json.get("blockTypes").is_some() {
            continue;
        }
        let id = if prefix.is_empty() {
            stem.to_owned()
        } else {
            format!("{prefix}/{stem}")
        };
        out.push(VariationInfo {
            title: json
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or(stem)
                .to_owned(),
            kind: if prefix.is_empty() {
                "style".into()
            } else {
                prefix.to_owned()
            },
            id,
        });
    }
    Ok(())
}

fn style_header(css: &str, name: &str) -> Option<String> {
    css.lines().take(40).find_map(|line| {
        let line = line.trim().trim_start_matches('*').trim();
        let value = line.strip_prefix(name)?.trim_start().strip_prefix(':')?;
        Some(value.trim().to_owned()).filter(|v| !v.is_empty())
    })
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ThemeDetails {
    #[serde(flatten)]
    pub info: ThemeInfo,
    /// Every file in the theme folder, sorted by path.
    pub files: Vec<ThemeFileEntry>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ThemeFileEntry {
    /// Path inside the theme folder, e.g. `style.css` or
    /// `styles/01-night.json`.
    pub path: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ThemeFile {
    pub path: String,
    pub content: String,
}

/// A theme's info and the files it is made of.
pub async fn details(themes_dir: &Path, slug: &str) -> AppResult<ThemeDetails> {
    validate_slug(slug)?;
    let dir = themes_dir.to_owned();
    let slug = slug.to_owned();
    blocking(move || {
        let info = read_info(&dir, &slug)?;
        let root = dir.join(&slug);
        let mut files = Vec::new();
        collect_files(&root, &root, &mut files)?;
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(ThemeDetails { info, files })
    })
    .await
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<ThemeFileEntry>) -> AppResult<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect_files(root, &path, out)?;
        } else if kind.is_file() {
            let relative = path.strip_prefix(root).unwrap_or(&path);
            out.push(ThemeFileEntry {
                path: relative
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/"),
                size: entry.metadata()?.len(),
            });
        }
    }
    Ok(())
}

/// One text file of a theme, such as `theme.json` or `style.css`.
pub async fn read_file(themes_dir: &Path, slug: &str, path: &str) -> AppResult<ThemeFile> {
    validate_slug(slug)?;
    let dir = existing_theme_dir(themes_dir, slug)?;
    let file = theme_file_path(&dir, path)?;
    let path = path.to_owned();
    blocking(move || {
        let bytes = match std::fs::read(&file) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(AppError::NotFound(format!("theme file '{path}'")));
            }
            Err(err) => return Err(err.into()),
        };
        let content = String::from_utf8(bytes)
            .map_err(|_| AppError::BadRequest(format!("'{path}' is not a text file")))?;
        Ok(ThemeFile { path, content })
    })
    .await
}

/// Create or replace a text file in an installed theme. JSON files must
/// parse, and `theme.json` must stay an object. The site picks the change up
/// right away.
pub async fn write_file(
    themes_dir: &Path,
    slug: &str,
    path: &str,
    content: String,
) -> AppResult<ThemeFileEntry> {
    validate_slug(slug)?;
    let lock = MUTATIONS.lock().await;
    let dir = existing_theme_dir(themes_dir, slug)?;
    let file = theme_file_path(&dir, path)?;
    let extension = file
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_lowercase);
    if extension.as_deref() == Some("php") {
        return Err(AppError::BadRequest(
            "PHP files are not used by WordMaker".into(),
        ));
    }
    if extension.as_deref() == Some("json") {
        let json: Value = serde_json::from_str(&content)
            .map_err(|err| AppError::BadRequest(format!("{path} is not valid JSON: {err}")))?;
        if is_theme_json(path) && !json.is_object() {
            return Err(AppError::BadRequest("theme.json must be an object".into()));
        }
    }
    let path = path.to_owned();
    blocking(move || {
        let _lock = lock;
        if file.is_dir() {
            return Err(AppError::BadRequest(format!("'{path}' is a folder")));
        }
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let staging = dir.join(format!(".write-{}", uuid::Uuid::new_v4().simple()));
        std::fs::write(&staging, content.as_bytes())?;
        if let Err(err) = std::fs::rename(&staging, &file) {
            let _ = std::fs::remove_file(&staging);
            return Err(err.into());
        }
        // The stylesheet cache and the asset stamp follow theme.json's
        // modification time, so touch it whichever file changed.
        std::fs::File::options()
            .write(true)
            .open(dir.join("theme.json"))?
            .set_modified(SystemTime::now())?;
        Ok(ThemeFileEntry {
            path,
            size: content.len() as u64,
        })
    })
    .await
}

/// Whether `path` names the theme's own `theme.json`, also as `theme.json/`
/// or in another case, which is the same file on macOS and Windows.
fn is_theme_json(path: &str) -> bool {
    let mut components = Path::new(path).components();
    matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(name)), None)
            if name.to_str().is_some_and(|name| name.to_lowercase() == "theme.json")
    )
}

fn existing_theme_dir(themes_dir: &Path, slug: &str) -> AppResult<PathBuf> {
    let dir = themes_dir.join(slug);
    if !dir.join("theme.json").is_file() {
        return Err(AppError::NotFound(format!("theme '{slug}'")));
    }
    Ok(dir)
}

fn theme_file_path(theme_dir: &Path, path: &str) -> AppResult<PathBuf> {
    let relative = Path::new(path);
    let safe = !path.is_empty()
        && relative.components().all(|c| match c {
            Component::Normal(part) => !part.to_string_lossy().starts_with('.'),
            _ => false,
        });
    if !safe {
        return Err(AppError::BadRequest(format!(
            "invalid theme file path '{path}'"
        )));
    }
    Ok(theme_dir.join(relative))
}

// ---------------------------------------------------------------------------
// Installing
// ---------------------------------------------------------------------------

/// The download URL of a theme in the wordpress.org directory.
pub fn wordpress_org_url(slug: &str, version: Option<&str>) -> AppResult<String> {
    validate_slug(slug)?;
    Ok(match version {
        Some(version)
            if version
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.') =>
        {
            format!("https://downloads.wordpress.org/theme/{slug}.{version}.zip")
        }
        Some(version) => return Err(AppError::BadRequest(format!("invalid version '{version}'"))),
        None => format!("https://downloads.wordpress.org/theme/{slug}.zip"),
    })
}

pub async fn install_from_url(
    themes_dir: &Path,
    url: &str,
    slug: Option<&str>,
) -> AppResult<ThemeInfo> {
    let bytes = download(url).await?;
    install_zip(themes_dir, bytes, slug).await
}

/// Download a theme zip from the public internet, following up to
/// `MAX_REDIRECTS` redirects. Each URL on the way is checked by
/// `connect`. Failures all read the same, so the answers say nothing about
/// what is listening where.
async fn download(url: &str) -> AppResult<Vec<u8>> {
    let failed = || AppError::BadRequest(format!("downloading a theme from {url} failed"));
    let mut current =
        Url::parse(url).map_err(|_| AppError::BadRequest(format!("invalid theme URL '{url}'")))?;
    let mut redirects = 0;
    let mut response = loop {
        let response = match connect(&current).await {
            Ok(response) => response,
            Err(Hop::Refused(message)) => return Err(AppError::BadRequest(message)),
            Err(Hop::Failed(err)) => {
                tracing::warn!(url = %current, error = %err, "theme download failed");
                return Err(failed());
            }
        };
        let status = response.status();
        if status.is_redirection() && redirects < MAX_REDIRECTS {
            let next = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|location| location.to_str().ok())
                .and_then(|location| current.join(location).ok());
            let Some(next) = next else {
                tracing::warn!(url = %current, %status, "theme download redirects nowhere");
                return Err(failed());
            };
            redirects += 1;
            current = next;
            continue;
        }
        if !status.is_success() {
            tracing::warn!(url = %current, %status, "theme download failed");
            return Err(failed());
        }
        break response;
    };
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|err| {
        tracing::warn!(url = %current, error = %err, "theme download failed");
        failed()
    })? {
        bytes.extend_from_slice(&chunk);
        if bytes.len() > MAX_DOWNLOAD_BYTES {
            return Err(AppError::BadRequest(format!(
                "theme download exceeds {MAX_DOWNLOAD_BYTES} bytes"
            )));
        }
    }
    Ok(bytes)
}

/// Why one request of a download got no response.
enum Hop {
    /// The URL is not one to fetch; the message says why.
    Refused(String),
    /// Anything else, which is only logged.
    Failed(String),
}

/// Request `url` without following redirects. The host is resolved once,
/// refused unless every address is on the public internet, and connected to
/// at exactly the address that was checked, without a proxy, so a theme URL
/// (or a redirect, or a DNS answer that changes) cannot reach the server
/// itself or its local network.
async fn connect(url: &Url) -> Result<reqwest::Response, Hop> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Hop::Refused(format!(
            "theme URL must be http or https: {url}"
        )));
    }
    let not_public = || Hop::Refused(format!("{url} is not on the public internet"));
    let port = url.port_or_known_default().unwrap_or(443);
    let address = match url.host() {
        Some(Host::Ipv4(ip)) => SocketAddr::new(IpAddr::V4(ip), port),
        Some(Host::Ipv6(ip)) => SocketAddr::new(IpAddr::V6(ip), port),
        Some(Host::Domain(domain)) => {
            let addresses: Vec<SocketAddr> =
                tokio::time::timeout(DNS_TIMEOUT, tokio::net::lookup_host((domain, port)))
                    .await
                    .map_err(|_| Hop::Failed(format!("resolving {domain} timed out")))?
                    .map_err(|err| Hop::Failed(format!("could not resolve {domain}: {err}")))?
                    .collect();
            if addresses.iter().any(|a| !crate::oauth::is_public(a.ip())) {
                return Err(not_public());
            }
            *addresses
                .first()
                .ok_or_else(|| Hop::Failed(format!("{domain} has no addresses")))?
        }
        None => return Err(Hop::Refused(format!("theme URL has no host: {url}"))),
    };
    if !crate::oauth::is_public(address.ip()) {
        return Err(not_public());
    }
    let mut client = reqwest::Client::builder()
        .user_agent(concat!("wordmaker/", env!("CARGO_PKG_VERSION")))
        .timeout(DOWNLOAD_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy();
    if let Some(Host::Domain(domain)) = url.host() {
        client = client.resolve(domain, address);
    }
    let client = client.build().map_err(|err| Hop::Failed(err.to_string()))?;
    client
        .get(url.clone())
        .send()
        .await
        .map_err(|err| Hop::Failed(err.to_string()))
}

/// Unpack a theme zip into `themes_dir/<slug>`, replacing an existing theme
/// of the same name. PHP files are left out: nothing here would run them.
pub async fn install_zip(
    themes_dir: &Path,
    bytes: Vec<u8>,
    slug: Option<&str>,
) -> AppResult<ThemeInfo> {
    if let Some(slug) = slug {
        validate_slug(slug)?;
    }
    let themes_dir = themes_dir.to_owned();
    let slug = slug.map(str::to_owned);
    let lock = MUTATIONS.lock().await;
    blocking(move || {
        // Kept until the files are in place, even if the request goes away.
        let _lock = lock;
        let slug = extract(&themes_dir, &bytes, slug)?;
        read_info(&themes_dir, &slug)
    })
    .await
}

fn extract(themes_dir: &Path, bytes: &[u8], slug: Option<String>) -> AppResult<String> {
    let bad = |message: String| AppError::BadRequest(message);
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|err| bad(format!("not a zip file: {err}")))?;
    // Before anything walks the entries: the count below only covers the
    // files that get extracted.
    if archive.len() > MAX_FILES {
        return Err(bad(format!("theme zip has more than {MAX_FILES} entries")));
    }

    // The theme root is wherever the shallowest theme.json is.
    let mut root: Option<PathBuf> = None;
    for i in 0..archive.len() {
        let file = archive.by_index(i).map_err(|err| bad(err.to_string()))?;
        let Some(path) = file.enclosed_name() else {
            continue;
        };
        if path.file_name().is_some_and(|name| name == "theme.json") {
            let parent = path.parent().unwrap_or(Path::new("")).to_owned();
            if root
                .as_ref()
                .is_none_or(|r| parent.components().count() < r.components().count())
            {
                root = Some(parent);
            }
        }
    }
    let root = root.ok_or_else(|| {
        bad("no theme.json found; only block themes (like Twenty Twenty-Five) are supported".into())
    })?;

    let slug = if let Some(slug) = slug {
        slug
    } else {
        let name = root
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| bad("theme zip has no top-level folder; pass a slug".into()))?;
        validate_slug(&name.to_lowercase())?
    };

    std::fs::create_dir_all(themes_dir)?;
    let staging = themes_dir.join(format!(".install-{}", uuid::Uuid::new_v4().simple()));
    let result = (|| -> AppResult<()> {
        let mut total = 0u64;
        let mut count = 0usize;
        for i in 0..archive.len() {
            let mut file = archive.by_index(i).map_err(|err| bad(err.to_string()))?;
            if file.is_dir() {
                continue;
            }
            let Some(path) = file.enclosed_name() else {
                continue;
            };
            let Ok(relative) = path.strip_prefix(&root) else {
                continue;
            };
            if relative
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("php"))
            {
                continue;
            }
            count += 1;
            if count > MAX_FILES {
                return Err(bad(format!("theme has more than {MAX_FILES} files")));
            }
            let target = staging.join(relative);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut out = std::fs::File::create(&target)?;
            let budget = MAX_EXTRACTED_BYTES - total;
            let written = std::io::copy(&mut (&mut file).take(budget + 1), &mut out)?;
            total += written;
            if total > MAX_EXTRACTED_BYTES {
                return Err(bad(format!(
                    "theme unpacks to more than {MAX_EXTRACTED_BYTES} bytes"
                )));
            }
        }
        read_json(&staging.join("theme.json"))?;
        // Move an earlier install aside rather than deleting it first, so
        // the theme is only missing between two renames, and a failed swap
        // puts it back.
        let target = themes_dir.join(&slug);
        let old = themes_dir.join(format!(".old-{}", uuid::Uuid::new_v4().simple()));
        let replacing = match std::fs::rename(&target, &old) {
            Ok(()) => true,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
            Err(err) => return Err(err.into()),
        };
        if let Err(err) = std::fs::rename(&staging, &target) {
            if replacing {
                let _ = std::fs::rename(&old, &target);
            }
            return Err(err.into());
        }
        if replacing {
            let _ = std::fs::remove_dir_all(&old);
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result.map(|()| slug)
}

pub fn validate_slug(slug: &str) -> AppResult<String> {
    let valid = !slug.is_empty()
        && slug.len() <= 64
        && !slug.starts_with(['-', '.'])
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if valid {
        Ok(slug.to_owned())
    } else {
        Err(AppError::BadRequest(format!(
            "invalid theme name '{slug}': use lowercase letters, digits, '-' and '_'"
        )))
    }
}

// ---------------------------------------------------------------------------
// Stylesheet
// ---------------------------------------------------------------------------

/// The site stylesheet for the active theme.
pub struct Stylesheet {
    pub css: String,
    /// Changes whenever `css` does; used to bust caches.
    pub version: String,
    /// How wide the theme lays content out.
    pub layout: Layout,
}

#[derive(Clone, PartialEq)]
struct CacheKey {
    theme: Option<String>,
    variations: Vec<String>,
    modified: Option<SystemTime>,
}

type Cached = Arc<Mutex<Option<(CacheKey, Arc<Stylesheet>)>>>;

/// Compiling a theme takes a few milliseconds; this keeps the last result.
#[derive(Default)]
pub struct ThemeCache {
    last: Cached,
    /// Held for the whole of a build, so requests that miss the cache at the
    /// same time wait for one build instead of each starting their own.
    building: Arc<tokio::sync::Mutex<()>>,
}

impl ThemeCache {
    pub async fn stylesheet(&self, themes_dir: &Path, settings: &Settings) -> Arc<Stylesheet> {
        // A theme that has gone missing falls back to the built-in look
        // rather than leaving the site unstyled.
        let theme = settings.theme.clone().filter(|t| {
            validate_slug(t).is_ok() && themes_dir.join(t).join("theme.json").is_file()
        });
        let modified = theme.as_ref().and_then(|t| {
            std::fs::metadata(themes_dir.join(t).join("theme.json"))
                .and_then(|m| m.modified())
                .ok()
        });
        let key = CacheKey {
            variations: if theme.is_some() {
                settings.theme_variations.clone()
            } else {
                Vec::new()
            },
            theme,
            modified,
        };
        if let Some(sheet) = cached(&self.last, &key) {
            return sheet;
        }
        let guard = self.building.clone().lock_owned().await;
        if let Some(sheet) = cached(&self.last, &key) {
            return sheet;
        }

        // The build keeps the lock and fills the cache itself, so it counts
        // even when the request that started it has gone away.
        let dir = themes_dir.to_owned();
        let last = self.last.clone();
        blocking(move || {
            let _guard = guard;
            let sheet = Arc::new(stylesheet(
                build(&dir, key.theme.as_deref(), &key.variations).unwrap_or_else(|err| {
                    tracing::error!(error = %err, "failed to compile theme, using the built-in one");
                    builtin()
                }),
            ));
            *last.lock().unwrap_or_else(PoisonError::into_inner) = Some((key, sheet.clone()));
            Ok(sheet)
        })
        .await
        .unwrap_or_else(|err| {
            tracing::error!(error = %err, "failed to compile theme, using the built-in one");
            Arc::new(stylesheet(builtin()))
        })
    }
}

fn cached(last: &Cached, key: &CacheKey) -> Option<Arc<Stylesheet>> {
    match last.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
        Some((cached_key, sheet)) if cached_key == key => Some(sheet.clone()),
        _ => None,
    }
}

fn builtin() -> (String, Layout) {
    build(Path::new(""), None, &[]).expect("built-in theme compiles")
}

fn stylesheet((css, layout): (String, Layout)) -> Stylesheet {
    let css = css::minify(&css);
    let mut hasher = DefaultHasher::new();
    css.hash(&mut hasher);
    Stylesheet {
        css,
        version: format!("{:016x}", hasher.finish()),
        layout,
    }
}

fn build(
    themes_dir: &Path,
    theme: Option<&str>,
    variations: &[String],
) -> AppResult<(String, Layout)> {
    let Some(slug) = theme else {
        let json: Value =
            serde_json::from_str(DEFAULT_THEME).expect("built-in theme is valid JSON");
        let compiled = css::compile(&css::Input {
            theme: &json,
            block_styles: &[],
            asset_base: "",
            style_css: None,
        })
        .map_err(|err| AppError::Internal(format!("built-in theme: {err}")))?;
        return Ok((site_css::assemble(&json, &compiled, true), layout(&json)));
    };

    let dir = themes_dir.join(slug);
    let mut json = read_json(&dir.join("theme.json"))?;
    for id in variations {
        // A variation that has gone away, say with a reinstall, is left
        // out rather than costing the site the whole theme.
        let variation = match variation_path(&dir, id).and_then(|path| read_json(&path)) {
            Ok(variation) => variation,
            Err(err) => {
                tracing::warn!(theme = slug, variation = %id, error = %err, "skipping style variation");
                continue;
            }
        };
        for key in ["settings", "styles"] {
            if let Some(part) = variation.get(key) {
                let target = json
                    .as_object_mut()
                    .ok_or_else(|| AppError::BadRequest("theme.json is not an object".into()))?
                    .entry(key)
                    .or_insert_with(|| Value::Object(serde_json::Map::default()));
                css::merge(target, part);
            }
        }
    }

    let block_styles = block_styles(&dir, &json);
    let style_css = std::fs::read_to_string(dir.join("style.css")).ok();
    let compiled = css::compile(&css::Input {
        theme: &json,
        block_styles: &block_styles,
        asset_base: &asset_base(&dir, slug),
        style_css: style_css.as_deref(),
    })
    .map_err(|err| AppError::BadRequest(format!("theme '{slug}': {err}")))?;
    Ok((site_css::assemble(&json, &compiled, false), layout(&json)))
}

/// Where the stylesheet finds the theme's fonts and images:
/// `/_wm/theme/{slug}@{stamp}`. The stamp changes when the theme is
/// installed again or one of its files is written, so the files can be
/// cached for good.
fn asset_base(dir: &Path, slug: &str) -> String {
    format!("/_wm/theme/{slug}@{}", stamp(dir))
}

/// Whether `stamp` names the theme's files as they are now, so they may be
/// cached for good under it. An old or made-up stamp may not: it would keep
/// whatever is served now once the theme changes.
pub fn is_current_stamp(themes_dir: &Path, slug: &str, stamp: &str) -> bool {
    stamp == self::stamp(&themes_dir.join(slug))
}

/// `theme.json`'s modification time in nanoseconds, which every install and
/// file write updates.
fn stamp(dir: &Path) -> String {
    let nanos = std::fs::metadata(dir.join("theme.json"))
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_nanos());
    format!("{nanos:x}")
}

/// The content and wide sizes from `settings.layout`, where they are given
/// in `px` or `rem`.
fn layout(theme: &Value) -> Layout {
    let size = |key: &str| {
        let value = theme
            .pointer(&format!("/settings/layout/{key}"))?
            .as_str()?
            .trim();
        let pixels = if let Some(px) = value.strip_suffix("px") {
            px.trim().parse::<f64>().ok()?
        } else {
            value.strip_suffix("rem")?.trim().parse::<f64>().ok()? * 16.0
        };
        if !(1.0..10_000.0).contains(&pixels) {
            return None;
        }
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "checked to lie between 1 and 10000 just above"
        )]
        let pixels = pixels.round() as u32;
        Some(pixels)
    };
    let fallback = Layout::default();
    Layout {
        content: size("contentSize").unwrap_or(fallback.content),
        wide: size("wideSize").unwrap_or(fallback.wide),
    }
}

fn variation_path(theme_dir: &Path, id: &str) -> AppResult<PathBuf> {
    let relative = Path::new(id);
    let safe = relative
        .components()
        .all(|c| matches!(c, Component::Normal(_)));
    if !safe || id.is_empty() {
        return Err(AppError::BadRequest(format!(
            "invalid style variation '{id}'"
        )));
    }
    let path = theme_dir.join("styles").join(format!("{id}.json"));
    if !path.is_file() {
        return Err(AppError::NotFound(format!("style variation '{id}'")));
    }
    Ok(path)
}

/// Section and block styles from `styles/**`, with overrides from the merged
/// `styles.variations` applied on top.
fn block_styles(theme_dir: &Path, theme: &Value) -> Vec<css::BlockStyle> {
    let mut found: Vec<css::BlockStyle> = Vec::new();
    let mut visit = |path: &Path| {
        let Ok(json) = read_json(path) else { return };
        if json.get("blockTypes").is_none() {
            return;
        }
        let slug = json
            .get("slug")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| path.file_stem().and_then(|s| s.to_str()).map(str::to_owned));
        if let (Some(slug), Some(styles)) = (slug, json.get("styles")) {
            found.push(css::BlockStyle {
                slug,
                styles: styles.clone(),
            });
        }
    };
    if let Ok(entries) = std::fs::read_dir(theme_dir.join("styles")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Ok(files) = std::fs::read_dir(&path) {
                    for file in files.flatten() {
                        visit(&file.path());
                    }
                }
            } else {
                visit(&path);
            }
        }
    }
    found.sort_by(|a, b| a.slug.cmp(&b.slug));

    if let Some(overrides) = theme
        .get("styles")
        .and_then(|s| s.get("variations"))
        .and_then(Value::as_object)
    {
        for (slug, styles) in overrides {
            match found.iter_mut().find(|s| &s.slug == slug) {
                Some(existing) => css::merge(&mut existing.styles, styles),
                None => found.push(css::BlockStyle {
                    slug: slug.clone(),
                    styles: styles.clone(),
                }),
            }
        }
    }
    found
}

fn read_json(path: &Path) -> AppResult<Value> {
    let text = std::fs::read_to_string(path)?;
    serde_json::from_str(&text)
        .map_err(|err| AppError::BadRequest(format!("{} is not valid JSON: {err}", path.display())))
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> AppResult<T> + Send + 'static,
) -> AppResult<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|err| AppError::Internal(format!("theme task failed: {err}")))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(theme: Option<&str>, variations: &[&str]) -> Settings {
        Settings {
            site_title: String::new(),
            site_language: "en".into(),
            site_tagline: None,
            footer_text: None,
            home_slug: None,
            theme: theme.map(str::to_owned),
            theme_variations: variations.iter().map(|v| (*v).to_owned()).collect(),
        }
    }

    #[tokio::test]
    async fn concurrent_requests_share_one_build() {
        let cache = Arc::new(ThemeCache::default());
        let builds: Vec<_> = (0..8)
            .map(|_| {
                let cache = cache.clone();
                tokio::spawn(async move {
                    cache
                        .stylesheet(Path::new("/nonexistent"), &settings(None, &[]))
                        .await
                })
            })
            .collect();
        let mut sheets = Vec::new();
        for build in builds {
            sheets.push(build.await.unwrap());
        }
        assert!(sheets.iter().all(|s| Arc::ptr_eq(s, &sheets[0])));
    }

    fn temp_themes() -> PathBuf {
        std::env::temp_dir().join(format!(
            "wordmaker-theme-test-{}",
            uuid::Uuid::new_v4().simple()
        ))
    }

    fn write_theme(themes_dir: &Path, slug: &str, theme_json: &str) {
        let dir = themes_dir.join(slug);
        std::fs::create_dir_all(dir.join("styles")).unwrap();
        std::fs::write(dir.join("theme.json"), theme_json).unwrap();
    }

    #[test]
    fn theme_json_in_any_spelling() {
        for path in ["theme.json", "THEME.JSON", "Theme.Json", "theme.json/"] {
            assert!(is_theme_json(path), "{path}");
        }
        for path in ["styles/theme.json", "theme.json5", "xtheme.json"] {
            assert!(!is_theme_json(path), "{path}");
        }
    }

    #[test]
    fn zip_with_too_many_entries_is_refused() {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("t/theme.json", options).unwrap();
        for i in 0..MAX_FILES {
            zip.start_file(format!("t/{i}.txt"), options).unwrap();
        }
        let bytes = zip.finish().unwrap().into_inner();
        let themes = temp_themes();
        let err = extract(&themes, &bytes, None).unwrap_err();
        assert!(err.to_string().contains("entries"), "{err}");
        assert!(!themes.exists());
    }

    fn zip_of(files: &[(&str, &str)]) -> Vec<u8> {
        use std::io::Write;

        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, content) in files {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(content.as_bytes()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn reinstall_swaps_the_theme_in() {
        let themes = temp_themes();
        let first = zip_of(&[("t/theme.json", "{}"), ("t/old.css", "a{}")]);
        let second = zip_of(&[("t/theme.json", "{}"), ("t/new.css", "b{}")]);
        extract(&themes, &first, None).unwrap();
        extract(&themes, &second, None).unwrap();
        assert!(themes.join("t/new.css").is_file());
        assert!(!themes.join("t/old.css").exists());
        let names: Vec<_> = std::fs::read_dir(&themes)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["t"], "nothing left aside");

        // A broken zip leaves the installed theme as it was.
        let broken = zip_of(&[("t/theme.json", "{")]);
        assert!(extract(&themes, &broken, None).is_err());
        assert!(themes.join("t/new.css").is_file());
        std::fs::remove_dir_all(&themes).unwrap();
    }

    #[tokio::test]
    async fn downloads_stay_on_the_public_internet() {
        for url in [
            "http://127.0.0.1:1/theme.zip",
            "HTTPS://localhost/theme.zip",
            "http://[::1]/theme.zip",
            "http://10.0.0.5:8080/theme.zip",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::ffff:192.168.0.1]/theme.zip",
        ] {
            let err = download(url).await.unwrap_err().to_string();
            assert!(err.contains("not on the public internet"), "{url}: {err}");
        }
        for url in ["ftp://example.com/theme.zip", "file:///etc/passwd"] {
            let err = download(url).await.unwrap_err().to_string();
            assert!(err.contains("http or https"), "{url}: {err}");
        }
        assert!(download("not a url").await.is_err());
    }

    #[test]
    fn missing_variation_is_skipped() {
        let themes = temp_themes();
        write_theme(
            &themes,
            "t",
            r##"{"settings":{"color":{"palette":[{"slug":"accent","color":"#123456"}]}}}"##,
        );
        std::fs::write(
            themes.join("t/styles/night.json"),
            r##"{"styles":{"color":{"text":"#abcdef"}}}"##,
        )
        .unwrap();
        let (css, _) = build(&themes, Some("t"), &["gone".into(), "night".into()]).unwrap();
        assert!(
            css.contains("--wp--preset--color--accent: #123456"),
            "{css}"
        );
        assert!(css.contains("color: #abcdef"), "{css}");
        std::fs::remove_dir_all(&themes).unwrap();
    }
}
