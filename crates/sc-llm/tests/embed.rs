//! The embedding call (TODO-agent-skills AS3.2) against a stub
//! OpenAI-compatible server: what is sent, what is read back, and the
//! refusals.

use sc_llm::{
    CFG_API_KEY, CFG_BASE_URL, LlmProviderDef, OPENAI_CHAT_BACKEND, OPENAI_RESPONSES_BACKEND, embed,
};

use crate::common::{Reply, serve, serve_capturing};

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
    let vectors = embed(
        &provider,
        "text-embedding-3-small",
        &["first".to_owned(), "second".to_owned()],
    )
    .await
    .unwrap();
    assert_eq!(vectors, vec![vec![1.0, 0.0, 0.25], vec![0.0, 1.0, 0.5]]);
    let body = requests.next_body().await.unwrap();
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
    let err = embed(&provider, "nope", &["x".to_owned()])
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("`local`") && err.contains("404"), "{err}");
    assert!(err.contains("model `nope` not found"), "{err}");
}

#[tokio::test]
async fn a_reply_with_the_wrong_number_of_vectors_is_refused() {
    let base = serve(Reply::json(r#"{"data":[{"index":0,"embedding":[1]}]}"#))
        .await
        .unwrap();
    let provider = LlmProviderDef::new("local", OPENAI_CHAT_BACKEND).with(CFG_BASE_URL, base);
    let err = embed(&provider, "m", &["a".to_owned(), "b".to_owned()])
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("1 embeddings for 2 texts"), "{err}");
}
