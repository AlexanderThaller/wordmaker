//! Put a compiled theme together with the rules for WordMaker's page frame
//! and for AsciiDoc content.
//!
//! The AsciiDoc rules from `adocers-html` go into a cascade layer, so every
//! rule the theme makes wins over them while they still cover what a theme
//! knows nothing about: admonitions, callouts, syntax highlighting.

use std::fmt::Write;

use serde_json::Value;

use super::css;

pub fn assemble(theme: &Value, compiled: &str, builtin: bool) -> String {
    let mut out = String::with_capacity(compiled.len() + 24 * 1024);
    let _ = writeln!(
        out,
        "@layer adocers {{\n{}\n{}\n}}",
        adocers_html::document_stylesheet(),
        adocers_html::toc_stylesheet()
    );
    if builtin {
        // The built-in theme's palette is defined in terms of these, so it
        // follows the reader's light or dark preference.
        out.push_str(adocers_html::stylesheet_variables());
    } else {
        out.push_str(&bridge_variables(theme));
    }
    out.push_str(FRAME_CSS);
    out.push_str(compiled);
    out.push_str(CONTENT_CSS);
    out
}

/// Point the variables the AsciiDoc rules read at the theme's colours.
fn bridge_variables(theme: &Value) -> String {
    let bg = css::css_at(theme, &["styles", "color", "background"])
        .unwrap_or_else(|| "var(--wp--preset--color--base, #fff)".into());
    let fg = css::css_at(theme, &["styles", "color", "text"])
        .unwrap_or_else(|| "var(--wp--preset--color--contrast, #111)".into());
    let link = css::css_at(theme, &["styles", "elements", "link", "color", "text"])
        .filter(|c| !c.eq_ignore_ascii_case("currentcolor") && c != "inherit");
    let accent = link.unwrap_or_else(|| "var(--fg)".into());
    let code_bg = css::css_at(
        theme,
        &["styles", "blocks", "core/code", "color", "background"],
    )
    .unwrap_or_else(|| "color-mix(in srgb, var(--fg) 5%, var(--bg))".into());

    let dark = css::literal_color(theme, &bg)
        .and_then(luminance)
        .is_some_and(|l| l < 0.35);
    let highlight = if dark {
        HIGHLIGHT_DARK
    } else {
        HIGHLIGHT_LIGHT
    };

    format!(
        ":root {{\n  color-scheme: {scheme};\n  --bg: {bg};\n  --fg: {fg};\n  --accent: \
         {accent};\n  --muted: color-mix(in srgb, var(--fg) 65%, var(--bg));\n  --rule: \
         color-mix(in srgb, var(--fg) 18%, var(--bg));\n  --code-bg: {code_bg};\n  --sidebar-bg: \
         color-mix(in srgb, var(--fg) 5%, var(--bg));\n{highlight}}}\n",
        scheme = if dark { "dark" } else { "light" },
    )
}

/// Relative luminance of a `#rgb`, `#rrggbb` or `#rrggbbaa` colour.
fn luminance(color: &str) -> Option<f64> {
    let hex = color.trim().strip_prefix('#')?;
    let channel = |s: &str| u8::from_str_radix(s, 16).ok().map(f64::from);
    let (r, g, b) = match hex.len() {
        3 => {
            let expand = |i: usize| channel(&hex[i..=i].repeat(2));
            (expand(0)?, expand(1)?, expand(2)?)
        }
        6 | 8 => (
            channel(&hex[0..2])?,
            channel(&hex[2..4])?,
            channel(&hex[4..6])?,
        ),
        _ => return None,
    };
    let linear = |c: f64| {
        let c = c / 255.0;
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    Some(0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b))
}

const HIGHLIGHT_LIGHT: &str = "  --admon-note: #19407c; --admon-tip: #b58900; --admon-important: \
                               #bf6900;
  --admon-caution: #bf3400; --admon-warning: #bf0000;
  --hl-comment: #656d77; --hl-keyword: #cf222e; --hl-string: #0a3069; --hl-number: #0550ae;
  --hl-function: #6639ba; --hl-type: #953800; --hl-constant: #0550ae; --hl-property: #0550ae;
  --hl-tag: #116329; --hl-punctuation: #57606a;
";

const HIGHLIGHT_DARK: &str = "  --admon-note: #6ea8e8; --admon-tip: #e3b341; --admon-important: \
                              #e08c3e;
  --admon-caution: #f0764a; --admon-warning: #f0716f;
  --hl-comment: #8b949e; --hl-keyword: #ff7b72; --hl-string: #a5d6ff; --hl-number: #79c0ff;
  --hl-function: #d2a8ff; --hl-type: #ffa657; --hl-constant: #79c0ff; --hl-property: #79c0ff;
  --hl-tag: #7ee787; --hl-punctuation: #8b949e;
";

/// The few block-library rules the page frame relies on. They come before the
/// theme's styles, as WordPress's own block CSS does, so a theme can override
/// them.
const FRAME_CSS: &str = r#"
*, *::before, *::after { box-sizing: border-box; }
body { margin: 0; background: var(--bg); color: var(--fg); }
.wp-site-blocks { min-height: 100vh; display: flex; flex-direction: column; }
.wp-site-blocks > main { flex: 1 0 auto; }
.wp-block-site-title { margin: 0; }
.wp-block-site-title :where(a) { color: inherit; font-family: inherit; font-size: inherit; font-style: inherit; font-weight: inherit; letter-spacing: inherit; line-height: inherit; text-decoration: inherit; }
.wp-block-site-tagline { margin: 0; }
.wp-block-navigation { display: flex; align-items: center; }
.wp-block-navigation__container { list-style: none; margin: 0; padding-left: 0; display: flex; flex-wrap: wrap; align-items: center; gap: inherit; }
.wp-block-navigation .wp-block-navigation-item__content { color: inherit; display: block; }
.wp-block-post-title { word-break: break-word; box-sizing: border-box; }
.wm-header-row { justify-content: space-between; flex-wrap: nowrap; }
.wm-header-row .wp-block-navigation { justify-content: flex-end; }
.wm-page-list { list-style: none; padding: 0; }
.wm-page-list > li + li { margin-top: var(--wp--style--block-gap); }
.wm-footer-text > :first-child { margin-top: 0; }

/* Responsive navigation: inline from 600px, an overlay behind a menu button
   below, as WordPress's navigation block lays it out. The overlay is a
   popover, so the browser's own popover styles are reset here. */
.wp-block-navigation__responsive-container-open,
.wp-block-navigation__responsive-container-close { display: flex; align-items: center; justify-content: center; padding: 0; border: 0; background: transparent; color: currentColor; cursor: pointer; }
.wp-block-navigation__responsive-container-open svg,
.wp-block-navigation__responsive-container-close svg { fill: currentColor; display: block; }
.wp-block-navigation__responsive-container[popover] { position: static; inset: auto; width: auto; height: auto; margin: 0; padding: 0; border: 0; background: transparent; color: inherit; overflow: visible; }
.wp-block-navigation__responsive-container-close { display: none; }
@media (min-width: 600px) {
  .wp-block-navigation__responsive-container-open { display: none; }
  .wp-block-navigation__responsive-container[popover] { display: block; }
}
@media (max-width: 599.98px) {
  .wp-block-navigation__responsive-container:popover-open { position: fixed; inset: 0; width: 100%; height: 100%; max-width: none; max-height: none; z-index: 100000; display: flex; flex-direction: column; overflow: auto; background: var(--wp--preset--color--base, var(--bg)); color: var(--wp--preset--color--contrast, var(--fg)); padding: clamp(1rem, var(--wp--style--root--padding-top), 20rem) clamp(1rem, var(--wp--style--root--padding-right), 20rem) clamp(1rem, var(--wp--style--root--padding-bottom), 20rem) clamp(1rem, var(--wp--style--root--padding-left), 20rem); }
  .wp-block-navigation__responsive-container:popover-open .wp-block-navigation__responsive-container-close { display: flex; align-self: flex-end; }
  .wp-block-navigation__responsive-container:popover-open .wp-block-navigation__container { flex-direction: column; flex-wrap: nowrap; align-items: flex-end; gap: var(--wp--style--block-gap, 2em); padding-top: calc(2rem + 24px); }
  :root:has(.wp-block-navigation__responsive-container:popover-open) { overflow: hidden; }
}

/* Core blocks, for WordPress block markup pasted into a page as a
   passthrough. WordPress adds layout classes when it renders a post; saved
   markup has none, so these rules stand in for both. */
.wp-block-columns { display: flex; flex-wrap: wrap; align-items: normal; gap: var(--wp--style--block-gap, 2em); }
.wp-block-columns.are-vertically-aligned-top { align-items: flex-start; }
.wp-block-columns.are-vertically-aligned-center { align-items: center; }
.wp-block-columns.are-vertically-aligned-bottom { align-items: flex-end; }
.wp-block-column { flex-grow: 1; flex-basis: 0; min-width: 0; overflow-wrap: break-word; }
.wp-block-column.is-vertically-aligned-top { align-self: flex-start; }
.wp-block-column.is-vertically-aligned-center { align-self: center; }
.wp-block-column.is-vertically-aligned-bottom { align-self: flex-end; }
.wp-block-column > * { margin-block: 0; }
.wp-block-column > * + * { margin-block-start: var(--wp--style--block-gap); }
@media (max-width: 781px) {
  .wp-block-columns:not(.is-not-stacked-on-mobile) > .wp-block-column { flex-basis: 100% !important; }
}
@media (min-width: 782px) {
  .wp-block-columns { flex-wrap: nowrap; }
  .wp-block-columns > .wp-block-column[style*="flex-basis"] { flex-grow: 0; }
}
.wp-block-image { margin-left: 0; margin-right: 0; }
.wp-block-image img { display: block; max-width: 100%; height: auto; box-sizing: border-box; }
.wp-block-image .wp-element-caption { margin-top: 0.5em; margin-bottom: 1em; }
.wp-block-gallery.has-nested-images { display: flex; flex-wrap: wrap; align-items: normal; gap: var(--wp--style--block-gap, 0.5em); margin-left: 0; margin-right: 0; }
.wp-block-gallery.has-nested-images > figure.wp-block-image { margin: 0; flex-grow: 1; width: calc(50% - var(--wp--style--block-gap, 0.5em) / 2); display: flex; flex-direction: column; }
@media (min-width: 600px) {
  .wp-block-gallery.columns-3 > figure.wp-block-image { width: calc(33.333% - var(--wp--style--block-gap, 0.5em) * 2 / 3); }
  .wp-block-gallery.columns-4 > figure.wp-block-image { width: calc(25% - var(--wp--style--block-gap, 0.5em) * 3 / 4); }
}
.wp-block-gallery.is-cropped > figure.wp-block-image img { width: 100%; flex: 1 0 0%; object-fit: cover; aspect-ratio: 3 / 4; }
"#;

/// Fit Asciidoctor's markup into WordPress's flow layout: section wrappers act
/// as constrained containers, block spacing comes from the theme's block gap.
const CONTENT_CSS: &str = r#"
.wm-adoc :is(.sect1, .sect2, .sect3, .sect4, .sect5, .sect6, .sectionbody) { max-width: none !important; }
.wm-adoc :is(.sect1, .sect2, .sect3, .sect4, .sect5, .sect6, .sectionbody) > * { margin-block-start: var(--wp--style--block-gap); margin-block-end: 0; }
.wm-adoc :is(.sect1, .sect2, .sect3, .sect4, .sect5, .sect6, .sectionbody) > :first-child { margin-block-start: 0; }
.wm-adoc :is(.sect1, .sect2, .sect3, .sect4, .sect5, .sect6, .sectionbody) > :where(:not(.alignleft, .alignright, .alignfull, .alignwide, .sectionbody, .sect2, .sect3, .sect4, .sect5, .sect6)) { max-width: var(--wp--style--global--content-size); margin-left: auto; margin-right: auto; }
.wm-adoc :is(.sect1, .sect2, .sect3, .sect4, .sect5, .sect6, .sectionbody) > .alignwide { max-width: var(--wp--style--global--wide-size); margin-left: auto; margin-right: auto; }
.wm-adoc :is(.sect1, .sect2, .sect3, .sect4, .sect5, .sect6, .sectionbody) > .alignfull { margin-left: calc(var(--wp--style--root--padding-left) * -1); margin-right: calc(var(--wp--style--root--padding-right) * -1); }
.wm-adoc :is(.sect1, .sect2, .sect3, .sect4, .sect5, .sect6):not(:first-child) { margin-block-start: calc(var(--wp--style--block-gap) * 2); }
.wm-adoc :where(h1, h2, h3, h4, h5, h6) { margin-bottom: 0; padding-bottom: 0; border-bottom: 0; }
.wm-adoc :where(.paragraph, li, dd, td, th) > p { margin: 0; }
.wm-adoc :where(.content, td.content, li, dd, blockquote) > * + * { margin-block-start: 0.75em; }
.wm-adoc :where(.ulist, .olist) > :where(ul, ol) { margin: 0; }
.wm-adoc :where(.openblock, .exampleblock)[class*="is-style-"] { padding: var(--wp--preset--spacing--50, 2rem); }
.wm-adoc :where(.openblock, .exampleblock)[class*="is-style-"] > .content > * + * { margin-block-start: var(--wp--style--block-gap); }
.wm-adoc .quoteblock > blockquote { margin: 0; padding: 0; border: 0; }
.wm-adoc :is(.openblock, .exampleblock, .sidebarblock):is(.alignfull, .alignwide) > .content > :where(:not(.alignfull, .alignwide)) { max-width: var(--wp--style--global--content-size); margin-left: auto; margin-right: auto; }
.wm-adoc :is(.openblock, .exampleblock, .sidebarblock).alignfull { padding-left: var(--wp--style--root--padding-left); padding-right: var(--wp--style--root--padding-right); }
.wm-adoc .imageblock img { display: block; }
.wm-adoc :where(.imageblock).alignfull img, .wm-adoc :where(.imageblock).alignwide img { width: 100%; }
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn luminance_of_colors() {
        assert!(luminance("#FFFFFF").unwrap() > 0.99);
        assert!(luminance("#1B1B1B").unwrap() < 0.05);
        assert!(luminance("#fff").unwrap() > 0.99);
        assert!(luminance("#FFFFFF33").unwrap() > 0.99);
        assert!(luminance("var(--x)").is_none());
    }
}
