//! Media: `generate_image` and `speech` (TODO-agent-skills AS11).
//!
//! Two thin layers over `sc-llm`'s non-chat calls (AS3.3, AS3.4) and the
//! display parts a person sees and the model does not (AS6.3).
//!
//! - **`generate_image`** offers one tool. The image is saved to a configured
//!   file store, as the run's caller and under §9's write rule, and shown to
//!   the person as an image part. The model is told the file's name and size
//!   **and nothing else** — no URL, no bytes — so it cannot make up a link to
//!   it, and it never pays for reading its own picture.
//! - **`speech`** offers no tool. Every final answer of a turn is spoken
//!   ([`AgentTrait::on_final_answer`]) and attached to the answer as an audio
//!   part. A run started by a trigger has nobody listening, so it says nothing.
//!
//! Both name their model the way a vector field does (AS2): a provider, blank
//! for the installation's default, and a model of the right kind, blank for
//! that provider's default of the kind. Each call is costed and charged as a
//! chat call is (AS3.5): `generate_image` through
//! [`Delegator::record_call`](sc_agent::Delegator::record_call), `speech`
//! through [`AnswerContext::record_call`].

use bytes::Bytes;
use sc_agent::{AgentTrait, AnswerContext, DisplayPart, ToolsContext, TraitCheck, TraitContext};
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_llm::{ImageRequest, LlmModelDef, LlmProviderDef, ModelKind, SpeechRequest, ToolSpec};
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Value as Json, json};

use crate::files::{
    FileScope, check_scope, check_tool_name, configured_scope, file_caller, open_to_write,
    optional_string_arg, scope_as_written, scope_fields, slugify, string_arg,
};
use crate::table::{arguments, config_str};

/// The LLM provider the media call is made with; blank is the default one.
pub const CFG_PROVIDER: &str = "provider";
/// The model, of the trait's kind; blank is the provider's default of it.
pub const CFG_MODEL: &str = "model";
/// An image's size (`1024x1024`, `1536x1024`, `auto`, …); blank is the
/// model's default.
pub const CFG_SIZE: &str = "size";
/// An image's quality (`low`, `medium`, `high`, `auto`); blank is the model's
/// default.
pub const CFG_QUALITY: &str = "quality";
/// The file format: an image's (`png`, `jpeg`, `webp`) or audio's (`mp3`, …).
pub const CFG_FORMAT: &str = "format";
/// Whether an image has a transparent background.
pub const CFG_TRANSPARENT: &str = "transparent";
/// The voice speech is in.
pub const CFG_VOICE: &str = "voice";
/// How fast it is spoken, 0.25 to 4.0.
pub const CFG_SPEED: &str = "speed";
/// How to speak (tone, accent, pace), for the models that take it.
pub const CFG_INSTRUCTIONS: &str = "instructions";

/// The prompt argument of the image tool.
pub const ARG_PROMPT: &str = "prompt";
/// The optional file name argument of the image tool.
pub const ARG_NAME: &str = "name";

/// The tool one `generate_image` instance offers, derived from its scope.
pub fn generate_image_tool_name(scope: &FileScope) -> String {
    format!("generate_image_{}", scope.slug())
}

/// The provider and model `config` names, of `kind`, refused by name when the
/// provider is one that has no such endpoint.
async fn configured_model(
    catalog: &Catalog,
    config: &Attrs,
    kind: ModelKind,
) -> Result<(LlmProviderDef, LlmModelDef)> {
    let provider = config_str(config, CFG_PROVIDER);
    let model = config_str(config, CFG_MODEL);
    // The provider first, so an Anthropic one is refused for what it is
    // rather than for having no default model of a kind it cannot have.
    let provider = sc_llm::embedding_provider(catalog, Some(provider.as_str())).await?;
    if !kind.served_by(&provider.backend) {
        return Err(Error::invalid(format!(
            "LLM provider `{}` is an Anthropic provider, and Anthropic has no {kind} model; \
             name an OpenAI or OpenAI-compatible provider in `{CFG_PROVIDER}`",
            provider.name
        )));
    }
    let model = sc_llm::require_model_of_kind(
        catalog,
        &provider,
        Some(model.as_str()).filter(|m| !m.is_empty()),
        kind,
    )
    .await?;
    Ok((provider, model))
}

/// A configured option that is set, or `None` for the model's default.
fn option(config: &Attrs, key: &str) -> Option<String> {
    Some(config_str(config, key)).filter(|v| !v.is_empty())
}

/// The two settings both traits start with.
fn model_fields(kind: &str) -> Vec<FormField> {
    vec![
        FormField::new(CFG_PROVIDER, BasicType::Text)
            .label("LLM provider")
            .sublabel("Blank is the default provider")
            .default_value(""),
        FormField::new(CFG_MODEL, BasicType::Text)
            .label(format!("{kind} model"))
            .sublabel(format!("Blank is the provider's default {kind} model"))
            .default_value(""),
    ]
}

// --- generate_image ----------------------------------------------------------

/// Generate an image, save it to a file store, and show it to the person.
pub struct GenerateImage;

impl GenerateImage {
    /// What the image request is, from the configuration and the prompt.
    fn request(config: &Attrs, prompt: String) -> ImageRequest {
        ImageRequest {
            prompt,
            size: option(config, CFG_SIZE),
            quality: option(config, CFG_QUALITY),
            format: option(config, CFG_FORMAT),
            background: config
                .get(CFG_TRANSPARENT)
                .and_then(Json::as_bool)
                .unwrap_or(false)
                .then(|| "transparent".to_owned()),
        }
    }
}

#[async_trait::async_trait]
impl AgentTrait for GenerateImage {
    fn name(&self) -> &str {
        "generate_image"
    }

    fn description(&self) -> &str {
        "Generate images from a description, saved to a file store and shown to the user"
    }

    fn config_spec(&self) -> Vec<FormField> {
        let mut spec = scope_fields();
        spec.extend(model_fields("Image"));
        spec.extend([
            FormField::new(CFG_SIZE, BasicType::Text)
                .label("Size")
                .sublabel("1024x1024, 1536x1024, 1024x1536 or auto; blank is the model's default")
                .default_value(""),
            FormField::new(CFG_QUALITY, BasicType::Text)
                .label("Quality")
                .options(["", "low", "medium", "high", "auto", "standard", "hd"])
                .default_value(""),
            FormField::new(CFG_FORMAT, BasicType::Text)
                .label("Format")
                .options(["png", "jpeg", "webp"])
                .default_value("png"),
            FormField::new(CFG_TRANSPARENT, BasicType::Bool)
                .label("Transparent background")
                .default_value(false),
        ]);
        spec
    }

    /// The store exists, the model is an image model of a provider that has
    /// the endpoint, and the options are ones an image can have.
    async fn validate_config(&self, check: &TraitCheck<'_>) -> Result<()> {
        let scope = check_scope(check).await?;
        check_tool_name(&generate_image_tool_name(&scope))?;
        configured_model(check.catalog, check.config, ModelKind::Image).await?;
        GenerateImage::request(check.config, "check".to_owned()).check()
    }

    fn tools(&self, _cx: &ToolsContext<'_>, config: &Attrs) -> Vec<ToolSpec> {
        let scope = scope_as_written(config);
        vec![ToolSpec::new(
            generate_image_tool_name(&scope),
            format!(
                "Generate an image from a description. The image is saved to {} and shown \
                 to the user beside your answer; you are told its file name and size, not \
                 its contents. Do not write a link, URL or Markdown image for it: the user \
                 already sees it.",
                scope.label()
            ),
            json!({
                "type": "object",
                "properties": {
                    ARG_PROMPT: {
                        "type": "string",
                        "description": "What the image shows, in detail: subject, style, \
                                        composition, colours.",
                    },
                    ARG_NAME: {
                        "type": "string",
                        "description": "A short file name for it, without an extension.",
                    },
                },
                "required": [ARG_PROMPT],
                "additionalProperties": false,
            }),
        )]
    }

    async fn call(
        &self,
        config: &Attrs,
        _tool: &str,
        args: &Json,
        ctx: &mut TraitContext<'_>,
    ) -> Result<Json> {
        let args = arguments(args, &[ARG_PROMPT, ARG_NAME])?;
        let prompt = string_arg(&args, ARG_PROMPT)?;
        let stem = match slugify(&optional_string_arg(&args, ARG_NAME)?) {
            stem if stem.is_empty() => "image".to_owned(),
            stem => stem.chars().take(60).collect(),
        };
        let scope = configured_scope(config)?;
        let request = GenerateImage::request(config, prompt);
        request.check()?;
        let (provider, model) = configured_model(ctx.catalog, config, ModelKind::Image).await?;

        // The write is checked before the image is paid for.
        let unique = uuid::Uuid::new_v4().simple().to_string();
        let file_name = |format: &str| format!("{stem}-{}.{format}", &unique[..8]);
        let wanted = request.format.as_deref().unwrap_or("png").to_owned();
        open_to_write(&scope, ctx, &file_name(&wanted)).await?;

        let image = sc_llm::generate_image(&provider, &model, &request).await?;
        // Charged before anything is kept or shown: an image that cannot be
        // paid for is not handed over.
        match ctx.delegate {
            Some(delegate) => delegate.record_call(&image.call).await?,
            None => sc_log::log_warn!(
                "agent `{}`: an image was generated outside a run that can charge for it",
                ctx.agent
            ),
        }

        let name = file_name(&image.format);
        let (store, path) = open_to_write(&scope, ctx, &name).await?;
        store.write(&path, Bytes::from(image.bytes.clone())).await?;
        // Whoever the run is for created the file (§2.1), as an upload would.
        if let Some(owner) = file_caller(ctx.caller).user_id {
            let mut meta = store.get_meta(&path).await.unwrap_or_default();
            meta.owner = Some(owner);
            if let Err(e) = store.set_meta(&path, &meta).await {
                sc_log::log_warn!(
                    "agent `{}`: recording `{path}`'s owner failed — {e}",
                    ctx.agent
                );
            }
        }
        let bytes = image.bytes.len();
        let shown = ctx.show(DisplayPart::Image {
            mime_type: image.mime_type().to_owned(),
            data: image.bytes,
            name: Some(name.clone()),
        });
        let mut result = json!({
            "file": scope.relative(&path),
            "bytes": bytes,
        });
        match shown {
            Ok(()) => {
                result["shown_to_user"] = json!(true);
                result["note"] = json!(
                    "The user sees the image beside your answer. Do not link to it or \
                     describe where it is stored."
                );
            }
            Err(e) => {
                result["shown_to_user"] = json!(false);
                result["note"] = json!(format!("The image was saved but not shown: {e}"));
            }
        }
        Ok(result)
    }
}

// --- speech ------------------------------------------------------------------

/// Read every final answer aloud, attached as an audio part. No tool.
pub struct Speech;

impl Speech {
    /// What the speech request is, from the configuration and the answer.
    fn request(config: &Attrs, text: String) -> Result<SpeechRequest> {
        let speed = match config.get(CFG_SPEED) {
            None | Some(Json::Null) => None,
            Some(value) => Some(value.as_f64().ok_or_else(|| {
                Error::invalid(format!("`{CFG_SPEED}` should be a number, got {value}"))
            })?),
        };
        Ok(SpeechRequest {
            text,
            voice: option(config, CFG_VOICE),
            speed,
            format: option(config, CFG_FORMAT),
            instructions: option(config, CFG_INSTRUCTIONS),
        })
    }
}

/// `text` cut to what one speech call takes, at the last sentence end that
/// fits where there is one.
pub fn speakable(text: &str) -> String {
    let text = text.trim();
    let limit = sc_llm::speech::MAX_SPEECH_CHARACTERS;
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let cut: String = text.chars().take(limit).collect();
    match cut.rfind(['.', '!', '?', '\n']) {
        Some(end) if end > limit / 2 => cut[..=end].trim().to_owned(),
        _ => cut,
    }
}

#[async_trait::async_trait]
impl AgentTrait for Speech {
    fn name(&self) -> &str {
        "speech"
    }

    fn description(&self) -> &str {
        "Read every answer aloud to the user"
    }

    fn config_spec(&self) -> Vec<FormField> {
        let mut spec = model_fields("Speech");
        spec.extend([
            FormField::new(CFG_VOICE, BasicType::Text)
                .label("Voice")
                .sublabel("alloy, ash, coral, echo, fable, nova, onyx, sage, shimmer, …")
                .default_value(sc_llm::speech::DEFAULT_VOICE),
            FormField::new(CFG_SPEED, BasicType::Float)
                .label("Speed")
                .sublabel("0.25 to 4.0; 1 is normal")
                .default_value(1.0),
            FormField::new(CFG_FORMAT, BasicType::Text)
                .label("Format")
                .options(sc_llm::speech::SPEECH_FORMATS)
                .default_value("mp3"),
            FormField::new(CFG_INSTRUCTIONS, BasicType::Text)
                .label("Instructions")
                .sublabel("How to speak: tone, accent, pace")
                .multiline()
                .default_value(""),
        ]);
        spec
    }

    /// The model is a speech model of a provider that has the endpoint, and
    /// the options are ones speech can have.
    async fn validate_config(&self, check: &TraitCheck<'_>) -> Result<()> {
        configured_model(check.catalog, check.config, ModelKind::Speech).await?;
        Speech::request(check.config, "check".to_owned())?.check()
    }

    fn tools(&self, _cx: &ToolsContext<'_>, _config: &Attrs) -> Vec<ToolSpec> {
        Vec::new()
    }

    async fn call(
        &self,
        _config: &Attrs,
        tool: &str,
        _args: &Json,
        _ctx: &mut TraitContext<'_>,
    ) -> Result<Json> {
        Err(Error::invalid(format!(
            "`speech` offers no tools, so there is no `{tool}`"
        )))
    }

    async fn on_final_answer(&self, config: &Attrs, cx: &mut AnswerContext<'_>) -> Result<()> {
        // A trigger's run has nobody listening.
        if cx.caller.triggered {
            return Ok(());
        }
        let request = Speech::request(config, speakable(cx.answer))?;
        let (provider, model) = configured_model(cx.catalog, config, ModelKind::Speech).await?;
        let speech = sc_llm::speak(&provider, &model, &request).await?;
        cx.record_call(speech.call.clone());
        cx.show(DisplayPart::Audio {
            mime_type: speech.mime_type().to_owned(),
            name: Some(format!("answer.{}", speech.format)),
            data: speech.bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_answer_is_cut_at_a_sentence_that_fits() {
        assert_eq!(speakable("  Hello.  "), "Hello.");
        let sentence = "This is one sentence. ";
        let long = sentence.repeat(400);
        let spoken = speakable(&long);
        assert!(spoken.chars().count() <= sc_llm::speech::MAX_SPEECH_CHARACTERS);
        assert!(spoken.ends_with('.'), "{spoken}");
    }

    #[test]
    fn transparency_is_the_background_option() {
        let config: Attrs = [
            (CFG_TRANSPARENT.to_owned(), json!(true)),
            (CFG_FORMAT.to_owned(), json!("webp")),
            (CFG_SIZE.to_owned(), json!("")),
        ]
        .into_iter()
        .collect();
        let request = GenerateImage::request(&config, "a cat".to_owned());
        assert_eq!(request.background.as_deref(), Some("transparent"));
        assert_eq!(request.format.as_deref(), Some("webp"));
        assert_eq!(request.size, None);
    }

    #[test]
    fn the_tool_is_named_after_its_store() {
        let scope = FileScope {
            store: "media".to_owned(),
            root: "generated".to_owned(),
        };
        assert_eq!(
            generate_image_tool_name(&scope),
            "generate_image_media_generated"
        );
    }
}
