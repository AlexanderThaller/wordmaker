use std::sync::Arc;

use base64::Engine;
use rmcp::{
    ErrorData, ServerHandler,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::{
    config::Config,
    domain::{
        media,
        pages::{self, NewPage, PageUpdate, Status},
        settings::{self, SettingsUpdate},
    },
    error::AppResult,
    render,
};

const INSTRUCTIONS: &str = "\
WordMaker is a small CMS. Each page has a title, a URL slug and a body written in AsciiDoc.

- Pages are created as drafts and only appear on the public site once published \
  (pass `publish: true` to create_page, or call publish_page).
- Write the body in AsciiDoc. The title is rendered as the page heading, so do not start \
  the body with a `= Title` line; use `==` for top-level sections.
- Use preview_adoc to check how AsciiDoc renders before saving.
- Every change to title or body is kept as a revision; list_revisions and restore_revision \
  undo mistakes.
- Upload images with upload_media and reference them by the returned `path`, \
  e.g. `image::/media/1a2b3c4d-logo.png[Logo]`.
- Link to other pages with `link:/other-slug[Other page]`.
- set_settings controls the site title and which page is shown at `/`.";

#[derive(Clone)]
pub struct WordMakerMcp {
    pool: SqlitePool,
    config: Arc<Config>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListPagesArgs {
    /// Only list pages with this status.
    #[serde(default)]
    pub status: Option<Status>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SlugArgs {
    /// The page's slug.
    pub slug: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdatePageArgs {
    /// Slug of the page to change.
    pub slug: String,
    /// New title.
    #[serde(default)]
    pub title: Option<String>,
    /// New slug; the page moves to the new URL.
    #[serde(default)]
    pub new_slug: Option<String>,
    /// New AsciiDoc body, replacing the old one entirely.
    #[serde(default)]
    pub body: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RevisionArgs {
    /// The page's slug.
    pub slug: String,
    /// Revision number, as listed by list_revisions.
    pub revision: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PreviewArgs {
    /// AsciiDoc source to render.
    pub body: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UploadMediaArgs {
    /// File name including extension, e.g. `logo.png`.
    pub filename: String,
    /// File content, base64 encoded.
    pub content_base64: String,
    /// MIME type; guessed from the file name when omitted.
    #[serde(default)]
    pub content_type: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MediaArgs {
    /// Stored file name, as returned by upload_media or list_media.
    pub filename: String,
}

impl WordMakerMcp {
    pub fn new(pool: SqlitePool, config: Arc<Config>) -> Self {
        Self { pool, config }
    }
}

#[tool_router]
impl WordMakerMcp {
    #[tool(description = "List pages (without bodies). Optionally filter by status.")]
    async fn list_pages(
        &self,
        Parameters(args): Parameters<ListPagesArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        respond(pages::list(&self.pool, args.status).await)
    }

    #[tool(description = "Get a page including its AsciiDoc body and public URL.")]
    async fn get_page(
        &self,
        Parameters(args): Parameters<SlugArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        respond(
            pages::get(&self.pool, &args.slug)
                .await
                .map(|p| p.into_view(&self.config)),
        )
    }

    #[tool(description = "Create a page. It is a draft unless `publish` is true.")]
    async fn create_page(
        &self,
        Parameters(args): Parameters<NewPage>,
    ) -> Result<CallToolResult, ErrorData> {
        respond(
            pages::create(&self.pool, args)
                .await
                .map(|p| p.into_view(&self.config)),
        )
    }

    #[tool(
        description = "Change a page's title, slug and/or body. Omitted fields stay as they are; \
                          a given body replaces the whole body."
    )]
    async fn update_page(
        &self,
        Parameters(args): Parameters<UpdatePageArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        respond(
            pages::update(
                &self.pool,
                &args.slug,
                PageUpdate {
                    title: args.title,
                    slug: args.new_slug,
                    body: args.body,
                },
            )
            .await
            .map(|p| p.into_view(&self.config)),
        )
    }

    #[tool(description = "Permanently delete a page and its revision history.")]
    async fn delete_page(
        &self,
        Parameters(args): Parameters<SlugArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        respond(
            pages::delete(&self.pool, &args.slug)
                .await
                .map(|()| serde_json::json!({ "deleted": args.slug })),
        )
    }

    #[tool(description = "Publish a page so it appears on the public site.")]
    async fn publish_page(
        &self,
        Parameters(args): Parameters<SlugArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        respond(
            pages::set_status(&self.pool, &args.slug, Status::Published)
                .await
                .map(|p| p.into_view(&self.config)),
        )
    }

    #[tool(
        description = "Turn a published page back into a draft, hiding it from the public site."
    )]
    async fn unpublish_page(
        &self,
        Parameters(args): Parameters<SlugArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        respond(
            pages::set_status(&self.pool, &args.slug, Status::Draft)
                .await
                .map(|p| p.into_view(&self.config)),
        )
    }

    #[tool(description = "List a page's revisions, newest first.")]
    async fn list_revisions(
        &self,
        Parameters(args): Parameters<SlugArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        respond(pages::list_revisions(&self.pool, &args.slug).await)
    }

    #[tool(description = "Get the title and body of one revision of a page.")]
    async fn get_revision(
        &self,
        Parameters(args): Parameters<RevisionArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        respond(pages::get_revision(&self.pool, &args.slug, args.revision).await)
    }

    #[tool(description = "Make an old revision current again (recorded as a new revision).")]
    async fn restore_revision(
        &self,
        Parameters(args): Parameters<RevisionArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        respond(
            pages::restore_revision(&self.pool, &args.slug, args.revision)
                .await
                .map(|p| p.into_view(&self.config)),
        )
    }

    #[tool(description = "Render AsciiDoc to HTML without saving anything, to check the markup.")]
    async fn preview_adoc(
        &self,
        Parameters(args): Parameters<PreviewArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        respond(render::render_async(args.body).await)
    }

    #[tool(
        description = "Upload a file (e.g. an image) given as base64. Returns the path to \
                          reference it from AsciiDoc."
    )]
    async fn upload_media(
        &self,
        Parameters(args): Parameters<UploadMediaArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let bytes =
            match base64::engine::general_purpose::STANDARD.decode(args.content_base64.trim()) {
                Ok(bytes) => bytes,
                Err(err) => {
                    return Ok(error_result(format!(
                        "content_base64 is not valid base64: {err}"
                    )));
                }
            };
        respond(
            media::store(
                &self.pool,
                &self.config.media_dir,
                &args.filename,
                args.content_type.as_deref(),
                &bytes,
                self.config.max_upload_bytes,
            )
            .await
            .map(|m| m.into_view(&self.config)),
        )
    }

    #[tool(description = "List uploaded media files.")]
    async fn list_media(&self) -> Result<CallToolResult, ErrorData> {
        respond(media::list(&self.pool).await.map(|all| {
            all.into_iter()
                .map(|m| m.into_view(&self.config))
                .collect::<Vec<_>>()
        }))
    }

    #[tool(description = "Delete an uploaded media file.")]
    async fn delete_media(
        &self,
        Parameters(args): Parameters<MediaArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        respond(
            media::delete(&self.pool, &self.config.media_dir, &args.filename)
                .await
                .map(|()| serde_json::json!({ "deleted": args.filename })),
        )
    }

    #[tool(description = "Get site settings: site title and the slug of the home page.")]
    async fn get_settings(&self) -> Result<CallToolResult, ErrorData> {
        respond(settings::get(&self.pool).await)
    }

    #[tool(
        description = "Change site settings. Set `home_slug` to a page slug to show that page \
                          at `/`, or to an empty string to show the list of pages instead."
    )]
    async fn set_settings(
        &self,
        Parameters(args): Parameters<SettingsUpdate>,
    ) -> Result<CallToolResult, ErrorData> {
        respond(settings::update(&self.pool, args).await)
    }
}

#[tool_handler]
impl ServerHandler for WordMakerMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("wordmaker", env!("CARGO_PKG_VERSION")))
            .with_instructions(format!(
                "{INSTRUCTIONS}\n\nThe public site is at {}.",
                self.config.base_url
            ))
    }
}

/// Domain errors become tool errors the model can read and react to, rather
/// than protocol errors.
fn respond<T: Serialize>(result: AppResult<T>) -> Result<CallToolResult, ErrorData> {
    match result {
        Ok(value) => {
            let json = serde_json::to_string_pretty(&value)
                .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
            Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
        }
        Err(err) => {
            if err.status().is_server_error() {
                tracing::error!(error = %err, "tool call failed");
            }
            Ok(error_result(err.to_string()))
        }
    }
}

fn error_result(message: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message)])
}
