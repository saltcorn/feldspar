//! The non-chat calls (TODO-agent-skills AS3) against a stub OpenAI-compatible
//! server: what is sent, what is read back, what it cost, and the refusals.
//! The embedding call is AS3.2; image generation AS3.3; speech AS3.4.

use base64::Engine as _;
use sc_llm::{
    CFG_API_KEY, CFG_BASE_URL, CFG_PRICE_INPUT, CFG_PRICE_PER_IMAGE,
    CFG_PRICE_PER_MILLION_CHARACTERS, ImageRequest, LlmModelDef, LlmProviderDef, ModelKind,
    OPENAI_CHAT_BACKEND, OPENAI_RESPONSES_BACKEND, SpeechRequest, embed, generate_image, speak,
};

use crate::common::{Reply, serve, serve_capturing};

/// A model of `kind` called `name` of `provider`.
fn model(provider: &LlmProviderDef, name: &str, kind: ModelKind) -> LlmModelDef {
    LlmModelDef::new(provider.id, name).kind(kind)
}

#[tokio::test]
async fn texts_are_posted_to_embeddings_and_read_back_in_order() {
    let (base, mut requests) = serve_capturing(Reply::json(
        r#"{"object":"list","data":[
            {"object":"embedding","index":1,"embedding":[0.0,1.0,0.5]},
            {"object":"embedding","index":0,"embedding":[1.0,0.0,0.25]}
        ],"model":"text-embedding-3-small","usage":{"prompt_tokens":4,"total_tokens":4}}"#,
    ))
    .await
    .unwrap();
    let provider = LlmProviderDef::new("openai", OPENAI_RESPONSES_BACKEND)
        .with(CFG_API_KEY, "sk-test")
        .with(CFG_BASE_URL, base);
    let small = model(&provider, "text-embedding-3-small", ModelKind::Embedding)
        .with(CFG_PRICE_INPUT, 0.02);
    let embedded = embed(
        &provider,
        &small,
        &["first".to_owned(), "second".to_owned()],
    )
    .await
    .unwrap();
    assert_eq!(
        embedded.vectors,
        vec![vec![1.0, 0.0, 0.25], vec![0.0, 1.0, 0.5]]
    );
    // Metered (AS3.5): the reported tokens at the input price.
    assert_eq!(embedded.call.usage.input_tokens, 4);
    assert_eq!(embedded.call.units, 2);
    assert_eq!(embedded.call.kind, ModelKind::Embedding);
    assert!((embedded.call.cost.unwrap() - 4.0 * 0.02 / 1e6).abs() < 1e-12);
    let (path, body) = requests.next_request().await.unwrap();
    assert_eq!(path, "/embeddings");
    assert_eq!(body["model"], "text-embedding-3-small");
    assert_eq!(body["input"], serde_json::json!(["first", "second"]));
}

#[tokio::test]
async fn a_refusal_from_the_host_names_the_provider_and_its_status() {
    let base = serve(Reply::error(
        404,
        r#"{"error":{"message":"model `nope` not found"}}"#,
    ))
    .await
    .unwrap();
    // A local Chat Completions host: no key.
    let provider = LlmProviderDef::new("local", OPENAI_CHAT_BACKEND).with(CFG_BASE_URL, base);
    let nope = model(&provider, "nope", ModelKind::Embedding);
    let err = embed(&provider, &nope, &["x".to_owned()])
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("`local`") && err.contains("404"), "{err}");
    assert!(err.contains("model `nope` not found"), "{err}");

    // The same for the other two calls.
    let image = model(&provider, "nope", ModelKind::Image);
    let err = generate_image(&provider, &image, &ImageRequest::new("a cat"))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("generate an image") && err.contains("404"),
        "{err}"
    );
    let voice = model(&provider, "nope", ModelKind::Speech);
    let err = speak(&provider, &voice, &SpeechRequest::new("hello"))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("could not speak") && err.contains("404"),
        "{err}"
    );
}

#[tokio::test]
async fn a_reply_with_the_wrong_number_of_vectors_is_refused() {
    let base = serve(Reply::json(r#"{"data":[{"index":0,"embedding":[1]}]}"#))
        .await
        .unwrap();
    let provider = LlmProviderDef::new("local", OPENAI_CHAT_BACKEND).with(CFG_BASE_URL, base);
    let m = model(&provider, "m", ModelKind::Embedding);
    let err = embed(&provider, &m, &["a".to_owned(), "b".to_owned()])
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("1 embeddings for 2 texts"), "{err}");
}

#[tokio::test]
async fn an_image_is_generated_with_only_the_options_set_and_costed_per_image() {
    let png = b"\x89PNG\r\n\x1a\n-not-really-pixels";
    let b64 = base64::engine::general_purpose::STANDARD.encode(png);
    let (base, mut requests) = serve_capturing(Reply::json(format!(
        r#"{{"created":1,"data":[{{"b64_json":"{b64}"}}],
            "usage":{{"input_tokens":12,"output_tokens":272,"total_tokens":284}}}}"#
    )))
    .await
    .unwrap();
    let provider = LlmProviderDef::new("openai", OPENAI_RESPONSES_BACKEND)
        .with(CFG_API_KEY, "sk-test")
        .with(CFG_BASE_URL, base);
    let gpt_image =
        model(&provider, "gpt-image-1", ModelKind::Image).with(CFG_PRICE_PER_IMAGE, 0.011);
    let request = ImageRequest {
        size: Some("1024x1024".into()),
        quality: Some("low".into()),
        format: Some("png".into()),
        background: Some("transparent".into()),
        ..ImageRequest::new("a lighthouse at dusk")
    };
    let image = generate_image(&provider, &gpt_image, &request)
        .await
        .unwrap();
    assert_eq!(image.bytes, png);
    assert_eq!(image.format, "png");
    assert_eq!(image.mime_type(), "image/png");
    assert_eq!(image.call.units, 1);
    assert_eq!(image.call.usage.output_tokens, 272);
    assert_eq!(image.call.cost, Some(0.011));

    let (path, body) = requests.next_request().await.unwrap();
    assert_eq!(path, "/images/generations");
    assert_eq!(body["model"], "gpt-image-1");
    assert_eq!(body["prompt"], "a lighthouse at dusk");
    assert_eq!(body["n"], 1);
    assert_eq!(body["size"], "1024x1024");
    assert_eq!(body["quality"], "low");
    assert_eq!(body["output_format"], "png");
    assert_eq!(body["background"], "transparent");
}

#[tokio::test]
async fn an_image_reply_that_is_not_an_image_is_refused() {
    let b64 = base64::engine::general_purpose::STANDARD.encode(b"<html>sorry</html>");
    let base = serve(Reply::json(format!(
        r#"{{"data":[{{"b64_json":"{b64}"}}]}}"#
    )))
    .await
    .unwrap();
    let provider =
        LlmProviderDef::new("local", OPENAI_CHAT_BACKEND).with(CFG_BASE_URL, base.as_str());
    let m = model(&provider, "sd", ModelKind::Image);
    let err = generate_image(&provider, &m, &ImageRequest::new("a cat"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("not an image"), "{err}");

    let base = serve(Reply::json(r#"{"data":[]}"#)).await.unwrap();
    let provider =
        LlmProviderDef::new("local", OPENAI_CHAT_BACKEND).with(CFG_BASE_URL, base.as_str());
    let m = model(&provider, "sd", ModelKind::Image);
    let err = generate_image(&provider, &m, &ImageRequest::new("a cat"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no image"), "{err}");
}

#[tokio::test]
async fn text_is_spoken_and_costed_per_character() {
    let (base, mut requests) = serve_capturing(Reply::typed("audio/wav", "RIFF....WAVEfmt "))
        .await
        .unwrap();
    let provider = LlmProviderDef::new("openai", OPENAI_RESPONSES_BACKEND)
        .with(CFG_API_KEY, "sk-test")
        .with(CFG_BASE_URL, base);
    let tts = model(&provider, "gpt-4o-mini-tts", ModelKind::Speech)
        .with(CFG_PRICE_PER_MILLION_CHARACTERS, 15.0);
    let request = SpeechRequest {
        voice: Some("nova".into()),
        speed: Some(1.25),
        format: Some("wav".into()),
        instructions: Some("Cheerful.".into()),
        ..SpeechRequest::new("Hello there")
    };
    let speech = speak(&provider, &tts, &request).await.unwrap();
    assert_eq!(speech.bytes, b"RIFF....WAVEfmt ");
    assert_eq!(speech.format, "wav");
    assert_eq!(speech.mime_type(), "audio/wav");
    assert_eq!(speech.call.units, 11);
    assert!((speech.call.cost.unwrap() - 11.0 * 15.0 / 1e6).abs() < 1e-12);

    let (path, body) = requests.next_request().await.unwrap();
    assert_eq!(path, "/audio/speech");
    assert_eq!(body["model"], "gpt-4o-mini-tts");
    assert_eq!(body["input"], "Hello there");
    assert_eq!(body["voice"], "nova");
    assert_eq!(body["speed"], 1.25);
    assert_eq!(body["response_format"], "wav");
    assert_eq!(body["instructions"], "Cheerful.");
}

#[tokio::test]
async fn a_speech_reply_of_json_is_refused() {
    let base = serve(Reply::json(r#"{"status":"queued"}"#)).await.unwrap();
    let provider = LlmProviderDef::new("local", OPENAI_CHAT_BACKEND).with(CFG_BASE_URL, base);
    let m = model(&provider, "kokoro", ModelKind::Speech);
    let err = speak(&provider, &m, &SpeechRequest::new("hi"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("not audio"), "{err}");
}

#[tokio::test]
async fn a_model_of_another_kind_or_anthropic_is_refused_before_anything_is_sent() {
    // Nothing listens at port 9: a request would fail differently.
    let provider = LlmProviderDef::new("openai", OPENAI_RESPONSES_BACKEND)
        .with(CFG_API_KEY, "sk-test")
        .with(CFG_BASE_URL, "http://127.0.0.1:9/v1");
    let chat = model(&provider, "gpt-5.1", ModelKind::Chat);
    let err = embed(&provider, &chat, &["x".to_owned()])
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("is a chat model, not an embedding model"),
        "{err}"
    );
    let embedding = model(&provider, "text-embedding-3-small", ModelKind::Embedding);
    let err = speak(&provider, &embedding, &SpeechRequest::new("hi"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("not a speech model"), "{err}");

    let claude = LlmProviderDef::new("claude", sc_llm::ANTHROPIC_BACKEND).with(CFG_API_KEY, "k");
    let image = model(&claude, "any", ModelKind::Image);
    let err = generate_image(&claude, &image, &ImageRequest::new("a cat"))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("Anthropic has no image generation endpoint"),
        "{err}"
    );
    let voice = model(&claude, "any", ModelKind::Speech);
    let err = speak(&claude, &voice, &SpeechRequest::new("hi"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("Anthropic has no speech endpoint"), "{err}");

    // A request that cannot work is refused before the kind is even looked at.
    let err = speak(&provider, &embedding, &SpeechRequest::new(""))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("needs some text"), "{err}");
}
