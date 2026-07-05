//! Telemetry coverage for the PRODUCTION streaming path (`run_streaming_turn`).
//!
//! The reference server drives turns through `runner::run_streaming_turn` (not
//! `KnowledgeChatRuntime::run_turn`), so this asserts — via a capturing `tracing`
//! subscriber, no live OTLP collector — that a real streaming turn emits:
//!
//! 1. A `gen_ai.chat` turn span carrying `gen_ai.system`, `gen_ai.request.model`,
//!    `gen_ai.conversation.id`, `gen_ai.agent.name`, and `smooai.org_id` (the
//!    monorepo TS chat handler's org attribute, so the studio groups by org).
//! 2. A child `gen_ai.tool` span carrying `gen_ai.tool.name` and the (redacted)
//!    `gen_ai.tool.call.arguments` the model passed.

#![allow(clippy::missing_panics_doc)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::json;
use tokio::sync::mpsc::unbounded_channel;

use smooth_operator::access_control::AccessContext;
use smooth_operator::adapter::StorageAdapter;
use smooth_operator_adapter_memory::InMemoryStorageAdapter;
use smooth_operator_core::llm::StreamEvent;
use smooth_operator_core::llm_provider::MockLlmClient;
use smooth_operator_core::{Document, DocumentType, LlmConfig};
use smooth_operator_server::runner::{self, TurnRequest};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing::Subscriber;
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

/// One captured span: its name + flattened field values (creation + `record`).
#[derive(Debug, Clone, Default)]
struct CapturedSpan {
    name: String,
    fields: HashMap<String, String>,
}

type SpanSink = Arc<Mutex<Vec<CapturedSpan>>>;

/// Records every span's name and string/int fields into a shared `Vec` so a test
/// can assert on GenAI attributes without a live OTLP collector.
struct CapturingLayer {
    sink: SpanSink,
    index: Arc<Mutex<HashMap<u64, usize>>>,
}

struct FieldVisitor<'a>(&'a mut HashMap<String, String>);

impl Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{value:?}"));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
}

impl<S> Layer<S> for CapturingLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, _ctx: Context<'_, S>) {
        let mut fields = HashMap::new();
        attrs.record(&mut FieldVisitor(&mut fields));
        let captured = CapturedSpan {
            name: attrs.metadata().name().to_string(),
            fields,
        };
        let mut sink = self.sink.lock().expect("sink poisoned");
        let idx = sink.len();
        sink.push(captured);
        self.index
            .lock()
            .expect("index poisoned")
            .insert(id.into_u64(), idx);
    }

    fn on_record(&self, id: &Id, values: &tracing::span::Record<'_>, _ctx: Context<'_, S>) {
        let idx = {
            let index = self.index.lock().expect("index poisoned");
            index.get(&id.into_u64()).copied()
        };
        if let Some(idx) = idx {
            let mut sink = self.sink.lock().expect("sink poisoned");
            if let Some(entry) = sink.get_mut(idx) {
                values.record(&mut FieldVisitor(&mut entry.fields));
            }
        }
    }
}

fn seeded_storage() -> Arc<dyn StorageAdapter> {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    storage
        .knowledge()
        .ingest(Document::new(
            "Returns are accepted within 30 days for a full refund.",
            "policies/returns.md",
            DocumentType::Documentation,
        ))
        .expect("ingest doc");
    storage
}

fn mock_llm() -> LlmConfig {
    LlmConfig::openrouter("not-a-real-key").with_model("openai/gpt-4o")
}

#[tokio::test]
async fn streaming_turn_emits_gen_ai_spans_with_org_and_tool_args() {
    let sink: SpanSink = Arc::new(Mutex::new(Vec::new()));
    let layer = CapturingLayer {
        sink: Arc::clone(&sink),
        index: Arc::new(Mutex::new(HashMap::new())),
    };
    // `#[tokio::test]` uses the current-thread runtime, so the spawned event
    // translator polls on this same thread and sees the thread-local subscriber.
    let subscriber = tracing_subscriber::registry().with(layer);
    let _guard = tracing::subscriber::set_default(subscriber);

    // Script the mock for the STREAMING path: turn 1 streams a knowledge_search
    // tool call (with args), turn 2 streams the final answer. (The non-streaming
    // `push_tool_call` helper doesn't drive `run_with_channel`.)
    let mock = MockLlmClient::new();
    mock.push_stream(vec![
        StreamEvent::ToolCallStart {
            index: 0,
            id: "call_kb_1".into(),
            name: "knowledge_search".into(),
        },
        StreamEvent::ToolCallArgumentsDelta {
            index: 0,
            arguments_chunk: json!({ "query": "return policy refund window" }).to_string(),
        },
        StreamEvent::Done {
            finish_reason: "tool_calls".into(),
        },
    ])
    .push_stream(vec![
        StreamEvent::Delta {
            content: "Items are accepted within 30 days for a full refund.".into(),
        },
        StreamEvent::Done {
            finish_reason: "stop".into(),
        },
    ]);

    let (tx, mut rx) = unbounded_channel::<serde_json::Value>();
    runner::run_streaming_turn(
        TurnRequest {
            storage: seeded_storage(),
            llm: mock_llm(),
            max_iterations: 4,
            conversation_id: "conv-otel-srv",
            request_id: "req-otel-srv",
            user_message: "what is the return policy?",
            access: AccessContext::anonymous(),
            llm_provider: Some(Arc::new(mock.clone())),
            reranker: None,
            confirmation: None,
            interactions: None,
            tool_provider: None,
            system_prompt: None,
            org_id: Some("org-telemetry".to_string()),
            gateway_key: None,
            workflow: None,
            judge: None,
            greeting_section: None,
            enabled_tools: None,
            auth_gate: None,
            tool_configs: None,
            extensions: None,
        },
        &tx,
    )
    .await
    .expect("run_streaming_turn");
    drop(tx);
    while rx.try_recv().is_ok() {}

    let spans = sink.lock().expect("sink poisoned").clone();

    // (1) The turn span carries system, model, conversation, agent, and org.
    let chat = spans
        .iter()
        .find(|s| s.name == "gen_ai.chat")
        .unwrap_or_else(|| panic!("expected a `gen_ai.chat` span; got: {spans:#?}"));
    assert_eq!(
        chat.fields.get("gen_ai.system").map(String::as_str),
        Some("smooth-operator")
    );
    assert_eq!(
        chat.fields.get("gen_ai.request.model").map(String::as_str),
        Some("openai/gpt-4o")
    );
    assert_eq!(
        chat.fields
            .get("gen_ai.conversation.id")
            .map(String::as_str),
        Some("conv-otel-srv")
    );
    assert_eq!(
        chat.fields.get("gen_ai.agent.name").map(String::as_str),
        Some("smooth-agent-chat")
    );
    assert_eq!(
        chat.fields.get("smooai.org_id").map(String::as_str),
        Some("org-telemetry"),
        "smooai.org_id groups the studio by org; span fields: {:?}",
        chat.fields
    );

    // (2) A child tool span with the tool name + redacted arguments.
    let tool = spans
        .iter()
        .find(|s| s.name == "gen_ai.tool")
        .unwrap_or_else(|| panic!("expected a `gen_ai.tool` span; got: {spans:#?}"));
    assert_eq!(
        tool.fields.get("gen_ai.tool.name").map(String::as_str),
        Some("knowledge_search")
    );
    let args = tool
        .fields
        .get("gen_ai.tool.call.arguments")
        .map(String::as_str)
        .unwrap_or_default();
    assert!(
        args.contains("return policy refund window"),
        "tool arguments should carry the model's query; got: {args:?}"
    );
}
