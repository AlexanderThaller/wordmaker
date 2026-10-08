//! Responsive images: scaled down copies of uploaded photos, and the
//! `<img>` attributes that let browsers pick them and reserve their space.

use std::{
    cell::Cell,
    collections::HashMap,
    io::BufWriter,
    path::{
        Path,
        PathBuf,
    },
    rc::Rc,
    sync::Mutex,
};

use image::{
    DynamicImage,
    ImageDecoder,
    ImageFormat,
    ImageReader,
    codecs::{
        jpeg::JpegEncoder,
        png::PngEncoder,
    },
    imageops::FilterType,
    metadata::Orientation,
};
use lol_html::{
    RewriteStrSettings,
    element,
    rewrite_str,
};
use tokio::sync::Semaphore;

/// Widths a scaled copy can be requested in. A fixed set keeps the number of
/// files that can be generated, and the work a visitor can cause, bounded.
/// Close enough together that a browser rarely fetches much more than it
/// shows: a phone's content column at 2x lands on 640 or 960, a column of
/// two on 320.
pub const WIDTHS: &[u32] = &[320, 480, 640, 800, 960, 1200, 1600];

/// The page padding on both sides together, roughly: themes put
/// 1.25–2rem (or 30px) on either side of the content on phones. Erring on
/// the narrow side would pick blurry copies; this errs slightly wide.
const GUTTER: &str = "3rem";

/// From this viewport width on, columns sit side by side (as in `WordPress`).
const COLUMNS_FROM: u32 = 782;

/// The gap between columns, roughly: `WordPress`'s default of 2em.
const COLUMN_GAP: u32 = 32;

/// How wide the theme lays content out, in CSS pixels, so `sizes` can say
/// how wide an image will be shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// The content column (`contentSize`).
    pub content: u32,
    /// `alignwide` blocks (`wideSize`).
    pub wide: u32,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            content: 800,
            wide: 1200,
        }
    }
}

/// Where in the page an image sits, as far as its width is concerned.
#[derive(Debug, Default)]
struct Placement {
    columns: Rc<Cell<u32>>,
    wide: Rc<Cell<u32>>,
    full: Rc<Cell<u32>>,
}

impl Placement {
    /// The `sizes` value for an image placed here.
    fn sizes(&self, layout: Layout) -> String {
        if self.full.get() > 0 {
            return "100vw".to_owned();
        }
        let width = if self.wide.get() > 0 {
            layout.wide
        } else {
            layout.content
        };
        if self.columns.get() > 0 {
            // Stacked, one per row, below the breakpoint.
            let column = width.saturating_sub(COLUMN_GAP) / 2;
            return format!("(min-width: {COLUMNS_FROM}px) {column}px, calc(100vw - {GUTTER})");
        }
        format!("(max-width: {width}px) calc(100vw - {GUTTER}), {width}px")
    }
}

/// Counts `depth` up while inside an element, down again at its end tag.
fn track(el: &mut lol_html::html_content::Element<'_, '_>, depth: &Rc<Cell<u32>>) {
    let inside = depth.clone();
    if el
        .on_end_tag(Box::new(move |_| {
            inside.set(inside.get().saturating_sub(1));
            Ok(())
        }))
        .is_ok()
    {
        depth.set(depth.get() + 1);
    }
}

/// Scaled copies live here, below the media directory. Stored media names
/// never start with a dot, so this cannot clash with an upload.
const VARIANTS_DIR: &str = ".variants";

const JPEG_QUALITY: u8 = 80;

/// Image sizes looked up so far; stored media never change, so entries stay
/// valid. Also limits how many images are scaled at the same time.
#[derive(Debug)]
pub struct Images {
    sizes: Mutex<HashMap<String, Option<Size>>>,
    scaling: Semaphore,
}

impl Default for Images {
    fn default() -> Self {
        Self {
            sizes: Mutex::default(),
            scaling: Semaphore::new(2),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Size {
    width: u32,
    height: u32,
    /// Whether scaled copies can be made, which needs a format we encode.
    scalable: bool,
}

impl Images {
    /// The displayed size of a stored image, if it is one.
    fn size(&self, media_dir: &Path, filename: &str) -> Option<Size> {
        let cached = self
            .sizes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(filename)
            .copied();
        if let Some(size) = cached {
            return size;
        }
        // Looked up without holding the lock; at worst two pages read the
        // same header.
        let size = read_size(&media_dir.join(filename));
        self.sizes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(filename.to_owned(), size);
        size
    }

    /// Add size, `srcset` and loading hints to the `<img>` elements of a
    /// rendered page. The first image is likely the largest thing on screen
    /// and is fetched eagerly; every later one waits until it is near.
    pub fn rewrite(&self, html: &str, media_dir: &Path, layout: Layout) -> String {
        let mut seen = 0_usize;
        let placement = Placement::default();
        let rewritten = rewrite_str(
            html,
            RewriteStrSettings::new()
                .append_element_content_handler(element!(".wp-block-column", |el| {
                    track(el, &placement.columns);
                    Ok(())
                }))
                .append_element_content_handler(element!(".alignwide", |el| {
                    track(el, &placement.wide);
                    Ok(())
                }))
                .append_element_content_handler(element!(".alignfull", |el| {
                    track(el, &placement.full);
                    Ok(())
                }))
                .append_element_content_handler(element!("img", |el| {
                    seen += 1;
                    if !el.has_attribute("loading") {
                        if seen == 1 {
                            el.set_attribute("fetchpriority", "high")?;
                        } else {
                            el.set_attribute("loading", "lazy")?;
                        }
                    }
                    if !el.has_attribute("decoding") {
                        el.set_attribute("decoding", "async")?;
                    }

                    let Some(filename) = el
                        .get_attribute("src")
                        .and_then(|src| media_filename(&src).map(str::to_owned))
                    else {
                        return Ok(());
                    };
                    let Some(size) = self.size(media_dir, &filename) else {
                        return Ok(());
                    };
                    // A width or height given in the document wins; setting
                    // only the other one would distort the image.
                    if !el.has_attribute("width") && !el.has_attribute("height") {
                        el.set_attribute("width", &size.width.to_string())?;
                        el.set_attribute("height", &size.height.to_string())?;
                    }
                    if size.scalable && !el.has_attribute("srcset") {
                        let mut candidates: Vec<String> = WIDTHS
                            .iter()
                            .filter(|&&w| w < size.width)
                            .map(|w| format!("/_wm/img/{w}/{filename} {w}w"))
                            .collect();
                        if !candidates.is_empty() {
                            candidates.push(format!("/media/{filename} {}w", size.width));
                            el.set_attribute("srcset", &candidates.join(", "))?;
                            el.set_attribute("sizes", &placement.sizes(layout))?;
                        }
                    }
                    Ok(())
                })),
        );
        rewritten.unwrap_or_else(|err| {
            tracing::warn!(error = %err, "could not rewrite images");
            html.to_owned()
        })
    }

    /// The file to serve for `filename` scaled to `width`, creating it on
    /// first use. Images narrower than `width` are served as they are.
    pub async fn variant(&self, media_dir: &Path, width: u32, filename: &str) -> Option<PathBuf> {
        if !WIDTHS.contains(&width) || media_filename(&format!("/media/{filename}")).is_none() {
            return None;
        }
        let source = media_dir.join(filename);
        let target = media_dir
            .join(VARIANTS_DIR)
            .join(width.to_string())
            .join(filename);
        if tokio::fs::try_exists(&target).await.unwrap_or(false) {
            return Some(target);
        }

        let _permit = self.scaling.acquire().await.ok()?;
        // Someone else may have made it while we waited.
        if tokio::fs::try_exists(&target).await.unwrap_or(false) {
            return Some(target);
        }
        let scaled = {
            let (source, target) = (source.clone(), target.clone());
            tokio::task::spawn_blocking(move || scale(&source, &target, width)).await
        };
        match scaled {
            Ok(Ok(true)) => Some(target),
            Ok(Ok(false)) => Some(source),
            Ok(Err(err)) => {
                if source.is_file() {
                    tracing::warn!(error = %err, filename, width, "could not scale image");
                }
                None
            }
            Err(err) => {
                tracing::error!(error = %err, "image scaling task failed");
                None
            }
        }
    }
}

/// Remove the scaled copies of a deleted file.
pub async fn delete_variants(media_dir: &Path, filename: &str) {
    for width in WIDTHS {
        let path = media_dir
            .join(VARIANTS_DIR)
            .join(width.to_string())
            .join(filename);
        let _ = tokio::fs::remove_file(path).await;
    }
}

/// `x.jpg` for `/media/x.jpg`, as long as it names a stored file directly.
fn media_filename(src: &str) -> Option<&str> {
    let name = src.strip_prefix("/media/")?;
    let plain =
        !name.is_empty() && !name.starts_with('.') && !name.contains(['/', '\\', '?', '#', '%']);
    plain.then_some(name)
}

fn scalable_format(format: ImageFormat) -> bool {
    matches!(format, ImageFormat::Jpeg | ImageFormat::Png)
}

fn read_size(path: &Path) -> Option<Size> {
    let reader = ImageReader::open(path).ok()?.with_guessed_format().ok()?;
    let format = reader.format()?;
    let mut decoder = reader.into_decoder().ok()?;
    let (width, height) = decoder.dimensions();
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let (width, height) = if turns_sideways(orientation) {
        (height, width)
    } else {
        (width, height)
    };
    Some(Size {
        width,
        height,
        scalable: scalable_format(format),
    })
}

fn turns_sideways(orientation: Orientation) -> bool {
    matches!(
        orientation,
        Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH
    )
}

/// Write `source` scaled to `width` to `target`. Returns `false`, writing
/// nothing, when the image is not wider than that already.
fn scale(source: &Path, target: &Path, width: u32) -> image::ImageResult<bool> {
    let reader = ImageReader::open(source)?.with_guessed_format()?;
    let format = reader
        .format()
        .filter(|&f| scalable_format(f))
        .ok_or_else(|| {
            image::ImageError::Unsupported(image::error::UnsupportedError::from(
                image::error::ImageFormatHint::Unknown,
            ))
        })?;
    let mut decoder = reader.into_decoder()?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder)?;
    // Browsers apply the EXIF orientation of the original; the copy has no
    // EXIF data, so it has to be turned already.
    image.apply_orientation(orientation);
    if image.width() <= width {
        return Ok(false);
    }
    let height = (u64::from(image.height()) * u64::from(width) / u64::from(image.width())).max(1);
    let height = u32::try_from(height).unwrap_or(u32::MAX);
    let image = image.resize_exact(width, height, FilterType::CatmullRom);

    let dir = target.parent().unwrap_or(target);
    std::fs::create_dir_all(dir)?;
    // Write next to the target and move it into place, so a visitor never
    // gets a half written file.
    let partial = dir.join(format!(".{}.partial", uuid::Uuid::new_v4().simple()));
    let written = (|| {
        let mut out = BufWriter::new(std::fs::File::create(&partial)?);
        match format {
            ImageFormat::Jpeg => image
                .to_rgb8()
                .write_with_encoder(JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY))?,
            _ => image.write_with_encoder(PngEncoder::new(&mut out))?,
        }
        out.into_inner()
            .map_err(std::io::IntoInnerError::into_error)?;
        std::fs::rename(&partial, target)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    written.map(|()| true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wordmaker-images-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_jpeg(dir: &Path, name: &str, width: u32, height: u32) {
        DynamicImage::new_rgb8(width, height)
            .save_with_format(dir.join(name), ImageFormat::Jpeg)
            .unwrap();
    }

    #[test]
    fn media_filenames() {
        assert_eq!(media_filename("/media/a-b.jpg"), Some("a-b.jpg"));
        assert_eq!(media_filename("/media/../x"), None);
        assert_eq!(media_filename("/media/.variants"), None);
        assert_eq!(media_filename("/media/a.jpg?x=1"), None);
        assert_eq!(media_filename("https://example.com/media/a.jpg"), None);
    }

    #[test]
    fn rewrite_adds_size_srcset_and_loading() {
        let dir = temp_dir();
        write_jpeg(&dir, "big.jpg", 1000, 500);
        let images = Images::default();
        let html = images.rewrite(
            r#"<img src="/media/big.jpg" alt="a"><img src="/elsewhere.png"><img src="/media/big.jpg" width="300">"#,
            &dir,
            Layout::default(),
        );
        assert!(
            html.contains(r#"<img src="/media/big.jpg" alt="a" fetchpriority="high" decoding="async" width="1000" height="500" srcset="/_wm/img/320/big.jpg 320w, /_wm/img/480/big.jpg 480w, /_wm/img/640/big.jpg 640w, /_wm/img/800/big.jpg 800w, /_wm/img/960/big.jpg 960w, /media/big.jpg 1000w" sizes="(max-width: 800px) calc(100vw - 3rem), 800px""#),
            "{html}"
        );
        assert!(
            html.contains(r#"<img src="/elsewhere.png" loading="lazy" decoding="async">"#),
            "{html}"
        );
        // An explicit width is kept and no height is invented.
        assert!(html.contains(r#"width="300" loading="lazy""#), "{html}");
        assert_eq!(html.matches("height=").count(), 1, "{html}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sizes_follow_where_the_image_sits() {
        let dir = temp_dir();
        write_jpeg(&dir, "p.jpg", 1600, 1200);
        let layout = Layout {
            content: 645,
            wide: 1340,
        };
        let html = Images::default().rewrite(
            r#"<div class="openblock wp-block-columns"><div class="content">
<div class="exampleblock wp-block-column"><img src="/media/p.jpg"></div>
</div></div>
<img src="/media/p.jpg">
<div class="imageblock alignwide"><img src="/media/p.jpg"></div>
<div class="imageblock alignfull"><img src="/media/p.jpg"></div>
<img src="/media/p.jpg">"#,
            &dir,
            layout,
        );
        let sizes: Vec<&str> = html
            .split("sizes=\"")
            .skip(1)
            .map(|rest| rest.split('"').next().unwrap())
            .collect();
        assert_eq!(
            sizes,
            [
                "(min-width: 782px) 306px, calc(100vw - 3rem)",
                "(max-width: 645px) calc(100vw - 3rem), 645px",
                "(max-width: 1340px) calc(100vw - 3rem), 1340px",
                "100vw",
                // Leaving the blocks again.
                "(max-width: 645px) calc(100vw - 3rem), 645px",
            ],
            "{html}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn small_images_get_no_srcset() {
        let dir = temp_dir();
        write_jpeg(&dir, "small.jpg", 300, 200);
        let html =
            Images::default().rewrite(r#"<img src="/media/small.jpg">"#, &dir, Layout::default());
        assert!(html.contains(r#"width="300" height="200""#), "{html}");
        assert!(!html.contains("srcset"), "{html}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn variants_are_scaled_and_bounded() {
        let dir = temp_dir();
        write_jpeg(&dir, "photo.jpg", 1000, 500);
        let images = Images::default();

        let path = images.variant(&dir, 480, "photo.jpg").await.unwrap();
        assert_eq!(image::image_dimensions(&path).unwrap(), (480, 240));
        // Second request is served from disk.
        assert_eq!(images.variant(&dir, 480, "photo.jpg").await.unwrap(), path);
        // Not wider than the original: the original is served.
        assert_eq!(
            images.variant(&dir, 1200, "photo.jpg").await.unwrap(),
            dir.join("photo.jpg")
        );

        assert!(images.variant(&dir, 481, "photo.jpg").await.is_none());
        assert!(images.variant(&dir, 480, "missing.jpg").await.is_none());
        assert!(images.variant(&dir, 480, "../photo.jpg").await.is_none());

        delete_variants(&dir, "photo.jpg").await;
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
