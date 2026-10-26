//! Text to speech: text in, audio out (TODO-agent-skills AS3.4), for the
//! `speech` trait (AS11.2).
//!
//! One request to the provider's `/audio/speech` (see `endpoint.rs` for what
//! the non-chat calls share), whose reply is the audio file itself:
//!
//! ```text
//! POST {base_url}/audio/speech
//!   { "model": "gpt-4o-mini-tts", "input": "…", "voice": "alloy", "speed": 1.0,
//!     "response_format": "mp3", "instructions": "Speak warmly." }
//! → audio/mpeg bytes
//! ```
//!
//! Costed per character, since the endpoint reports no tokens.

use std::time::Duration;

use sc_error::{Error, Result};
use serde_json::{Map, Value as Json};

use crate::Usage;
use crate::def::LlmProviderDef;
use crate::endpoint::{Endpoint, ModelCall};
use crate::model::{LlmModelDef, ModelKind};

/// How long speaking one text may take.
const SPEECH_TIMEOUT: Duration = Duration::from_secs(180);

/// The longest text spoken in one call: OpenAI's limit.
pub const MAX_SPEECH_CHARACTERS: usize = 4_096;

/// The voice used when none is named.
pub const DEFAULT_VOICE: &str = "alloy";

/// The audio formats there are.
pub const SPEECH_FORMATS: [&str; 6] = ["mp3", "opus", "aac", "flac", "wav", "pcm"];

/// What to say and how. Every option but the text and voice is the model's
/// default when `None`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpeechRequest {
    /// What is said.
    pub text: String,
    /// The voice, by the host's name for it; blank is [`DEFAULT_VOICE`].
    pub voice: Option<String>,
    /// From 0.25 to 4.0; 1.0 is normal.
    pub speed: Option<f64>,
    /// One of [`SPEECH_FORMATS`]; the host's default (mp3) when unset.
    pub format: Option<String>,
    /// How to say it (tone, accent, pace), for the models that take it.
    pub instructions: Option<String>,
}

impl SpeechRequest {
    /// Say `text` in the default voice.
    pub fn new(text: impl Into<String>) -> SpeechRequest {
        SpeechRequest {
            text: text.into(),
            ..SpeechRequest::default()
        }
    }

    /// Check what can be checked without asking: some text, not too much,
    /// a speed in range and a format that exists.
    pub fn check(&self) -> Result<()> {
        let text = self.text.trim();
        if text.is_empty() {
            return Err(Error::invalid("speech needs some text to say"));
        }
        let chars = text.chars().count();
        if chars > MAX_SPEECH_CHARACTERS {
            return Err(Error::invalid(format!(
                "speech is at most {MAX_SPEECH_CHARACTERS} characters a call; this text has \
                 {chars}"
            )));
        }
        if let Some(speed) = self.speed
            && !(0.25..=4.0).contains(&speed)
        {
            return Err(Error::invalid(format!(
                "speech speed {speed} is outside 0.25 to 4.0"
            )));
        }
        if let Some(format) = set(&self.format)
            && !SPEECH_FORMATS.contains(&format)
        {
            return Err(Error::invalid(format!(
                "audio format `{format}` is not one of {}",
                SPEECH_FORMATS.join(", ")
            )));
        }
        Ok(())
    }

    /// The request body for `model`.
    fn body(&self, model: &str) -> Json {
        let mut body = Map::new();
        body.insert("model".into(), model.into());
        body.insert("input".into(), self.text.trim().into());
        body.insert(
            "voice".into(),
            set(&self.voice).unwrap_or(DEFAULT_VOICE).into(),
        );
        if let Some(speed) = self.speed {
            body.insert("speed".into(), speed.into());
        }
        if let Some(format) = set(&self.format) {
            body.insert("response_format".into(), format.into());
        }
        if let Some(instructions) = set(&self.instructions) {
            body.insert("instructions".into(), instructions.into());
        }
        Json::Object(body)
    }

    /// The format the reply will be in.
    fn format(&self) -> &str {
        set(&self.format).unwrap_or("mp3")
    }
}

/// Spoken text.
#[derive(Debug, Clone, PartialEq)]
pub struct Speech {
    /// The audio file's bytes.
    pub bytes: Vec<u8>,
    /// Its format: one of [`SPEECH_FORMATS`].
    pub format: String,
    /// What the call used and cost.
    pub call: ModelCall,
}

impl Speech {
    /// Its media type, as a file store or a browser wants it.
    pub fn mime_type(&self) -> &'static str {
        match self.format.as_str() {
            "opus" => "audio/ogg",
            "aac" => "audio/aac",
            "flac" => "audio/flac",
            "wav" => "audio/wav",
            "pcm" => "audio/L16",
            _ => "audio/mpeg",
        }
    }
}

/// Say `request`'s text with speech model `model` of `provider`, costed per
/// character.
///
/// Refused before anything is sent for a request [`SpeechRequest::check`]
/// refuses, a model of another kind, and an Anthropic provider; afterwards,
/// for an empty reply or one that is JSON rather than audio.
pub async fn speak(
    provider: &LlmProviderDef,
    model: &LlmModelDef,
    request: &SpeechRequest,
) -> Result<Speech> {
    const DOING: &str = "speak";
    request.check()?;
    let endpoint = Endpoint::new(provider, model, ModelKind::Speech, SPEECH_TIMEOUT)?;
    let (bytes, content_type) = endpoint
        .post("/audio/speech", &request.body(endpoint.model()), DOING)
        .await?;
    if bytes.is_empty() {
        return Err(endpoint.failed(DOING, &"the reply is empty"));
    }
    if content_type.is_some_and(|t| t.starts_with("application/json")) {
        let text: String = String::from_utf8_lossy(&bytes).chars().take(500).collect();
        return Err(endpoint.failed(DOING, &format!("the reply is not audio: {text}")));
    }
    let characters = request.text.trim().chars().count() as u64;
    let cost = model.prices().speech_cost(characters);
    Ok(Speech {
        bytes,
        format: request.format().to_owned(),
        call: endpoint.finish(Usage::default(), characters, cost),
    })
}

/// An option's value, when it is set and not blank.
fn set(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_body_names_a_voice_always_and_the_rest_when_set() {
        let body = SpeechRequest::new(" hello ").body("tts-1");
        assert_eq!(body["input"], "hello");
        assert_eq!(body["voice"], DEFAULT_VOICE);
        assert!(body.get("speed").is_none());
        let body = SpeechRequest {
            voice: Some("nova".into()),
            speed: Some(1.5),
            format: Some("wav".into()),
            instructions: Some("Whisper.".into()),
            ..SpeechRequest::new("hello")
        }
        .body("gpt-4o-mini-tts");
        assert_eq!(body["voice"], "nova");
        assert_eq!(body["speed"], 1.5);
        assert_eq!(body["response_format"], "wav");
        assert_eq!(body["instructions"], "Whisper.");
    }

    #[test]
    fn what_cannot_work_is_refused_before_asking() {
        assert!(SpeechRequest::new("").check().is_err());
        let long = SpeechRequest::new("a".repeat(MAX_SPEECH_CHARACTERS + 1));
        assert!(long.check().unwrap_err().to_string().contains("4096"));
        let fast = SpeechRequest {
            speed: Some(5.0),
            ..SpeechRequest::new("hi")
        };
        assert!(fast.check().is_err());
        let ogg = SpeechRequest {
            format: Some("ogg".into()),
            ..SpeechRequest::new("hi")
        };
        assert!(ogg.check().unwrap_err().to_string().contains("ogg"));
    }
}
