//! Compile a `WordPress` `theme.json` into the stylesheet `WordPress` itself
//! would generate for it: preset custom properties, `@font-face` rules, layout
//! rules, and the theme's element, block and block-style-variation styles.
//!
//! Only the declarative part of a block theme is understood. Templates and PHP
//! patterns are not; the page frame is `WordMaker`'s own, using `WordPress`'s
//! class names so these rules apply to it.

use std::fmt::Write;

use serde_json::{
    Map,
    Value,
};

/// A block style variation (`styles/sections/*.json`, `styles/blocks/*.json`
/// or `styles.variations` in `theme.json`), compiled as `.is-style-<slug>`.
pub struct BlockStyle {
    pub slug: String,
    pub styles: Value,
}

pub struct Input<'a> {
    /// `theme.json` with the chosen style variations merged in.
    pub theme: &'a Value,
    pub block_styles: &'a [BlockStyle],
    /// URL prefix that `file:./` references resolve against.
    pub asset_base: &'a str,
    /// The theme's own `style.css`, appended as is.
    pub style_css: Option<&'a str>,
}

pub fn compile(input: &Input<'_>) -> String {
    let theme = input.theme;
    let settings = theme.get("settings").unwrap_or(&Value::Null);
    let styles = theme.get("styles").unwrap_or(&Value::Null);
    let ctx = Ctx {
        root: theme,
        asset_base: input.asset_base,
    };
    let mut css = String::new();

    font_faces(&mut css, settings, input.asset_base);
    root_variables(&mut css, settings, styles, &ctx);
    css.push_str(LAYOUT_CSS);
    utility_classes(&mut css, settings);

    // Global styles: the body, then elements, then blocks.
    if let Some(styles) = styles.as_object() {
        let body = declarations(styles, &ctx, Scope::Body);
        rule(&mut css, "body", &body);
        if let Some(elements) = styles.get("elements").and_then(Value::as_object) {
            compile_elements(&mut css, &[], elements, &ctx);
        }
        if let Some(blocks) = styles.get("blocks").and_then(Value::as_object) {
            compile_blocks(&mut css, &[], blocks, &ctx);
        }
    }

    for style in input.block_styles {
        let selector = vec![format!(".is-style-{}", style.slug)];
        if let Some(node) = style.styles.as_object() {
            compile_node(&mut css, &selector, node, &ctx);
        }
    }

    if let Some(style_css) = input.style_css {
        css.push_str(style_css);
        css.push('\n');
    }
    if let Some(custom) = styles.get("css").and_then(Value::as_str) {
        css.push_str(custom);
        css.push('\n');
    }
    css
}

struct Ctx<'a> {
    root: &'a Value,
    asset_base: &'a str,
}

#[derive(Clone, Copy, PartialEq)]
enum Scope {
    /// The top-level `styles`: padding becomes root padding, not body padding.
    Body,
    Other,
}

// ---------------------------------------------------------------------------
// Presets and custom properties
// ---------------------------------------------------------------------------

const DEFAULT_PALETTE: &[(&str, &str)] = &[
    ("black", "#000000"),
    ("cyan-bluish-gray", "#abb8c3"),
    ("white", "#ffffff"),
    ("pale-pink", "#f78da7"),
    ("vivid-red", "#cf2e2e"),
    ("luminous-vivid-orange", "#ff6900"),
    ("luminous-vivid-amber", "#fcb900"),
    ("light-green-cyan", "#7bdcb5"),
    ("vivid-green-cyan", "#00d084"),
    ("pale-cyan-blue", "#8ed1fc"),
    ("vivid-cyan-blue", "#0693e3"),
    ("vivid-purple", "#9b51e0"),
];

const DEFAULT_FONT_SIZES: &[(&str, &str)] = &[
    ("small", "13px"),
    ("medium", "20px"),
    ("large", "36px"),
    ("x-large", "42px"),
];

const DEFAULT_SPACING_SIZES: &[(&str, &str)] = &[
    ("20", "0.44rem"),
    ("30", "0.67rem"),
    ("40", "1rem"),
    ("50", "1.5rem"),
    ("60", "2.25rem"),
    ("70", "3.38rem"),
    ("80", "5.06rem"),
];

fn root_variables(css: &mut String, settings: &Value, styles: &Value, ctx: &Ctx<'_>) {
    let mut vars: Vec<(String, String)> = Vec::new();

    let enabled = |path: &[&str]| lookup(settings, path).and_then(Value::as_bool) != Some(false);

    if enabled(&["color", "defaultPalette"]) {
        for (slug, color) in DEFAULT_PALETTE {
            vars.push((format!("--wp--preset--color--{slug}"), (*color).into()));
        }
    }
    for preset in presets(settings, &["color", "palette"]) {
        if let (Some(slug), Some(color)) = (str_field(preset, "slug"), str_field(preset, "color")) {
            vars.push((format!("--wp--preset--color--{slug}"), color.into()));
        }
    }
    for preset in presets(settings, &["color", "gradients"]) {
        if let (Some(slug), Some(gradient)) =
            (str_field(preset, "slug"), str_field(preset, "gradient"))
        {
            vars.push((format!("--wp--preset--gradient--{slug}"), gradient.into()));
        }
    }

    if enabled(&["typography", "defaultFontSizes"]) {
        for (slug, size) in DEFAULT_FONT_SIZES {
            vars.push((format!("--wp--preset--font-size--{slug}"), (*size).into()));
        }
    }
    let fluid = FluidSettings::from(settings);
    for preset in presets(settings, &["typography", "fontSizes"]) {
        if let (Some(slug), Some(size)) = (str_field(preset, "slug"), preset.get("size")) {
            let size = value_string(size);
            let value = fluid.font_size(&size, preset.get("fluid"));
            vars.push((format!("--wp--preset--font-size--{slug}"), value));
        }
    }
    for preset in presets(settings, &["typography", "fontFamilies"]) {
        if let (Some(slug), Some(family)) =
            (str_field(preset, "slug"), str_field(preset, "fontFamily"))
        {
            vars.push((format!("--wp--preset--font-family--{slug}"), family.into()));
        }
    }

    if enabled(&["spacing", "defaultSpacingSizes"]) {
        for (slug, size) in DEFAULT_SPACING_SIZES {
            vars.push((format!("--wp--preset--spacing--{slug}"), (*size).into()));
        }
    }
    for preset in presets(settings, &["spacing", "spacingSizes"]) {
        if let (Some(slug), Some(size)) = (str_field(preset, "slug"), preset.get("size")) {
            vars.push((format!("--wp--preset--spacing--{slug}"), value_string(size)));
        }
    }
    for preset in presets(settings, &["shadow", "presets"]) {
        if let (Some(slug), Some(shadow)) = (str_field(preset, "slug"), str_field(preset, "shadow"))
        {
            vars.push((format!("--wp--preset--shadow--{slug}"), shadow.into()));
        }
    }

    if let Some(custom) = settings.get("custom") {
        custom_properties(&mut vars, "--wp--custom", custom);
    }

    if let Some(size) = lookup(settings, &["layout", "contentSize"]).and_then(Value::as_str) {
        vars.push(("--wp--style--global--content-size".into(), size.into()));
    }
    if let Some(size) = lookup(settings, &["layout", "wideSize"]).and_then(Value::as_str) {
        vars.push(("--wp--style--global--wide-size".into(), size.into()));
    }

    // Root padding is always declared, so the layout rules can rely on it.
    let padding = lookup(styles, &["spacing", "padding"]);
    for side in ["top", "right", "bottom", "left"] {
        let value = match padding {
            Some(Value::Object(sides)) => sides.get(side).map(|v| resolve(v, ctx)),
            Some(Value::String(all)) => Some(resolve(&Value::String(all.clone()), ctx)),
            _ => None,
        };
        vars.push((
            format!("--wp--style--root--padding-{side}"),
            value.unwrap_or_else(|| "0px".into()),
        ));
    }
    let gap =
        lookup(styles, &["spacing", "blockGap"]).map_or_else(|| "24px".into(), |v| resolve(v, ctx));
    vars.push(("--wp--style--block-gap".into(), gap));

    css.push_str(":root {\n");
    for (name, value) in vars {
        let _ = writeln!(css, "  {name}: {value};");
    }
    css.push_str("}\n");
}

fn custom_properties(vars: &mut Vec<(String, String)>, prefix: &str, value: &Value) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                custom_properties(vars, &format!("{prefix}--{}", kebab(key)), value);
            }
        }
        Value::Null | Value::Array(_) => {}
        other => vars.push((prefix.to_owned(), value_string(other))),
    }
}

fn utility_classes(css: &mut String, settings: &Value) {
    for preset in presets(settings, &["color", "palette"]) {
        let Some(slug) = str_field(preset, "slug") else {
            continue;
        };
        let var = format!("var(--wp--preset--color--{slug})");
        let _ = writeln!(
            css,
            ".has-{slug}-color {{ color: {var} !important; }}\n.has-{slug}-background-color {{ \
             background-color: {var} !important; }}\n.has-{slug}-border-color {{ border-color: \
             {var} !important; }}"
        );
    }
    for preset in presets(settings, &["color", "gradients"]) {
        let Some(slug) = str_field(preset, "slug") else {
            continue;
        };
        let _ = writeln!(
            css,
            ".has-{slug}-gradient-background {{ background: var(--wp--preset--gradient--{slug}) \
             !important; }}"
        );
    }
    for preset in presets(settings, &["typography", "fontSizes"]) {
        let Some(slug) = str_field(preset, "slug") else {
            continue;
        };
        let _ = writeln!(
            css,
            ".has-{slug}-font-size {{ font-size: var(--wp--preset--font-size--{slug}) !important; \
             }}"
        );
    }
    for preset in presets(settings, &["typography", "fontFamilies"]) {
        let Some(slug) = str_field(preset, "slug") else {
            continue;
        };
        let _ = writeln!(
            css,
            ".has-{slug}-font-family {{ font-family: var(--wp--preset--font-family--{slug}) \
             !important; }}"
        );
    }
}

/// Fluid typography, as `wp_get_typography_font_size_value` computes it.
struct FluidSettings {
    enabled: bool,
    min_viewport_rem: f64,
    max_viewport_rem: f64,
    min_font_size_rem: f64,
}

impl FluidSettings {
    fn from(settings: &Value) -> Self {
        let fluid = lookup(settings, &["typography", "fluid"]);
        let wide = lookup(settings, &["layout", "wideSize"])
            .and_then(Value::as_str)
            .and_then(to_rem);
        let option = |key: &str| {
            fluid
                .and_then(|f| f.get(key))
                .and_then(Value::as_str)
                .and_then(to_rem)
        };
        Self {
            enabled: matches!(fluid, Some(Value::Bool(true) | Value::Object(_))),
            min_viewport_rem: option("minViewportWidth").unwrap_or(20.0),
            max_viewport_rem: option("maxViewportWidth").or(wide).unwrap_or(100.0),
            min_font_size_rem: option("minFontSize").unwrap_or(0.875),
        }
    }

    fn font_size(&self, size: &str, preset_fluid: Option<&Value>) -> String {
        if !self.enabled || matches!(preset_fluid, Some(Value::Bool(false))) {
            return size.to_owned();
        }
        let explicit = |key| {
            preset_fluid
                .and_then(|f| f.get(key))
                .and_then(Value::as_str)
                .and_then(to_rem)
        };
        let Some(preferred) = to_rem(size) else {
            return size.to_owned();
        };
        let max = explicit("max").unwrap_or(preferred);
        let min = if let Some(min) = explicit("min") {
            min
        } else {
            if preferred <= self.min_font_size_rem {
                return size.to_owned();
            }
            let px = preferred * 16.0;
            let factor = (1.0 - 0.075 * px.log2()).clamp(0.25, 0.75);
            (preferred * factor).max(self.min_font_size_rem)
        };
        if min >= max {
            return size.to_owned();
        }
        let factor = 100.0 * ((max - min) / (self.max_viewport_rem - self.min_viewport_rem));
        let vw = self.min_viewport_rem / 100.0;
        format!(
            "clamp({}rem, {}rem + ((1vw - {}rem) * {}), {}rem)",
            round(min),
            round(min),
            round(vw),
            round(factor),
            round(max)
        )
    }
}

fn to_rem(value: &str) -> Option<f64> {
    let value = value.trim();
    if let Some(n) = value.strip_suffix("rem") {
        n.trim().parse().ok()
    } else if let Some(n) = value.strip_suffix("px") {
        n.trim().parse::<f64>().ok().map(|px| px / 16.0)
    } else {
        None
    }
}

fn round(value: f64) -> String {
    let rounded = (value * 1000.0).round() / 1000.0;
    format!("{rounded}")
}

// ---------------------------------------------------------------------------
// Fonts
// ---------------------------------------------------------------------------

fn font_faces(css: &mut String, settings: &Value, asset_base: &str) {
    for family in presets(settings, &["typography", "fontFamilies"]) {
        let Some(faces) = family.get("fontFace").and_then(Value::as_array) else {
            continue;
        };
        for face in faces {
            let Some(name) = str_field(face, "fontFamily") else {
                continue;
            };
            let sources: Vec<&str> = match face.get("src") {
                Some(Value::String(src)) => vec![src.as_str()],
                Some(Value::Array(srcs)) => srcs.iter().filter_map(Value::as_str).collect(),
                _ => continue,
            };
            let src = sources
                .iter()
                .map(|src| {
                    let url = asset_url(src, asset_base);
                    match font_format(src) {
                        Some(format) => format!("url('{url}') format('{format}')"),
                        None => format!("url('{url}')"),
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            let name = if name.starts_with(['"', '\'']) {
                name.to_owned()
            } else {
                format!("\"{name}\"")
            };
            let _ = write!(
                css,
                "@font-face {{ font-family: {name}; src: {src}; font-display: fallback;"
            );
            for (key, property) in [
                ("fontStyle", "font-style"),
                ("fontWeight", "font-weight"),
                ("fontStretch", "font-stretch"),
                ("unicodeRange", "unicode-range"),
            ] {
                if let Some(value) = face.get(key) {
                    let _ = write!(css, " {property}: {};", value_string(value));
                }
            }
            css.push_str(" }\n");
        }
    }
}

fn font_format(src: &str) -> Option<&'static str> {
    let extension = src.rsplit('.').next()?.to_lowercase();
    Some(match extension.as_str() {
        "woff2" => "woff2",
        "woff" => "woff",
        "ttf" => "truetype",
        "otf" => "opentype",
        _ => return None,
    })
}

fn asset_url(src: &str, asset_base: &str) -> String {
    match src.strip_prefix("file:") {
        Some(path) => format!("{asset_base}/{}", path.trim_start_matches("./")),
        None => src.to_owned(),
    }
}

// ---------------------------------------------------------------------------
// Element, block and variation styles
// ---------------------------------------------------------------------------

fn element_selectors(element: &str) -> Option<&'static [&'static str]> {
    Some(match element {
        "link" => &["a:where(:not(.wp-element-button))"],
        "heading" => &["h1", "h2", "h3", "h4", "h5", "h6"],
        "h1" => &["h1"],
        "h2" => &["h2"],
        "h3" => &["h3"],
        "h4" => &["h4"],
        "h5" => &["h5"],
        "h6" => &["h6"],
        "button" => &[".wp-element-button", ".wp-block-button__link"],
        "caption" => &[
            ".wp-element-caption",
            "figcaption",
            // AsciiDoc block titles play the part of captions.
            ".imageblock > .title",
            ".listingblock > .title",
            ".exampleblock > .title",
            "table.tableblock > caption",
        ],
        "cite" => &["cite", ".attribution"],
        _ => return None,
    })
}

/// The selector a block's styles apply to, plus the `AsciiDoc` markup that
/// plays the same part.
fn block_selectors(block: &str) -> Vec<String> {
    let aliases: &[&str] = match block {
        "core/paragraph" => return vec!["p".into()],
        "core/heading" => {
            return ["h1", "h2", "h3", "h4", "h5", "h6"]
                .map(String::from)
                .to_vec();
        }
        "core/list" => &[".wp-block-list", ".ulist > ul", ".olist > ol"],
        "core/code" => &[".wp-block-code", ".listingblock pre", ".literalblock pre"],
        "core/quote" => &[".wp-block-quote", ".quoteblock"],
        "core/separator" => &[".wp-block-separator", "hr"],
        "core/table" => &[".wp-block-table > table", "table.tableblock"],
        "core/image" => &[".wp-block-image", ".imageblock"],
        "core/button" => &[".wp-block-button .wp-block-button__link"],
        _ => &[],
    };
    let class = match block.strip_prefix("core/") {
        Some(name) => format!(".wp-block-{name}"),
        None => format!(".wp-block-{}", block.replace('/', "-")),
    };
    let mut selectors = vec![class];
    selectors.extend(aliases.iter().map(|s| (*s).to_owned()));
    selectors.dedup();
    selectors
}

const PSEUDO_CLASSES: &[&str] = &[
    ":hover",
    ":focus",
    ":focus-visible",
    ":focus-within",
    ":active",
    ":visited",
    ":link",
    ":any-link",
];

fn compile_elements(
    css: &mut String,
    scope: &[String],
    elements: &Map<String, Value>,
    ctx: &Ctx<'_>,
) {
    for (element, node) in elements {
        let (Some(selectors), Some(node)) = (element_selectors(element), node.as_object()) else {
            continue;
        };
        let selectors: Vec<String> = descend(scope, selectors);
        let decls = declarations(node, ctx, Scope::Other);
        rule(css, &where_root(&selectors), &decls);
        for pseudo in PSEUDO_CLASSES {
            if let Some(state) = node.get(*pseudo).and_then(Value::as_object) {
                let with_state: Vec<String> =
                    selectors.iter().map(|s| format!("{s}{pseudo}")).collect();
                rule(
                    css,
                    &where_root(&with_state),
                    &declarations(state, ctx, Scope::Other),
                );
            }
        }
        if let Some(custom) = node.get("css").and_then(Value::as_str) {
            custom_css(css, &selectors, custom);
        }
    }
}

fn compile_blocks(css: &mut String, scope: &[String], blocks: &Map<String, Value>, ctx: &Ctx<'_>) {
    for (block, node) in blocks {
        let Some(node) = node.as_object() else {
            continue;
        };
        let own = block_selectors(block);
        let own: Vec<&str> = own.iter().map(String::as_str).collect();
        let selectors = descend(scope, &own);
        compile_node(css, &selectors, node, ctx);
    }
}

/// Styles for one selector: its declarations, nested elements, nested blocks
/// and custom CSS.
fn compile_node(css: &mut String, selectors: &[String], node: &Map<String, Value>, ctx: &Ctx<'_>) {
    rule(
        css,
        &where_root(selectors),
        &declarations(node, ctx, Scope::Other),
    );
    if let Some(elements) = node.get("elements").and_then(Value::as_object) {
        compile_elements(css, selectors, elements, ctx);
    }
    if let Some(blocks) = node.get("blocks").and_then(Value::as_object) {
        compile_blocks(css, selectors, blocks, ctx);
    }
    if let Some(custom) = node.get("css").and_then(Value::as_str) {
        custom_css(css, selectors, custom);
    }
}

/// Each selector in `scope` combined with each in `inner` as a descendant.
fn descend(scope: &[String], inner: &[&str]) -> Vec<String> {
    if scope.is_empty() {
        return inner.iter().map(|s| (*s).to_owned()).collect();
    }
    scope
        .iter()
        .flat_map(|outer| inner.iter().map(move |inner| format!("{outer} {inner}")))
        .collect()
}

fn where_root(selectors: &[String]) -> String {
    format!(":root :where({})", selectors.join(", "))
}

/// A block's `css` property, with `&` standing for the block's selector, as
/// `WP_Theme_JSON::process_blocks_custom_css` interprets it.
fn custom_css(css: &mut String, selectors: &[String], custom: &str) {
    for part in custom.split('&') {
        if !part.contains('{') {
            let part = part.trim();
            if !part.is_empty() {
                let _ = writeln!(css, "{} {{ {part} }}", where_root(selectors));
            }
            continue;
        }
        let part = part.replace('}', "");
        let Some((nested, body)) = part.split_once('{') else {
            continue;
        };
        let combined: Vec<String> = if nested.starts_with(' ') {
            selectors
                .iter()
                .flat_map(|outer| {
                    nested
                        .split(',')
                        .map(move |inner| format!("{outer} {}", inner.trim()))
                })
                .collect()
        } else {
            selectors
                .iter()
                .flat_map(|outer| {
                    nested
                        .split(',')
                        .map(move |inner| format!("{outer}{}", inner.trim()))
                })
                .collect()
        };
        let _ = writeln!(css, "{} {{ {} }}", where_root(&combined), body.trim());
    }
}

fn rule(css: &mut String, selector: &str, decls: &[(String, String)]) {
    if decls.is_empty() {
        return;
    }
    let _ = write!(css, "{selector} {{");
    for (property, value) in decls {
        let _ = write!(css, " {property}: {value};");
    }
    css.push_str(" }\n");
}

/// The CSS declarations for one style node.
fn declarations(node: &Map<String, Value>, ctx: &Ctx<'_>, scope: Scope) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let push = |out: &mut Vec<(String, String)>, property: &str, value: Option<&Value>| {
        if let Some(value) = value
            && (!value.is_object() || value.get("ref").is_some())
        {
            out.push((property.to_owned(), resolve(value, ctx)));
        }
    };

    if let Some(color) = node.get("color") {
        push(&mut out, "color", color.get("text"));
        push(&mut out, "background-color", color.get("background"));
        push(&mut out, "background", color.get("gradient"));
    }
    if let Some(background) = node.get("background") {
        if let Some(image) = background.get("backgroundImage") {
            let url = image.get("url").and_then(Value::as_str).or(image.as_str());
            if let Some(url) = url {
                let url = asset_url(url, ctx.asset_base);
                out.push(("background-image".into(), format!("url('{url}')")));
            }
        }
        for (key, property) in [
            ("backgroundSize", "background-size"),
            ("backgroundPosition", "background-position"),
            ("backgroundRepeat", "background-repeat"),
            ("backgroundAttachment", "background-attachment"),
        ] {
            push(&mut out, property, background.get(key));
        }
    }
    if let Some(typography) = node.get("typography").and_then(Value::as_object) {
        for (key, value) in typography {
            let property = match key.as_str() {
                "textColumns" => "column-count".to_owned(),
                "fontSize" | "fontFamily" | "fontStyle" | "fontWeight" | "letterSpacing"
                | "lineHeight" | "textDecoration" | "textTransform" | "textAlign"
                | "writingMode" | "fontStretch" => kebab(key),
                _ => continue,
            };
            push(&mut out, &property, Some(value));
        }
    }
    if let Some(spacing) = node.get("spacing") {
        if scope == Scope::Other {
            box_property(&mut out, "padding", spacing.get("padding"), ctx);
        }
        box_property(&mut out, "margin", spacing.get("margin"), ctx);
        if scope == Scope::Other
            && let Some(gap) = spacing.get("blockGap")
            && !gap.is_object()
        {
            out.push(("gap".into(), resolve(gap, ctx)));
        }
    }
    if let Some(border) = node.get("border").and_then(Value::as_object) {
        for key in ["color", "style", "width"] {
            if let Some(value) = border.get(key) {
                out.push((format!("border-{key}"), resolve(value, ctx)));
            }
        }
        match border.get("radius") {
            Some(Value::Object(corners)) => {
                for (corner, value) in corners {
                    out.push((
                        format!("border-{}-radius", kebab(corner)),
                        resolve(value, ctx),
                    ));
                }
            }
            Some(radius) => out.push(("border-radius".into(), resolve(radius, ctx))),
            None => {}
        }
        for side in ["top", "right", "bottom", "left"] {
            if let Some(Value::Object(props)) = border.get(side) {
                for (key, value) in props {
                    out.push((format!("border-{side}-{key}"), resolve(value, ctx)));
                }
            }
        }
    }
    if let Some(outline) = node.get("outline").and_then(Value::as_object) {
        for (key, value) in outline {
            out.push((format!("outline-{}", kebab(key)), resolve(value, ctx)));
        }
    }
    if let Some(shadow) = node.get("shadow") {
        out.push(("box-shadow".into(), resolve(shadow, ctx)));
    }
    if let Some(dimensions) = node.get("dimensions").and_then(Value::as_object) {
        for (key, value) in dimensions {
            out.push((kebab(key), resolve(value, ctx)));
        }
    }
    out
}

fn box_property(
    out: &mut Vec<(String, String)>,
    property: &str,
    value: Option<&Value>,
    ctx: &Ctx<'_>,
) {
    match value {
        Some(Value::Object(sides)) => {
            for side in ["top", "right", "bottom", "left"] {
                if let Some(value) = sides.get(side) {
                    out.push((format!("{property}-{side}"), resolve(value, ctx)));
                }
            }
        }
        Some(value) => out.push((property.to_owned(), resolve(value, ctx))),
        None => {}
    }
}

/// Turn a theme.json value into CSS: `var:preset|color|base` becomes
/// `var(--wp--preset--color--base)`, `{"ref": "styles.x"}` is looked up.
fn resolve(value: &Value, ctx: &Ctx<'_>) -> String {
    if let Some(reference) = value.get("ref").and_then(Value::as_str) {
        let path: Vec<&str> = reference.split('.').collect();
        return match lookup(ctx.root, &path) {
            Some(target) if target.get("ref").is_none() => resolve(target, ctx),
            _ => "initial".into(),
        };
    }
    let text = value_string(value);
    match text.strip_prefix("var:") {
        Some(path) => format!(
            "var(--wp--{})",
            path.split('|').map(kebab).collect::<Vec<_>>().join("--")
        ),
        None => text,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn lookup<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(value, |value, key| value.get(*key))
}

fn presets<'a>(settings: &'a Value, path: &[&str]) -> impl Iterator<Item = &'a Value> {
    lookup(settings, path)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn value_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `fontSize` → `font-size`, `topLeft` → `top-left`.
fn kebab(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for c in name.chars() {
        if c.is_ascii_uppercase() {
            if !out.is_empty() {
                out.push('-');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// Deep-merge `overlay` into `base`: objects merge, everything else replaces.
pub fn merge(base: &mut Value, overlay: &Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(key) {
                    Some(existing) => merge(existing, value),
                    None => {
                        base.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (base, overlay) => *base = overlay.clone(),
    }
}

/// The CSS for the value at `path` in `theme`, if there is one.
pub fn css_at(theme: &Value, path: &[&str]) -> Option<String> {
    let ctx = Ctx {
        root: theme,
        asset_base: "",
    };
    lookup(theme, path).map(|value| resolve(value, &ctx))
}

/// The literal colour a CSS value stands for, following one level of
/// `var(--wp--preset--color--x)` into the palette.
pub fn literal_color<'a>(theme: &'a Value, css: &'a str) -> Option<&'a str> {
    let Some(slug) = css
        .strip_prefix("var(--wp--preset--color--")
        .and_then(|rest| rest.strip_suffix(')'))
    else {
        return Some(css);
    };
    presets(theme.get("settings")?, &["color", "palette"])
        .find(|preset| str_field(preset, "slug") == Some(slug))
        .and_then(|preset| str_field(preset, "color"))
}

/// The layout rules `WordPress` emits with every block theme's global styles.
const LAYOUT_CSS: &str = r"
:where(body) { margin: 0; }
.wp-site-blocks { padding-top: var(--wp--style--root--padding-top); padding-bottom: var(--wp--style--root--padding-bottom); }
.has-global-padding { padding-right: var(--wp--style--root--padding-right); padding-left: var(--wp--style--root--padding-left); }
.has-global-padding > .alignfull { margin-right: calc(var(--wp--style--root--padding-right) * -1); margin-left: calc(var(--wp--style--root--padding-left) * -1); }
.has-global-padding :where(:not(.alignfull.is-layout-flow) > .has-global-padding:not(.wp-block-block, .alignfull)) { padding-right: 0; padding-left: 0; }
.has-global-padding :where(:not(.alignfull.is-layout-flow) > .has-global-padding:not(.wp-block-block, .alignfull)) > .alignfull { margin-left: 0; margin-right: 0; }
:where(.wp-site-blocks) > * { margin-block-start: var(--wp--style--block-gap); margin-block-end: 0; }
:where(.wp-site-blocks) > :first-child { margin-block-start: 0; }
:where(.wp-site-blocks) > :last-child { margin-block-end: 0; }
:root :where(.is-layout-flow, .is-layout-constrained) > * { margin-block-start: var(--wp--style--block-gap); margin-block-end: 0; }
:root :where(.is-layout-flow, .is-layout-constrained) > :first-child { margin-block-start: 0; }
:root :where(.is-layout-flow, .is-layout-constrained) > :last-child { margin-block-end: 0; }
:root :where(.is-layout-flex, .is-layout-grid) { gap: var(--wp--style--block-gap); }
.is-layout-flow > .alignleft, .is-layout-constrained > .alignleft { float: left; margin-inline-start: 0; margin-inline-end: 2em; }
.is-layout-flow > .alignright, .is-layout-constrained > .alignright { float: right; margin-inline-start: 2em; margin-inline-end: 0; }
.is-layout-flow > .aligncenter, .is-layout-constrained > .aligncenter { margin-left: auto !important; margin-right: auto !important; }
.is-layout-constrained > :where(:not(.alignleft):not(.alignright):not(.alignfull)) { max-width: var(--wp--style--global--content-size); margin-left: auto !important; margin-right: auto !important; }
.is-layout-constrained > .alignwide { max-width: var(--wp--style--global--wide-size); }
body .is-layout-flex { display: flex; }
.is-layout-flex { flex-wrap: wrap; align-items: center; }
.is-layout-flex > :is(*, div) { margin: 0; }
body .is-layout-grid { display: grid; }
.is-layout-grid > :is(*, div) { margin: 0; }
.has-text-align-center { text-align: center; }
.has-text-align-left { text-align: left; }
.has-text-align-right { text-align: right; }
";

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn compile_json(theme: &Value) -> String {
        compile(&Input {
            theme,
            block_styles: &[],
            asset_base: "/_wm/theme/t",
            style_css: None,
        })
    }

    #[test]
    fn presets_become_custom_properties() {
        let css = compile_json(&json!({
            "settings": {
                "color": { "defaultPalette": false, "palette": [{ "slug": "base", "color": "#fff" }] },
                "layout": { "contentSize": "645px", "wideSize": "1340px" },
                "custom": { "lineHeight": { "body": 1.5 } }
            }
        }));
        assert!(css.contains("--wp--preset--color--base: #fff;"), "{css}");
        assert!(!css.contains("vivid-red"), "{css}");
        assert!(css.contains("--wp--style--global--content-size: 645px;"));
        assert!(
            css.contains("--wp--custom--line-height--body: 1.5;"),
            "{css}"
        );
        assert!(css.contains(".has-base-background-color"));
    }

    #[test]
    fn fluid_font_sizes_follow_wordpress() {
        let css = compile_json(&json!({
            "settings": {
                "layout": { "wideSize": "1340px" },
                "typography": {
                    "fluid": true,
                    "defaultFontSizes": false,
                    "fontSizes": [
                        { "slug": "small", "size": "0.875rem", "fluid": false },
                        { "slug": "medium", "size": "1rem", "fluid": { "min": "1rem", "max": "1.125rem" } }
                    ]
                }
            }
        }));
        assert!(
            css.contains("--wp--preset--font-size--small: 0.875rem;"),
            "{css}"
        );
        // 100 * 0.125 / (83.75 - 20) = 0.196
        assert!(
            css.contains(
                "--wp--preset--font-size--medium: clamp(1rem, 1rem + ((1vw - 0.2rem) * 0.196), \
                 1.125rem);"
            ),
            "{css}"
        );
    }

    #[test]
    fn font_faces_point_at_theme_assets() {
        let css = compile_json(&json!({
            "settings": { "typography": { "fontFamilies": [{
                "slug": "fira-code", "fontFamily": "\"Fira Code\", monospace",
                "fontFace": [{ "fontFamily": "Fira Code", "fontWeight": "300 700",
                               "src": ["file:./assets/fonts/fira.woff2"] }]
            }]}}
        }));
        assert!(
            css.contains(
                "@font-face { font-family: \"Fira Code\"; src: \
                 url('/_wm/theme/t/assets/fonts/fira.woff2') format('woff2');"
            ),
            "{css}"
        );
        assert!(css.contains("font-weight: 300 700;"));
    }

    #[test]
    fn styles_resolve_presets_and_nest() {
        let css = compile_json(&json!({
            "styles": {
                "color": { "background": "var:preset|color|base" },
                "spacing": { "padding": { "left": "var:preset|spacing|50" }, "blockGap": "1.2rem" },
                "elements": {
                    "link": { "color": { "text": "currentColor" }, ":hover": { "typography": { "textDecoration": "none" } } },
                    "h1": { "typography": { "fontSize": { "ref": "styles.typography.fontSize" } } }
                },
                "typography": { "fontSize": "2rem" },
                "blocks": {
                    "core/quote": {
                        "border": { "width": "0 0 0 2px" },
                        "elements": { "cite": { "typography": { "fontSize": "small" } } },
                        "css": "&.has-text-align-right { border-width: 0 2px 0 0; }"
                    },
                    "core/list": { "css": "& li{margin-top: 0.5rem;}" }
                }
            }
        }));
        assert!(
            css.contains("body { background-color: var(--wp--preset--color--base);"),
            "{css}"
        );
        assert!(css.contains("--wp--style--root--padding-left: var(--wp--preset--spacing--50);"));
        assert!(css.contains("--wp--style--block-gap: 1.2rem;"));
        assert!(
            css.contains(
                ":root :where(a:where(:not(.wp-element-button))) { color: currentColor; }"
            ),
            "{css}"
        );
        assert!(css.contains(
            ":root :where(a:where(:not(.wp-element-button)):hover) { text-decoration: none; }"
        ));
        assert!(
            css.contains(":root :where(h1) { font-size: 2rem; }"),
            "{css}"
        );
        assert!(
            css.contains(":root :where(.wp-block-quote, .quoteblock) { border-width: 0 0 0 2px; }"),
            "{css}"
        );
        assert!(
            css.contains(", .quoteblock .attribution) { font-size: small; }"),
            "{css}"
        );
        assert!(
            css.contains(
                ":root :where(.wp-block-quote.has-text-align-right, \
                 .quoteblock.has-text-align-right) { border-width: 0 2px 0 0; }"
            ),
            "{css}"
        );
        assert!(
            css.contains(
                ":root :where(.wp-block-list li, .ulist > ul li, .olist > ol li) { margin-top: \
                 0.5rem; }"
            ),
            "{css}"
        );
    }

    #[test]
    fn merge_replaces_arrays_and_merges_objects() {
        let mut base = json!({ "a": { "b": 1, "c": [1, 2] }, "d": 1 });
        merge(&mut base, &json!({ "a": { "c": [3] }, "e": 2 }));
        assert_eq!(base, json!({ "a": { "b": 1, "c": [3] }, "d": 1, "e": 2 }));
    }
}
