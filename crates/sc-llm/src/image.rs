//! Image generation: a prompt in, an image's bytes out (TODO-agent-skills
//! AS3.3), for the `generate_image` trait (AS11.1).
//!
//! One request to the provider's `/images/generations` (see `endpoint.rs` for
//! what the non-chat calls share):
//!
//! ```text
//! POST {base_url}/images/generations
//!   { "model": "gpt-image-1", "prompt": "…", "n": 1, "size": "1024x1024",
//!     "quality": "low", "output_format": "png", "background": "transparent" }
//! → { "data": [ { "b64_json": "…" } ], "usage": { "input_tokens": …, "output_tokens": … } }
//! ```
//!
//! Only the options that are set are sent, because the models disagree about
//! which they take (`dall-e-3` has no `output_format` or `background`). A host
//! that answers with a `url` instead of `b64_json` is fetched from it.

use std::time::Duration;

use base64::Engine as _;
use sc_error::{Error, Result};
use serde_json::{Map, Value as Json};

use crate::def::LlmProviderDef;
use crate::endpoint::{Endpoint, ModelCall, read_usage};
use crate::model::{LlmModelDef, ModelKind};

/// How long one image may take. Generating one can take a minute or more.
const IMAGE_TIMEOUT: Duration = Duration::from_secs(300);

/// The longest prompt sent. OpenAI's own limit for `gpt-image-1`.
pub const MAX_IMAGE_PROMPT: usize = 32_000;

/// What to generate. Every option but the prompt is the model's default when
/// `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageRequest {
    /// What the image shows.
    pub prompt: String,
    /// `1024x1024`, `1536x1024`, `auto`, … — passed through, since each model
    /// has its own list.
    pub size: Option<String>,
    /// `low`, `medium`, `high`, `auto` (or `standard`, `hd` for `dall-e-3`).
    pub quality: Option<String>,
    /// `png`, `jpeg` or `webp`.
    pub format: Option<String>,
    /// `transparent`, `opaque` or `auto`.
    pub background: Option<String>,
}

impl ImageRequest {
    /// A request for `prompt` with every option the model's default.
    pub fn new(prompt: impl Into<String>) -> ImageRequest {
        ImageRequest {
            prompt: prompt.into(),
            ..ImageRequest::default()
        }
    }

    /// Check what can be checked without asking: a prompt, its length, a
    /// format and background that exist, and transparency only where the
    /// format has it.
    pub fn check(&self) -> Result<()> {
        let prompt = self.prompt.trim();
        if prompt.is_empty() {
            return Err(Error::invalid("an image needs a prompt"));
        }
        let chars = prompt.chars().count();
        if chars > MAX_IMAGE_PROMPT {
            return Err(Error::invalid(format!(
                "an image prompt is at most {MAX_IMAGE_PROMPT} characters; this one has {chars}"
            )));
        }
        if let Some(format) = set(&self.format)
            && !["png", "jpeg", "webp"].contains(&format)
        {
            return Err(Error::invalid(format!(
                "image format `{format}` is not one of png, jpeg, webp"
            )));
        }
        if let Some(background) = set(&self.background) {
            if !["transparent", "opaque", "auto"].contains(&background) {
                return Err(Error::invalid(format!(
                    "image background `{background}` is not one of transparent, opaque, auto"
                )));
            }
            if background == "transparent" && set(&self.format) == Some("jpeg") {
                return Err(Error::invalid(
                    "a jpeg cannot be transparent; ask for png or webp",
                ));
            }
        }
        Ok(())
    }

    /// The request body for `model`.
    fn body(&self, model: &str) -> Json {
        let mut body = Map::new();
        body.insert("model".into(), model.into());
        body.insert("prompt".into(), self.prompt.trim().into());
        body.insert("n".into(), 1.into());
        for (key, value) in [
            ("size", &self.size),
            ("quality", &self.quality),
            ("output_format", &self.format),
            ("background", &self.background),
        ] {
            if let Some(value) = set(value) {
                body.insert(key.into(), value.into());
            }
        }
        Json::Object(body)
    }
}

/// A generated image.
#[derive(Debug, Clone, PartialEq)]
pub struct GeneratedImage {
    /// The image file's bytes.
    pub bytes: Vec<u8>,
    /// Its format, read from the bytes: `png`, `jpeg`, `webp` or `gif`.
    pub format: String,
    /// The prompt the model actually drew, when it rewrote it (`dall-e-3`).
    pub revised_prompt: Option<String>,
    /// What the call used and cost.
    pub call: ModelCall,
}

impl GeneratedImage {
    /// Its media type, as a file store or a browser wants it.
    pub fn mime_type(&self) -> &'static str {
        match self.format.as_str() {
            "jpeg" => "image/jpeg",
            "webp" => "image/webp",
            "gif" => "image/gif",
            _ => "image/png",
        }
    }
}

/// Generate one image with image model `model` of `provider`, costed per image
/// or by the tokens the provider reports.
///
/// Refused before anything is sent for a request [`ImageRequest::check`]
/// refuses, a model of another kind, and an Anthropic provider; afterwards,
/// for a reply with no image or one whose bytes are not an image.
pub async fn generate_image(
    provider: &LlmProviderDef,
    model: &LlmModelDef,
    request: &ImageRequest,
) -> Result<GeneratedImage> {
    const DOING: &str = "generate an image";
    request.check()?;
    let endpoint = Endpoint::new(provider, model, ModelKind::Image, IMAGE_TIMEOUT)?;
    let (reply, _) = endpoint
        .post(
            "/images/generations",
            &request.body(endpoint.model()),
            DOING,
        )
        .await?;
    let reply = endpoint.json(&reply, DOING)?;
    let item = reply
        .get("data")
        .and_then(Json::as_array)
        .and_then(|d| d.first())
        .ok_or_else(|| endpoint.failed(DOING, &"the reply has no image"))?;
    let bytes = if let Some(b64) = item.get("b64_json").and_then(Json::as_str) {
        base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .map_err(|e| endpoint.failed(DOING, &format!("the image is not base64: {e}")))?
    } else if let Some(url) = item.get("url").and_then(Json::as_str) {
        endpoint.fetch(url, DOING).await?.0
    } else {
        return Err(endpoint.failed(DOING, &"the reply has neither `b64_json` nor `url`"));
    };
    let format = image_format(&bytes)
        .ok_or_else(|| endpoint.failed(DOING, &"what came back is not an image"))?;
    let usage = read_usage(&reply);
    let cost = model.prices().image_cost(1, &usage);
    Ok(GeneratedImage {
        bytes,
        format: format.to_owned(),
        revised_prompt: item
            .get("revised_prompt")
            .and_then(Json::as_str)
            .map(str::to_owned),
        call: endpoint.finish(usage, 1, cost),
    })
}

/// An image's format, by its first bytes.
pub fn image_format(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpeg")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else if bytes.starts_with(b"GIF8") {
        Some("gif")
    } else {
        None
    }
}

/// An option's value, when it is set and not blank.
fn set(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_options_set_are_sent() {
        let request = ImageRequest {
            size: Some("1024x1024".into()),
            background: Some(" ".into()),
            ..ImageRequest::new("a cat")
        };
        let body = request.body("dall-e-3");
        assert_eq!(body["size"], "1024x1024");
        assert_eq!(body["n"], 1);
        assert!(body.get("background").is_none());
        assert!(body.get("output_format").is_none());
    }

    #[test]
    fn what_cannot_work_is_refused_before_asking() {
        assert!(ImageRequest::new("  ").check().is_err());
        let jpeg = ImageRequest {
            format: Some("jpeg".into()),
            background: Some("transparent".into()),
            ..ImageRequest::new("a cat")
        };
        assert!(
            jpeg.check()
                .unwrap_err()
                .to_string()
                .contains("transparent")
        );
        let tiff = ImageRequest {
            format: Some("tiff".into()),
            ..ImageRequest::new("a cat")
        };
        assert!(tiff.check().unwrap_err().to_string().contains("tiff"));
    }

    #[test]
    fn formats_are_read_from_the_bytes() {
        assert_eq!(image_format(b"\x89PNG\r\n\x1a\nrest"), Some("png"));
        assert_eq!(image_format(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("jpeg"));
        assert_eq!(image_format(b"RIFF\0\0\0\0WEBPVP8 "), Some("webp"));
        assert_eq!(image_format(b"<html>"), None);
    }
}
