use serde::Serialize;

use crate::error::{
    AppError,
    AppResult,
};

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Rendered {
    /// The document body as an HTML fragment.
    pub html: String,
    /// The outline as HTML, when the document has sections.
    pub toc: Option<String>,
}

/// Render `AsciiDoc` source to an HTML fragment.
pub fn render(source: &str) -> Rendered {
    let mut parser = asciidoc_parser::Parser::default();
    let document = parser.parse(source);
    let options = adocers_html::Options {
        fragment: true,
        icons: true,
        highlight: true,
        mermaid: true,
        math: true,
        ..adocers_html::Options::default()
    };
    let rendered = adocers_html::render(&document, &options);
    Rendered {
        html: rendered.html,
        toc: rendered.toc,
    }
}

/// [`render`] on the blocking pool; highlighting and diagrams can be slow.
pub async fn render_async(source: String) -> AppResult<Rendered> {
    tokio::task::spawn_blocking(move || render(&source))
        .await
        .map_err(|err| AppError::Internal(format!("render task failed: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_paragraphs_and_sections() {
        let out = render("== Hello\n\nSome *bold* prose.\n");
        assert!(out.html.contains("<strong>bold</strong>"), "{}", out.html);
        assert!(out.html.contains("Hello"), "{}", out.html);
        assert!(out.toc.is_some());
    }

    #[test]
    fn columns_markup_matches_the_column_css() {
        // site_css styles `.openblock.wp-block-columns > .content` as the
        // flex container and `.wp-block-column` blocks inside it.
        let out = render(
            "[.wp-block-columns]
--
[.wp-block-column]
====
Left
====

[.wp-block-column]
====
Right
====
--
",
        );
        let html = out.html.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            html.contains(
                r#"<div class="openblock wp-block-columns"> <div class="content"> <div class="exampleblock wp-block-column">"#
            ),
            "{html}"
        );
        assert_eq!(
            html.matches("exampleblock wp-block-column").count(),
            2,
            "{html}"
        );
    }
}
