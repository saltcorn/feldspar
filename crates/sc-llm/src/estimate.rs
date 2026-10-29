//! A token estimate for a request before it is sent (TODO §4, §9).
//!
//! It exists to decide **when to compact**, not to bill. A tokenizer per vendor
//! would be exact and would add a dependency per vendor, and the question it
//! answers ("is this context near its budget?") does not need exactness. So the
//! estimate is a character heuristic, and a run corrects it with a calibration
//! factor taken from the `input_tokens` the provider reported for the previous
//! step ([`TokenEstimator::calibrate`]).
//!
//! Images are the exception to "characters": each vendor publishes a rule for
//! what an image costs, and a screenshot counted by its base64 length would be
//! wrong by two orders of magnitude.

use serde::{Deserialize, Serialize};

use crate::def::ANTHROPIC_BACKEND;
use crate::message::{ImagePart, LlmMessage, LlmRequest, ProviderItem};

/// Characters per token for ordinary text and code. English prose runs near
/// four, and code and JSON a little lower; three and a half errs towards
/// compacting early.
const CHARS_PER_TOKEN: f64 = 3.5;

/// The framing tokens each message costs beyond its content: the role marker
/// and separators.
const PER_MESSAGE_OVERHEAD: u64 = 4;

/// The dimensions assumed for an image whose header cannot be read.
const UNKNOWN_IMAGE_SIDE: u32 = 1024;

/// How far calibration may move the estimate. A report far outside this range
/// is more likely a provider that did not report (or reported something else)
/// than a heuristic that far off.
const MIN_FACTOR: f64 = 0.25;
const MAX_FACTOR: f64 = 4.0;

/// The raw estimate for `req`, using `backend`'s image rule. No calibration.
pub fn estimate_tokens(req: &LlmRequest, backend: &str) -> u64 {
    let mut chars: usize = 0;
    let mut tokens: u64 = 0;

    if let Some(system) = &req.system {
        chars += system.len();
    }
    for tool in &req.tools {
        chars += tool.name.len() + tool.description.len() + tool.parameters.to_string().len();
        tokens += PER_MESSAGE_OVERHEAD;
    }
    for message in &req.messages {
        tokens += PER_MESSAGE_OVERHEAD;
        match message {
            LlmMessage::User {
                content,
                attachments,
            } => {
                chars += content.len();
                for attachment in attachments {
                    if attachment.is_image() {
                        tokens += image_tokens(&attachment.image(), backend);
                    } else {
                        chars += attachment.data.len() + attachment.name.len();
                    }
                }
            }
            LlmMessage::Assistant {
                content,
                tool_calls,
                provider_items,
            } => {
                chars += content.len();
                for call in tool_calls {
                    chars += call.name.len() + call.arguments.to_string().len();
                }
                for item in provider_items {
                    chars += match item {
                        // Encrypted reasoning is billed as the reasoning it
                        // encodes, which is somewhat shorter than its payload.
                        ProviderItem::EncryptedReasoning {
                            encrypted_content, ..
                        } => encrypted_content.len() * 3 / 4,
                        ProviderItem::SignedThinking { thinking, .. } => thinking.len(),
                        ProviderItem::RedactedThinking { data } => data.len() * 3 / 4,
                    };
                }
            }
            LlmMessage::ToolResult {
                content, images, ..
            } => {
                chars += content.len();
                for image in images {
                    tokens += image_tokens(image, backend);
                }
            }
        }
    }
    tokens + (chars as f64 / CHARS_PER_TOKEN).ceil() as u64
}

/// What one image costs on `backend`, by the vendor's published rule.
///
/// - **Anthropic:** the image is scaled to fit 1568 pixels on its long side,
///   then costs `width × height / 750`.
/// - **OpenAI** (both APIs, and the default for any other host): the image is
///   scaled to fit 2048×2048, then its short side to 768, and costs 85 plus 170
///   per 512-pixel tile.
pub fn image_tokens(image: &ImagePart, backend: &str) -> u64 {
    let (width, height) =
        image_dimensions(&image.data).unwrap_or((UNKNOWN_IMAGE_SIDE, UNKNOWN_IMAGE_SIDE));
    let (w, h) = (f64::from(width.max(1)), f64::from(height.max(1)));
    if backend == ANTHROPIC_BACKEND {
        let scale = (1568.0 / w.max(h)).min(1.0);
        let (w, h) = (w * scale, h * scale);
        (w * h / 750.0).ceil() as u64
    } else {
        let scale = (2048.0 / w.max(h)).min(1.0);
        let (w, h) = (w * scale, h * scale);
        let scale = (768.0 / w.min(h)).min(1.0);
        let (w, h) = (w * scale, h * scale);
        let tiles = (w / 512.0).ceil() * (h / 512.0).ceil();
        85 + 170 * tiles as u64
    }
}

/// The width and height of a PNG or JPEG, read from its header.
fn image_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    // PNG: the signature, then the IHDR chunk with width and height big-endian.
    if data.len() >= 24 && data.starts_with(b"\x89PNG\r\n\x1a\n") {
        let width = u32::from_be_bytes(data[16..20].try_into().ok()?);
        let height = u32::from_be_bytes(data[20..24].try_into().ok()?);
        return Some((width, height));
    }
    // JPEG: walk the segments to a start-of-frame marker.
    if data.len() >= 4 && data[0] == 0xFF && data[1] == 0xD8 {
        let mut i = 2;
        while i + 9 < data.len() {
            if data[i] != 0xFF {
                return None;
            }
            let marker = data[i + 1];
            let length = usize::from(u16::from_be_bytes([data[i + 2], data[i + 3]]));
            let is_frame = matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
            if is_frame {
                let height = u16::from_be_bytes([data[i + 5], data[i + 6]]);
                let width = u16::from_be_bytes([data[i + 7], data[i + 8]]);
                return Some((u32::from(width), u32::from(height)));
            }
            i += 2 + length;
        }
    }
    None
}

/// A run's estimator: the raw estimate times a factor calibrated against what
/// the provider reported.
///
/// Serialisable, so a run that is resumed keeps its calibration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenEstimator {
    /// The backend whose image rule applies.
    pub backend: String,
    /// Reported tokens per estimated token, from the last calibration. `1.0`
    /// before any.
    pub factor: f64,
}

impl TokenEstimator {
    /// An uncalibrated estimator for `backend`.
    pub fn new(backend: impl Into<String>) -> TokenEstimator {
        TokenEstimator {
            backend: backend.into(),
            factor: 1.0,
        }
    }

    /// The calibrated estimate for `req`.
    pub fn estimate(&self, req: &LlmRequest) -> u64 {
        (estimate_tokens(req, &self.backend) as f64 * self.factor).ceil() as u64
    }

    /// Calibrate against a step: `req` was sent and the provider reported
    /// `input_tokens` for it. A report of zero means the provider did not say,
    /// and changes nothing.
    pub fn calibrate(&mut self, req: &LlmRequest, input_tokens: u64) {
        let raw = estimate_tokens(req, &self.backend);
        if input_tokens == 0 || raw == 0 {
            return;
        }
        self.factor = (input_tokens as f64 / raw as f64).clamp(MIN_FACTOR, MAX_FACTOR);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::def::OPENAI_RESPONSES_BACKEND;
    use crate::message::{ToolCall, ToolSpec};
    use serde_json::json;

    fn request() -> LlmRequest {
        LlmRequest::prompt("Summarise the open invoices for March, grouped by customer.")
            .system("You are a careful bookkeeping assistant.")
            .tools([ToolSpec::new(
                "query_invoices",
                "Query the invoices table.",
                json!({"type": "object", "properties": {"where": {"type": "object"}}}),
            )])
    }

    #[test]
    fn the_estimate_is_stable_and_grows_with_the_request() {
        let req = request();
        let first = estimate_tokens(&req, OPENAI_RESPONSES_BACKEND);
        assert_eq!(first, estimate_tokens(&req, OPENAI_RESPONSES_BACKEND));
        // Roughly a quarter of the characters, plus framing.
        assert!((40..120).contains(&first), "{first}");

        let mut longer = req.clone();
        let call = ToolCall {
            id: "c1".to_owned(),
            name: "query_invoices".to_owned(),
            arguments: json!({"where": {"month": 3}}),
        };
        longer
            .messages
            .push(LlmMessage::assistant_with_calls("", vec![call.clone()]));
        longer
            .messages
            .push(LlmMessage::tool_result(&call, "x".repeat(3_500)));
        let grown = estimate_tokens(&longer, OPENAI_RESPONSES_BACKEND);
        assert!(grown >= first + 1_000, "{first} → {grown}");
    }

    #[test]
    fn calibration_scales_later_estimates_by_the_last_report() {
        let req = request();
        let mut estimator = TokenEstimator::new(OPENAI_RESPONSES_BACKEND);
        let raw = estimator.estimate(&req);
        assert_eq!(raw, estimate_tokens(&req, OPENAI_RESPONSES_BACKEND));

        estimator.calibrate(&req, raw * 2);
        assert_eq!(estimator.estimate(&req), raw * 2);

        // No report changes nothing; an absurd one is clamped.
        estimator.calibrate(&req, 0);
        assert_eq!(estimator.estimate(&req), raw * 2);
        estimator.calibrate(&req, raw * 1_000);
        assert!((estimator.factor - MAX_FACTOR).abs() < f64::EPSILON);

        // It survives a round trip with the run.
        let back: TokenEstimator =
            serde_json::from_value(serde_json::to_value(&estimator).unwrap()).unwrap();
        assert_eq!(back, estimator);
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut data = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        data.extend_from_slice(&width.to_be_bytes());
        data.extend_from_slice(&height.to_be_bytes());
        data.extend_from_slice(&[8, 6, 0, 0, 0]);
        data
    }

    fn jpeg(width: u16, height: u16) -> Vec<u8> {
        let mut data = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00];
        data.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        data.extend_from_slice(&height.to_be_bytes());
        data.extend_from_slice(&width.to_be_bytes());
        data.extend_from_slice(&[0; 12]);
        data
    }

    #[test]
    fn images_are_counted_by_each_vendors_rule() {
        assert_eq!(image_dimensions(&png(1280, 800)), Some((1280, 800)));
        assert_eq!(image_dimensions(&jpeg(640, 480)), Some((640, 480)));
        assert_eq!(image_dimensions(b"not an image"), None);

        let shot = ImagePart::new("image/png", png(1280, 800));
        // Anthropic: fits in 1568, so 1280 × 800 / 750.
        assert_eq!(image_tokens(&shot, ANTHROPIC_BACKEND), 1366);
        // OpenAI: short side to 768 → 1229 × 768, 3 × 2 tiles.
        assert_eq!(image_tokens(&shot, OPENAI_RESPONSES_BACKEND), 85 + 170 * 6);

        let small = ImagePart::new("image/jpeg", jpeg(512, 512));
        assert_eq!(image_tokens(&small, OPENAI_RESPONSES_BACKEND), 85 + 170);

        // A screenshot is counted by its rule, not its bytes.
        let mut req = LlmRequest::prompt("look");
        req.messages.push(LlmMessage::ToolResult {
            tool_call_id: "c".to_owned(),
            name: "view_app".to_owned(),
            content: String::new(),
            images: vec![ImagePart::new("image/png", {
                let mut d = png(1280, 800);
                d.resize(500_000, 0);
                d
            })],
        });
        let estimate = estimate_tokens(&req, ANTHROPIC_BACKEND);
        assert!((1366..1400).contains(&estimate), "{estimate}");
    }
}
