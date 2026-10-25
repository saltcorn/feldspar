//! **Image resizing** for a server-rendered site (TODO-astro W2.4): what
//! `<Image>` from `feldspar:components` points at.
//!
//! `<Image src={hero} width={600} />` renders `<img src="/_image?src=/_astro/
//! hero.1a2b3c.jpg&w=600" …>`, and this answers that URL: it decodes the
//! site's own image, resizes it, encodes it and keeps the result, so each
//! variant costs one resize per mount. Only images the build put under
//! `/_astro/` can be resized — what a page imported, named by content hash — so
//! a variant URL can be cached forever and nobody can point the resizer at
//! anything else.
//!
//! Bounded, because the URL is anybody's to type — before the application's
//! admission check, since an image is a file (security review #26): never
//! larger than the image (and never over [`MAX_SIDE`]), at most
//! [`MAX_VARIANTS`] sizes of one image made since the site was mounted —
//! counted when a resize starts, under the same lock as the check, and never
//! given back when a variant is evicted — at most [`MAX_BYTES`] of them kept,
//! and at most [`MAX_RESIZING`] resizes at once, each decoding no more than
//! [`MAX_DECODE_ALLOC`]. A request that waited for a resize is answered from
//! the variant if another request made it meanwhile.

use std::collections::{HashMap, VecDeque};
use std::io::Cursor;
use std::sync::Mutex;

use bytes::Bytes;
use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat, ImageReader, Limits};

use crate::{AppRequest, AppResponse};

/// The path `<Image>`'s variants are served at.
pub const IMAGE_PATH: &str = "/_image";
/// The widest or tallest variant made.
pub const MAX_SIDE: u32 = 4096;
/// The most distinct variants of one image.
pub const MAX_VARIANTS: usize = 32;
/// How many bytes of variants one site keeps. The oldest go first.
pub const MAX_BYTES: usize = 64 << 20;
/// The largest source image decoded, per side.
const MAX_DECODE_SIDE: u32 = 8_192;
/// The most memory one decode may allocate.
pub const MAX_DECODE_ALLOC: u64 = 128 << 20;
/// How many resizes one site runs at once.
pub const MAX_RESIZING: usize = 2;
/// How long a request waits for a resize to be free before it is told to come
/// back.
const RESIZE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// What one variant is asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRequest {
    /// The bundle key of the source (`_astro/hero.1a2b3c.jpg`).
    pub source: String,
    /// The width.
    pub width: u32,
    /// The height, when both were given: the image is cropped to fill.
    pub height: Option<u32>,
    /// The format to encode.
    pub format: ImageFormat,
    /// JPEG quality.
    pub quality: u8,
}

impl ImageRequest {
    /// Read a `/_image` query. The error is a sentence for a 400.
    pub fn parse(req: &AppRequest) -> std::result::Result<ImageRequest, String> {
        let src = req
            .query
            .get("src")
            .ok_or("`src` is missing: name an image under /_astro/")?;
        let source = src.trim_start_matches('/');
        if !source.starts_with("_astro/") || source.contains("..") {
            return Err(format!(
                "`{src}` is not an image the build wrote; only an imported image (under \
                 /_astro/) can be resized"
            ));
        }
        let original = format_of(source)
            .ok_or_else(|| format!("`{src}` is not a PNG, JPEG, GIF or WebP image"))?;
        let side = |name: &str| -> std::result::Result<Option<u32>, String> {
            match req.query.get(name) {
                None => Ok(None),
                Some(v) => v
                    .parse::<u32>()
                    .ok()
                    .filter(|n| (1..=MAX_SIDE).contains(n))
                    .map(Some)
                    .ok_or_else(|| format!("`{name}` must be a number from 1 to {MAX_SIDE}")),
            }
        };
        let width = side("w")?.ok_or("`w` is missing: give the width to resize to")?;
        let height = side("h")?;
        let format = match req.query.get("f").map(|f| f.to_ascii_lowercase()) {
            None => match original {
                // An animation would lose its frames: a resized GIF is a PNG.
                ImageFormat::Gif => ImageFormat::Png,
                other => other,
            },
            Some(f) => match f.as_str() {
                "png" => ImageFormat::Png,
                "jpeg" | "jpg" => ImageFormat::Jpeg,
                "webp" => ImageFormat::WebP,
                _ => return Err(format!("`f={f}` is not png, jpeg or webp")),
            },
        };
        let quality = match req.query.get("q") {
            None => 80,
            Some(q) => q
                .parse::<u8>()
                .ok()
                .filter(|q| (1..=100).contains(q))
                .ok_or("`q` must be a number from 1 to 100")?,
        };
        Ok(ImageRequest {
            source: source.to_owned(),
            width,
            height,
            format,
            quality,
        })
    }

    fn key(&self) -> String {
        format!(
            "{}|{}|{:?}|{:?}|{}",
            self.source, self.width, self.height, self.format, self.quality
        )
    }
}

/// The format a source's extension names, when it is one this resizes.
fn format_of(path: &str) -> Option<ImageFormat> {
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some(ImageFormat::Png),
        "jpg" | "jpeg" => Some(ImageFormat::Jpeg),
        "gif" => Some(ImageFormat::Gif),
        "webp" => Some(ImageFormat::WebP),
        _ => None,
    }
}

fn content_type(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::WebP => "image/webp",
        _ => "image/png",
    }
}

/// Resize `bytes` as `request` asks. Never larger than the source: a width or
/// height past it is the source's, keeping the asked aspect ratio.
pub fn resize(bytes: &[u8], request: &ImageRequest) -> std::result::Result<Vec<u8>, String> {
    let format = format_of(&request.source).ok_or("not an image this resizes")?;
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DECODE_SIDE);
    limits.max_image_height = Some(MAX_DECODE_SIDE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|e| format!("{} could not be read: {e}", request.source))?;
    let (iw, ih) = (image.width(), image.height());
    let resized = match request.height {
        Some(h) => {
            let scale = (f64::from(iw) / f64::from(request.width))
                .min(f64::from(ih) / f64::from(h))
                .min(1.0);
            let w = scaled(request.width, scale);
            let h = scaled(h, scale);
            image.resize_to_fill(w, h, FilterType::Lanczos3)
        }
        None => {
            let w = request.width.min(iw);
            let h = scaled(ih, f64::from(w) / f64::from(iw));
            if (w, h) == (iw, ih) {
                image
            } else {
                image.resize_exact(w, h, FilterType::Lanczos3)
            }
        }
    };
    encode(resized, request)
}

fn scaled(side: u32, scale: f64) -> u32 {
    // At most MAX_DECODE_SIDE, so the cast cannot truncate.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let out = (f64::from(side) * scale).round() as u32;
    out.max(1)
}

fn encode(image: DynamicImage, request: &ImageRequest) -> std::result::Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let failed = |e: image::ImageError| format!("encoding {}: {e}", request.source);
    match request.format {
        ImageFormat::Jpeg => {
            let encoder =
                image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, request.quality);
            DynamicImage::ImageRgb8(image.to_rgb8())
                .write_with_encoder(encoder)
                .map_err(failed)?;
        }
        ImageFormat::WebP => {
            let encoder = image::codecs::webp::WebPEncoder::new_lossless(&mut out);
            DynamicImage::ImageRgba8(image.to_rgba8())
                .write_with_encoder(encoder)
                .map_err(failed)?;
        }
        _ => {
            let encoder = image::codecs::png::PngEncoder::new(&mut out);
            image.write_with_encoder(encoder).map_err(failed)?;
        }
    }
    Ok(out)
}

#[derive(Default)]
struct Store {
    variants: HashMap<String, Bytes>,
    /// Every variant of each source a resize was started for since the site
    /// was mounted. Never shrinks: an evicted variant made again is the same
    /// variant, and one never made is a new one.
    made: HashMap<String, std::collections::HashSet<String>>,
    order: VecDeque<String>,
    bytes: usize,
}

/// One site's resized images, kept.
pub struct SiteImages {
    store: Mutex<Store>,
    resizing: tokio::sync::Semaphore,
}

impl Default for SiteImages {
    fn default() -> Self {
        SiteImages {
            store: Mutex::new(Store::default()),
            resizing: tokio::sync::Semaphore::new(MAX_RESIZING),
        }
    }
}

/// What looking a variant up found.
enum Lookup {
    Kept(Bytes),
    TooMany,
    Make,
}

impl SiteImages {
    /// Answer a `/_image` request from `source`, the bytes of the image it
    /// names (`None` when the bundle has no such file).
    pub async fn serve(&self, req: &AppRequest, source: Option<Bytes>) -> AppResponse {
        let request = match ImageRequest::parse(req) {
            Ok(request) => request,
            Err(message) => return plain(400, &message),
        };
        let Some(source) = source else {
            return plain(404, &format!("/{} is not in this site", request.source));
        };
        let key = request.key();
        let too_many = || {
            plain(
                400,
                &format!(
                    "/{} already has {MAX_VARIANTS} sizes; use fewer distinct widths",
                    request.source
                ),
            )
        };
        let bytes = match self.lookup(&request, &key) {
            Lookup::Kept(bytes) => bytes,
            Lookup::TooMany => return too_many(),
            Lookup::Make => {
                let permit = match tokio::time::timeout(RESIZE_WAIT, self.resizing.acquire()).await
                {
                    Ok(Ok(permit)) => permit,
                    _ => {
                        return plain(503, "the site is resizing other images; try again")
                            .header("retry-after", "5");
                    }
                };
                // Whoever held the permit may have made this very variant.
                match self.lookup(&request, &key) {
                    Lookup::Kept(bytes) => {
                        return AppResponse::ok(content_type(request.format), bytes)
                            .header("cache-control", "public, max-age=31536000, immutable");
                    }
                    Lookup::TooMany => return too_many(),
                    Lookup::Make => {}
                }
                let job = request.clone();
                let made = tokio::task::spawn_blocking(move || resize(&source, &job)).await;
                let bytes = match made {
                    Ok(Ok(bytes)) => Bytes::from(bytes),
                    Ok(Err(message)) => return plain(422, &message),
                    Err(_) => return plain(500, "resizing the image failed"),
                };
                self.keep(key, bytes.clone());
                drop(permit);
                bytes
            }
        };
        AppResponse::ok(content_type(request.format), bytes)
            .header("cache-control", "public, max-age=31536000, immutable")
    }

    /// The kept variant, or whether one may be made: counted as made here,
    /// under the lock the check is made under, so concurrent requests for
    /// distinct sizes cannot all pass it.
    fn lookup(&self, request: &ImageRequest, key: &str) -> Lookup {
        let Ok(mut store) = self.store.lock() else {
            return Lookup::TooMany;
        };
        if let Some(bytes) = store.variants.get(key) {
            return Lookup::Kept(bytes.clone());
        }
        let made = store.made.entry(request.source.clone()).or_default();
        if !made.contains(key) {
            if made.len() >= MAX_VARIANTS {
                return Lookup::TooMany;
            }
            made.insert(key.to_owned());
        }
        Lookup::Make
    }

    fn keep(&self, key: String, bytes: Bytes) {
        let Ok(mut store) = self.store.lock() else {
            return;
        };
        if store.variants.contains_key(&key) {
            return;
        }
        store.bytes += bytes.len();
        store.variants.insert(key.clone(), bytes);
        store.order.push_back(key);
        while store.bytes > MAX_BYTES {
            let Some(old) = store.order.pop_front() else {
                break;
            };
            if let Some(gone) = store.variants.remove(&old) {
                store.bytes -= gone.len();
            }
        }
    }
}

fn plain(status: u16, message: &str) -> AppResponse {
    AppResponse::with_status(
        status,
        "text/plain; charset=utf-8",
        Bytes::from(message.to_owned()),
    )
    .header("cache-control", "no-store")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let image =
            DynamicImage::ImageRgb8(image::RgbImage::from_pixel(w, h, image::Rgb([200, 30, 30])));
        let mut out = Vec::new();
        image
            .write_with_encoder(image::codecs::png::PngEncoder::new(&mut out))
            .unwrap();
        out
    }

    fn ask(query: &[(&str, &str)]) -> AppRequest {
        let mut req = AppRequest::get(IMAGE_PATH);
        for (k, v) in query {
            req.query.insert((*k).to_owned(), (*v).to_owned());
        }
        req
    }

    fn size(bytes: &[u8]) -> (u32, u32) {
        let image = image::load_from_memory(bytes).unwrap();
        (image.width(), image.height())
    }

    #[test]
    fn only_the_builds_own_images_are_resized() {
        for (src, why) in [
            ("/etc/passwd", "not an image the build wrote"),
            ("/_astro/../x.png", "not an image the build wrote"),
            ("/_astro/logo.svg", "not a PNG, JPEG, GIF or WebP"),
        ] {
            let err = ImageRequest::parse(&ask(&[("src", src), ("w", "10")])).unwrap_err();
            assert!(err.contains(why), "{src}: {err}");
        }
        let err = ImageRequest::parse(&ask(&[("src", "/_astro/a.png"), ("w", "0")])).unwrap_err();
        assert!(err.contains("from 1 to"), "{err}");
        let ok = ImageRequest::parse(&ask(&[("src", "/_astro/a.gif"), ("w", "10")])).unwrap();
        assert_eq!(ok.format, ImageFormat::Png, "a resized GIF is a PNG");
    }

    #[test]
    fn a_width_keeps_the_aspect_and_both_crop_and_nothing_grows() {
        let source = png(400, 200);
        let req = |q: &[(&str, &str)]| ImageRequest::parse(&ask(q)).unwrap();
        let out = resize(&source, &req(&[("src", "/_astro/a.png"), ("w", "100")])).unwrap();
        assert_eq!(size(&out), (100, 50));
        let out = resize(
            &source,
            &req(&[("src", "/_astro/a.png"), ("w", "100"), ("h", "100")]),
        )
        .unwrap();
        assert_eq!(size(&out), (100, 100), "cropped to fill");
        let out = resize(&source, &req(&[("src", "/_astro/a.png"), ("w", "4000")])).unwrap();
        assert_eq!(size(&out), (400, 200), "never larger than the source");
        let out = resize(
            &source,
            &req(&[("src", "/_astro/a.png"), ("w", "100"), ("f", "webp")]),
        )
        .unwrap();
        assert_eq!(image::guess_format(&out).unwrap(), ImageFormat::WebP);
        let out = resize(
            &source,
            &req(&[
                ("src", "/_astro/a.png"),
                ("w", "100"),
                ("f", "jpeg"),
                ("q", "50"),
            ]),
        )
        .unwrap();
        assert_eq!(image::guess_format(&out).unwrap(), ImageFormat::Jpeg);
    }

    #[tokio::test]
    async fn a_variant_is_made_once_and_kept() {
        let images = SiteImages::default();
        let source = Bytes::from(png(64, 64));
        let req = ask(&[("src", "/_astro/a.png"), ("w", "32")]);
        let first = images.serve(&req, Some(source.clone())).await;
        assert_eq!(first.status, 200);
        assert_eq!(first.content_type, "image/png");
        assert!(
            first
                .headers
                .iter()
                .any(|(k, v)| k == "cache-control" && v.contains("immutable"))
        );
        assert_eq!(size(&first.body), (32, 32));
        // Kept: answered without the source this time.
        let again = images.serve(&req, Some(Bytes::new())).await;
        assert_eq!(again.body, first.body);
        let missing = images.serve(&req, None).await;
        assert_eq!(missing.status, 404);
    }

    #[tokio::test]
    async fn concurrent_distinct_sizes_cannot_pass_the_variant_cap() {
        let images = std::sync::Arc::new(SiteImages::default());
        let source = Bytes::from(png(256, 256));
        let mut tasks = Vec::new();
        for w in 1..=(MAX_VARIANTS as u32 + 8) {
            let (images, source) = (images.clone(), source.clone());
            tasks.push(tokio::spawn(async move {
                let w = w.to_string();
                let req = ask(&[("src", "/_astro/a.png"), ("w", w.as_str())]);
                images.serve(&req, Some(source)).await.status
            }));
        }
        let mut statuses = Vec::new();
        for t in tasks {
            statuses.push(t.await.unwrap());
        }
        assert_eq!(statuses.iter().filter(|s| **s == 200).count(), MAX_VARIANTS);
        assert_eq!(statuses.iter().filter(|s| **s == 400).count(), 8);
        // A variant made before is still served; a new one is not, even once
        // the kept ones are evicted.
        images.store.lock().unwrap().variants.clear();
        let old = ask(&[("src", "/_astro/a.png"), ("w", "1")]);
        assert_eq!(images.serve(&old, Some(source.clone())).await.status, 200);
        let new = ask(&[("src", "/_astro/a.png"), ("w", "200")]);
        assert_eq!(images.serve(&new, Some(source)).await.status, 400);
    }

    #[test]
    fn a_source_too_large_to_decode_is_refused() {
        // 9000 is over MAX_DECODE_SIDE.
        let source = png(9000, 1);
        let req = ImageRequest::parse(&ask(&[("src", "/_astro/a.png"), ("w", "10")])).unwrap();
        let err = resize(&source, &req).unwrap_err();
        assert!(err.contains("could not be read"), "{err}");
    }
}
