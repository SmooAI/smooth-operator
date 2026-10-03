//! Assistant history must not replay `reasoning_content` to Groq (SMOODEV-3630).
//!
//! Core replays the model's reasoning on assistant history because thinking-mode
//! upstreams (DeepSeek, Kimi) 400 without it. Groq 400s WITH it
//! (`'messages.3' : property 'reasoning_content' is unsupported`), and the
//! gateway then quietly serves the turn from a fallback model. Core 1.14.1 strips
//! it for `groq-*`; this workspace sat on 1.10.0, so every multi-turn Groq row of
//! the nightly evals graded the fallback instead of Groq. These tests pin the
//! wire behaviour of the core this workspace actually resolves, so a floor that
//! predates the fix fails here instead of in prod error tracking.

use smooth_operator_core::llm::{ApiFormat, RetryPolicy};
use smooth_operator_core::{LlmClient, LlmConfig, Message};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn config(server: &MockServer, model: &str) -> LlmConfig {
    LlmConfig {
        api_url: server.uri(),
        api_key: "not-a-real-key".into(),
        model: model.into(),
        max_tokens: 64,
        temperature: 0.0,
        retry_policy: RetryPolicy::default(),
        api_format: ApiFormat::OpenAiCompat,
    }
}

/// Send a history whose assistant message carries reasoning, and return the
/// `messages` array of the request body the upstream received.
async fn sent_messages(model: &str) -> Vec<serde_json::Value> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let mut prior = Message::assistant("The return window is 17 days.");
    prior.reasoning_content = Some("The KB says 17 days.".into());
    let history = [
        Message::system("You are a support agent."),
        Message::user("What is the return window?"),
        prior,
        Message::user("And is there a gift card?"),
    ];
    let refs: Vec<&Message> = history.iter().collect();

    LlmClient::new(config(&server, model))
        .chat(&refs, &[])
        .await
        .expect("chat against the mock upstream");

    let requests = server
        .received_requests()
        .await
        .expect("request recording is on");
    let body: serde_json::Value =
        serde_json::from_slice(&requests[0].body).expect("JSON request body");
    body["messages"].as_array().expect("messages array").clone()
}

#[tokio::test]
async fn groq_history_carries_no_reasoning_content() {
    let messages = sent_messages("groq-gpt-oss-120b").await;
    assert_eq!(messages.len(), 4, "the whole history was sent");
    for (i, m) in messages.iter().enumerate() {
        assert!(
            m.get("reasoning_content").is_none(),
            "messages.{i} replays reasoning_content to Groq, which 400s on it: {m}"
        );
    }
}

/// The strip is Groq-only: thinking-mode upstreams still need the replay, so
/// this guards against "fixing" Groq by dropping reasoning for everyone.
#[tokio::test]
async fn thinking_mode_history_still_replays_reasoning_content() {
    let messages = sent_messages("deepseek-v4-flash").await;
    assert_eq!(messages[2]["role"], "assistant");
    assert_eq!(messages[2]["reasoning_content"], "The KB says 17 days.");
}
