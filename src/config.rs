use std::path::PathBuf;

/// Settings shared by every subcommand.
#[derive(Clone, Debug, clap::Args)]
pub struct Config {
    /// SQLite database URL.
    #[arg(
        long = "db",
        env = "WORDMAKER_DB",
        default_value = "sqlite://wordmaker.db"
    )]
    pub database_url: String,

    /// Directory uploaded media files are stored in.
    #[arg(long, env = "WORDMAKER_MEDIA_DIR", default_value = "media")]
    pub media_dir: PathBuf,

    /// Directory installed themes are stored in.
    #[arg(long, env = "WORDMAKER_THEMES_DIR", default_value = "themes")]
    pub themes_dir: PathBuf,

    /// Public URL of the site, used to build links to pages and media.
    #[arg(
        long,
        env = "WORDMAKER_BASE_URL",
        default_value = "http://127.0.0.1:3000"
    )]
    pub base_url: String,

    /// Largest accepted media upload, in bytes.
    #[arg(long, env = "WORDMAKER_MAX_UPLOAD", default_value_t = 25 * 1024 * 1024)]
    pub max_upload_bytes: usize,
}

impl Config {
    pub fn page_url(&self, slug: &str) -> String {
        format!("{}/{slug}", self.base_url.trim_end_matches('/'))
    }

    pub fn media_url(&self, filename: &str) -> String {
        format!("{}/media/{filename}", self.base_url.trim_end_matches('/'))
    }
}
