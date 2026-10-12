//! `view_image` — show the model an image file, for a model that can see one.
//!
//! `read_file` refuses a PNG, rightly: base64 is not something a model reads,
//! and it costs the context. But a model with `vision` *can* look at the PNG,
//! and an agent building a page has two kinds of image it needs to look at:
//!
//! - **the code's own**: a logo under `public/`, an icon in `src/assets/`,
//!   named by a `path` in the coding scope, exactly as `read_file` names one;
//! - **the application's served images**: what a static directory serves out of
//!   another store, named by the `url` `list_assets` gave — `/img/hero.png` —
//!   so the one name the agent has for an asset is the one it can look at.
//!
//! Both are read through the same [`check_access`](sc_files::check_access) as
//! every other reader, as the run's caller; a URL is resolved by the same
//! [`sc_app::Application::static_dir_for`] and [`sc_app::StaticDir::resolve`] the
//! router serves through, and refused where the router would 404.
//!
//! **The image is sent as a model takes it.** PNG, JPEG, GIF and WebP are the
//! formats both vendors accept. One that is at most [`MAX_SIDE`] pixels on its
//! long side and [`MAX_IMAGE_BYTES`] long is sent as it is; a larger one is
//! scaled to fit and re-encoded (PNG where it has transparency, else JPEG), since
//! a vendor scales it down to about that size anyway and the bytes beyond it are
//! latency and nothing else. An SVG is text, and is read with `read_file`.
//!
//! Offered only to a model with `vision`, in every mode: it reads.

use std::io::Cursor;

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat, ImageReader, Limits, RgbImage};
use sc_agent::{Elidable, TraitContext};
use sc_error::{Error, Result};
use sc_llm::{ImagePart, ToolSpec};
use sc_types::Attrs;
use serde_json::{Value as Json, json};

use super::assets;
use super::check::configured_application;
use crate::files::{ARG_PATH, FileScope, open_at, optional_string_arg};
use crate::table::arguments;

/// An image served by one of the application's static directories.
const ARG_URL: &str = "url";

/// The long side an image is scaled to fit: Anthropic's own scaling point, and
/// within OpenAI's.
pub const MAX_SIDE: u32 = 1568;

/// The most bytes one image is sent as — the cap a `view_app` screenshot has.
pub const MAX_IMAGE_BYTES: usize = 1_500_000;

/// The largest file read at all. Anything bigger is not a picture of a page.
const MAX_FILE_BYTES: u64 = 25_000_000;

/// The largest image decoded, on either side, so a small file that claims to be
/// huge is refused rather than allocated.
const MAX_DECODE_SIDE: u32 = 16_384;

/// The tool one configured scope offers.
pub fn tool_name(scope: &FileScope) -> String {
    format!("view_image_{}", scope.slug())
}

/// The tool: `path` always, `url` where the configuration names an application
/// whose static directories a URL could be in.
pub fn spec(scope: &FileScope, config: &Attrs) -> ToolSpec {
    let mut properties = json!({
        ARG_PATH: {"type": "string", "description": "In this project, e.g. `public/logo.png`"},
    });
    let description = match configured_application(config) {
        Some(application) => {
            if let Some(map) = properties.as_object_mut() {
                map.insert(
                    ARG_URL.to_owned(),
                    json!({"type": "string", "description": "A URL the application serves, as list_assets gives it, e.g. `/img/hero.png`"}),
                );
            }
            format!(
                "Look at a PNG, JPEG, GIF or WebP image: give either a `path` in this project \
                 or a `url` that `{application}` serves. Large images are scaled down before \
                 they are shown. An SVG is text: read it with read_file."
            )
        }
        None => "Look at a PNG, JPEG, GIF or WebP image in this project. Large images are \
                 scaled down before they are shown. An SVG is text: read it with read_file."
            .to_owned(),
    };
    ToolSpec::new(
        tool_name(scope),
        description,
        json!({
            "type": "object",
            "properties": properties,
            "additionalProperties": false,
        }),
    )
}

/// Read one image, as the run's caller, and attach it to the result.
pub async fn call(
    scope: &FileScope,
    config: &Attrs,
    args: &Json,
    ctx: &mut TraitContext<'_>,
) -> Result<Json> {
    let args = arguments(args, &[ARG_PATH, ARG_URL])?;
    let rel = optional_string_arg(&args, ARG_PATH)?;
    let url = optional_string_arg(&args, ARG_URL)?;
    let (named, where_, bytes) = match (rel.is_empty(), url.is_empty()) {
        (false, true) => {
            let (store, path) = open_at(scope, ctx, &rel).await?;
            let bytes = read(store.as_ref(), &path, &rel, scope).await?;
            (rel, String::new(), bytes)
        }
        (true, false) => {
            let (store, path, dir) = open_url(scope, config, &url, ctx).await?;
            let bytes = read(store.as_ref(), &path, &url, scope).await?;
            (url, format!(" (`{path}` in store `{dir}`)"), bytes)
        }
        _ => {
            return Err(Error::invalid(format!(
                "give exactly one of `{ARG_PATH}` (a file in this project) or `{ARG_URL}` (an \
                 image the application serves)"
            )));
        }
    };
    let image = tokio::task::spawn_blocking(move || prepare(&bytes))
        .await
        .map_err(|e| Error::msg(format!("reading the image: {e}")))?
        .map_err(|reason| Error::invalid(format!("`{named}` {reason}")))?;

    let mut out = format!(
        "view_image {named}{where_}\n{}×{} {}, {} KB",
        image.original.0,
        image.original.1,
        image.format,
        image.original_bytes.div_ceil(1024)
    );
    match image.scaled {
        true => out.push_str(&format!(
            ": attached scaled to {}×{} ({} KB {})",
            image.size.0,
            image.size.1,
            image.data.len().div_ceil(1024),
            image.media_type.trim_start_matches("image/").to_uppercase()
        )),
        false => out.push_str(": attached"),
    }
    ctx.attach_image(ImagePart::new(image.media_type, image.data));
    Ok(Json::String(out))
}

/// The store, store path and store name a static-directory URL names, checked
/// as the router checks it — and refused, with one sentence, wherever the router
/// would answer 404.
async fn open_url(
    scope: &FileScope,
    config: &Attrs,
    url: &str,
    ctx: &TraitContext<'_>,
) -> Result<(std::sync::Arc<dyn sc_files::FileStore>, String, String)> {
    let app = assets::application(ctx.catalog, config).await?;
    // The path of an absolute URL, and neither query nor fragment: what the
    // router matches on.
    let path = match url.split_once("://") {
        Some((_, rest)) => rest.find('/').map_or("/", |i| &rest[i..]),
        None => url,
    };
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let not_served = || {
        Error::invalid(format!(
            "`{url}` is not an image `{}` serves; `{}` names the ones it does",
            app.subdomain,
            assets::tool_name(scope)
        ))
    };
    if !path.starts_with('/') {
        return Err(not_served());
    }
    let (dir, rest) = app.static_dir_for(path).ok_or_else(not_served)?;
    if !app.can_access_file_store(&dir.store) {
        return Err(not_served());
    }
    let store_path = dir.resolve(rest).ok_or_else(not_served)?;
    let (store, floor) = assets::dir_scope(dir).connect(ctx.catalog).await?;
    sc_files::check_access(
        store.as_ref(),
        floor,
        &store_path,
        &crate::files::file_caller(ctx.caller),
    )
    .await?;
    Ok((store, store_path, dir.store.0.clone()))
}

/// The file's bytes, refusing a directory, a missing file and one too large to
/// be an image worth sending.
async fn read(
    store: &dyn sc_files::FileStore,
    path: &str,
    named: &str,
    scope: &FileScope,
) -> Result<Vec<u8>> {
    let stat = store
        .stat(path)
        .await?
        .ok_or_else(|| Error::not_found(format!("`{named}` does not exist")))?;
    if stat.is_dir {
        return Err(Error::invalid(format!(
            "`{named}` is a directory; `{}` lists what is in one",
            super::find::tool_name(scope)
        )));
    }
    if stat.size > MAX_FILE_BYTES {
        return Err(Error::invalid(format!(
            "`{named}` is {} MB, too large to look at",
            stat.size / 1_000_000
        )));
    }
    Ok(store.read(path).await?.to_vec())
}

/// An image ready to attach.
#[derive(Debug)]
struct Prepared {
    media_type: &'static str,
    data: Vec<u8>,
    /// The format the file was, for the result line.
    format: &'static str,
    original: (u32, u32),
    original_bytes: usize,
    /// What is attached.
    size: (u32, u32),
    scaled: bool,
}

/// The image as a model takes it: as it is when it is small enough, scaled to
/// fit [`MAX_SIDE`] and within [`MAX_IMAGE_BYTES`] when it is not. The error is
/// the rest of a sentence that starts with the file's name.
fn prepare(bytes: &[u8]) -> std::result::Result<Prepared, String> {
    let format = image::guess_format(bytes).map_err(|_| unreadable(bytes))?;
    let (media_type, label) = match format {
        ImageFormat::Png => ("image/png", "PNG"),
        ImageFormat::Jpeg => ("image/jpeg", "JPEG"),
        ImageFormat::Gif => ("image/gif", "GIF"),
        ImageFormat::WebP => ("image/webp", "WebP"),
        other => {
            return Err(format!(
                "is {other:?}, which a model cannot be shown; PNG, JPEG, GIF and WebP can"
            ));
        }
    };
    let reader = || {
        let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
        let mut limits = Limits::default();
        limits.max_image_width = Some(MAX_DECODE_SIDE);
        limits.max_image_height = Some(MAX_DECODE_SIDE);
        reader.limits(limits);
        reader
    };
    let (width, height) = reader()
        .into_dimensions()
        .map_err(|e| format!("is not a readable {label}: {e}"))?;
    if width.max(height) <= MAX_SIDE && bytes.len() <= MAX_IMAGE_BYTES {
        return Ok(Prepared {
            media_type,
            data: bytes.to_vec(),
            format: label,
            original: (width, height),
            original_bytes: bytes.len(),
            size: (width, height),
            scaled: false,
        });
    }

    let decoded = reader()
        .decode()
        .map_err(|e| format!("is not a readable {label}: {e}"))?;
    let fitted = match width.max(height) > MAX_SIDE {
        true => decoded.resize(MAX_SIDE, MAX_SIDE, FilterType::Triangle),
        false => decoded,
    };
    let size = (fitted.width(), fitted.height());
    let done = |media_type, data| Prepared {
        media_type,
        data,
        format: label,
        original: (width, height),
        original_bytes: bytes.len(),
        size,
        scaled: true,
    };
    // Transparency survives only in a PNG, so it is tried first for an image
    // that uses any — not merely one with an alpha channel, which most PNGs have
    // whether or not a pixel is see-through. Everything else is a JPEG.
    if fitted.color().has_alpha() && fitted.to_rgba8().pixels().any(|p| p.0[3] < 255) {
        let mut png = Vec::new();
        fitted
            .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
            .map_err(|e| format!("could not be re-encoded: {e}"))?;
        if png.len() <= MAX_IMAGE_BYTES {
            return Ok(done("image/png", png));
        }
    }
    let flat = on_white(&fitted);
    for quality in [85, 70, 50] {
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, quality)
            .encode_image(&flat)
            .map_err(|e| format!("could not be re-encoded: {e}"))?;
        if jpeg.len() <= MAX_IMAGE_BYTES {
            return Ok(done("image/jpeg", jpeg));
        }
    }
    Err(format!(
        "is too large to send a model even scaled to {}×{}",
        size.0, size.1
    ))
}

/// Why bytes that are not an image were refused: an SVG is text, and says so.
fn unreadable(bytes: &[u8]) -> String {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]).to_lowercase();
    match head.contains("<svg") {
        true => "is an SVG, which is text: read it with the read_file tool".to_owned(),
        false => "is not a PNG, JPEG, GIF or WebP image".to_owned(),
    }
}

/// The image over a white background, for a format with no transparency: a
/// transparent pixel dropped to its colour channels is usually black.
fn on_white(image: &DynamicImage) -> RgbImage {
    let rgba = image.to_rgba8();
    RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [r, g, b, a] = rgba.get_pixel(x, y).0;
        let blend =
            |c: u8| ((u16::from(c) * u16::from(a) + 255 * (255 - u16::from(a))) / 255) as u8;
        image::Rgb([blend(r), blend(g), blend(b)])
    })
}

/// An old look is one line: the image is what costs, and the name is enough to
/// look again.
pub fn elide(old: &Elidable<'_>) -> String {
    let named = old
        .content
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("view_image "))
        .map(|rest| rest.split(" (`").next().unwrap_or(rest))
        .unwrap_or("an image");
    format!("[elided image {named}]")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    fn png(width: u32, height: u32, alpha: u8) -> Vec<u8> {
        let image = RgbaImage::from_pixel(width, height, Rgba([200, 30, 30, alpha]));
        let mut out = Vec::new();
        DynamicImage::ImageRgba8(image)
            .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn a_small_image_is_sent_as_it_is() {
        let bytes = png(40, 20, 255);
        let image = prepare(&bytes).unwrap();
        assert_eq!(image.media_type, "image/png");
        assert_eq!(image.data, bytes);
        assert_eq!(image.original, (40, 20));
        assert!(!image.scaled);
    }

    #[test]
    fn a_large_image_is_scaled_to_fit_keeping_its_shape() {
        let image = prepare(&png(4000, 1000, 255)).unwrap();
        assert!(image.scaled);
        assert_eq!(image.original, (4000, 1000));
        assert_eq!(image.size, (MAX_SIDE, 392));
        // Opaque: a JPEG, which is what a photograph costs least as.
        assert_eq!(image.media_type, "image/jpeg");
        let decoded = image::load_from_memory(&image.data).unwrap();
        assert_eq!((decoded.width(), decoded.height()), image.size);
        assert!(image.data.len() <= MAX_IMAGE_BYTES);
    }

    #[test]
    fn transparency_keeps_a_png() {
        let image = prepare(&png(2000, 2000, 128)).unwrap();
        assert_eq!(image.media_type, "image/png");
        assert_eq!(image.size, (MAX_SIDE, MAX_SIDE));
    }

    #[test]
    fn what_is_not_a_viewable_image_says_what_it_is() {
        let svg = br#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg"></svg>"#;
        assert!(prepare(svg).unwrap_err().contains("read_file"));
        assert!(
            prepare(b"hello")
                .unwrap_err()
                .contains("not a PNG, JPEG, GIF or WebP")
        );
        // A BMP is an image, and not one a model is shown.
        let bmp = b"BM\0\0\0\0\0\0\0\0\x36\0\0\0";
        assert!(prepare(bmp).unwrap_err().contains("Bmp"));
    }

    #[test]
    fn transparent_pixels_flatten_to_white() {
        let clear = DynamicImage::ImageRgba8(RgbaImage::from_pixel(1, 1, Rgba([0, 0, 0, 0])));
        assert_eq!(on_white(&clear).get_pixel(0, 0).0, [255, 255, 255]);
    }

    #[test]
    fn an_old_look_is_one_line_naming_what_was_looked_at() {
        let call = sc_llm::ToolCall {
            id: "1".to_owned(),
            name: "view_image_code_web".to_owned(),
            arguments: json!({"url": "/img/hero.png"}),
        };
        let old = Elidable {
            index: 2,
            call: &call,
            content: "view_image /img/hero.png (`images/hero.png` in store `media`)\n\
                      2000×1000 PNG, 40 KB: attached scaled to 1568×784 (30 KB JPEG)",
            images: 1,
            transcript: &[],
            state: None,
        };
        assert_eq!(elide(&old), "[elided image /img/hero.png]");
    }

    #[test]
    fn the_url_is_offered_only_beside_an_application() {
        let scope = FileScope {
            store: "code".to_owned(),
            root: "web".to_owned(),
        };
        let bare = spec(&scope, &Attrs::new());
        assert_eq!(bare.name, "view_image_code_web");
        assert!(bare.parameters["properties"].get(ARG_URL).is_none());
        let config: Attrs = [("application".to_owned(), json!("todo"))]
            .into_iter()
            .collect();
        let with = spec(&scope, &config);
        assert!(with.parameters["properties"].get(ARG_URL).is_some());
        assert!(with.description.contains("`todo`"), "{}", with.description);
    }
}
