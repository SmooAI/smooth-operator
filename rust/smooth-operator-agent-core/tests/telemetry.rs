//! TDD test for the OpenTelemetry GenAI instrumentation on `run_turn`.
//!
//! This drives a real smooth-operator agent loop with a `MockLlmClient` (no
//! network, no API key, no live OTLP collector) and asserts — via a capturing
//! `tracing` subscriber — that:
//!
//! 1. A `gen_ai.chat` span is recorded carrying the GenAI semantic-convention
//!    attributes `gen_ai.system`, `gen_ai.request.model`, and
//!    `gen_ai.conversation.id`.
//! 2. A per-tool span (`gen_ai.tool`) is recorded with `gen_ai.tool.name` when
//!    a tool fires during the turn.
//!
//! The capturing layer records each span's name plus a flattened map of its
//! string/int field values, so the assertions read the exact attribute values.

#![allow(clippy::missing_panics_doc)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use smooth_operator::llm_provider::MockLlmClient;
use smooth_operator::{Document, DocumentType, LlmConfig};
use smooth_operator_agent_adapter_memory::InMemoryStorageAdapter;
use smooth_operator_agent_core::runtime::KnowledgeChatRuntime;
use smooth_operator_agent_core::StorageAdapter;

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing::Subscriber;
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

/// One captured span: its name + the flattened field values recorded on it
/// (both at creation via `Attributes` and later via `record`).
#[derive(Debug, Clone, Default)]
struct CapturedSpan {
    name: String,
    fields: HashMap<String, String>,
}

/// Shared, thread-safe sink the capturing layer appends every span into.
type SpanSink = Arc<Mutex<Vec<CapturedSpan>>>;

/// A `tracing` layer that records every span's name and string/int fields into
/// a shared `Vec`, so a test can assert on the GenAI attributes without a live
/// OTLP collector.
struct CapturingLayer {
    sink: SpanSink,
    /// Maps span id -> index into `sink`, so `on_record` updates the right entry.
    index: Arc<Mutex<HashMap<u64, usize>>>,
}

/// Visitor that flattens recorded fields to `String` values.
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

fn seeded_storage() -> Arc<InMemoryStorageAdapter> {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let kb = storage.knowledge();
    kb.ingest(Document::new(
        "SmooAI returns are accepted within 30 days of delivery for a full refund.",
        "policies/returns.md",
        DocumentType::Documentation,
    ))
    .expect("ingest returns policy");
    storage
}

fn test_llm() -> LlmConfig {
    // Never makes a real call — the mock intercepts every request.
    LlmConfig::openrouter("not-a-real-key").with_model("openai/gpt-4o")
}

#[tokio::test]
async fn run_turn_records_gen_ai_spans() {
    let sink: SpanSink = Arc::new(Mutex::new(Vec::new()));
    let layer = CapturingLayer {
        sink: Arc::clone(&sink),
        index: Arc::new(Mutex::new(HashMap::new())),
    };

    // Scope the subscriber to this turn so it can't leak into other tests. The
    // guard keeps it as the thread-local default across the awaits below;
    // `#[tokio::test]` runs the current-thread runtime, so all of `run_turn`'s
    // polling stays on this thread and the subscriber stays active.
    let subscriber = tracing_subscriber::registry().with(layer);
    let _guard = tracing::subscriber::set_default(subscriber);

    let storage = seeded_storage();

    // Script the mock: turn 1 emits a knowledge_search tool call, turn 2 the
    // final answer. This guarantees a tool span fires.
    let mock = MockLlmClient::new();
    mock.push_tool_call(
        "call_kb_1",
        "knowledge_search",
        serde_json::json!({ "query": "return policy refund window" }),
    )
    .push_text("Items are accepted within 30 days of delivery for a full refund.");

    let runtime =
        KnowledgeChatRuntime::new(storage, test_llm()).with_llm_provider(Arc::new(mock.clone()));

    // Run the turn under the capturing subscriber (active via `_guard`).
    runtime
        .run_turn("conv-otel-1", "What is the return policy?")
        .await
        .expect("run_turn");

    let spans = sink.lock().expect("sink poisoned").clone();

    // (1) The top-level GenAI chat span exists with the conventional attributes.
    let chat = spans
        .iter()
        .find(|s| s.name == "gen_ai.chat")
        .unwrap_or_else(|| panic!("expected a `gen_ai.chat` span; got: {spans:#?}"));

    assert_eq!(
        chat.fields.get("gen_ai.system").map(String::as_str),
        Some("smooth-operator-agent"),
        "gen_ai.system should be the system name; span fields: {:?}",
        chat.fields
    );
    assert_eq!(
        chat.fields.get("gen_ai.request.model").map(String::as_str),
        Some("openai/gpt-4o"),
        "gen_ai.request.model should be the configured model; span fields: {:?}",
        chat.fields
    );
    assert_eq!(
        chat.fields
            .get("gen_ai.conversation.id")
            .map(String::as_str),
        Some("conv-otel-1"),
        "gen_ai.conversation.id should be the conversation id; span fields: {:?}",
        chat.fields
    );

    // (2) A tool span fired for the knowledge_search call.
    let tool = spans
        .iter()
        .find(|s| s.name == "gen_ai.tool")
        .unwrap_or_else(|| panic!("expected a `gen_ai.tool` span; got: {spans:#?}"));
    assert_eq!(
        tool.fields.get("gen_ai.tool.name").map(String::as_str),
        Some("knowledge_search"),
        "gen_ai.tool.name should name the fired tool; span fields: {:?}",
        tool.fields
    );
}
