//! OpenTelemetry GenAI instrumentation for the agent turn.
//!
//! This module sets up `tracing` → OpenTelemetry export and defines the span
//! attribute names from the **GenAI semantic conventions** so the traces this
//! crate emits interoperate with the smooai monorepo's existing `gen_ai.*`
//! spans and the Microsoft Agent Framework.
//!
//! ## Span shape
//! [`KnowledgeChatRuntime::run_turn`](crate::runtime::KnowledgeChatRuntime::run_turn)
//! opens a span named [`SPAN_CHAT`] (`gen_ai.chat`) per turn, carrying:
//!
//! - [`GEN_AI_SYSTEM`] (`gen_ai.system`) = [`SYSTEM_NAME`]
//! - [`GEN_AI_REQUEST_MODEL`] (`gen_ai.request.model`) — the configured model
//! - [`GEN_AI_CONVERSATION_ID`] (`gen_ai.conversation.id`) — the conversation id
//! - on completion, [`GEN_AI_USAGE_INPUT_TOKENS`] /
//!   [`GEN_AI_USAGE_OUTPUT_TOKENS`] when the engine reported token usage.
//!
//! Each tool call the engine emits opens a child span named [`SPAN_TOOL`]
//! (`gen_ai.tool`) carrying [`GEN_AI_TOOL_NAME`] (`gen_ai.tool.name`) and the
//! tool's wall-clock `duration_ms`.
//!
//! ## Exporter gating (no collector needed for tests/binaries)
//! [`init_telemetry`] installs an OTLP exporter **only** when
//! `OTEL_EXPORTER_OTLP_ENDPOINT` is set. When it is unset, a local-only `fmt`
//! layer is installed instead, so the binary, the lambda, and the test suite
//! run with zero external dependencies. The function is idempotent — calling it
//! more than once is a no-op after the first successful install.

use std::sync::atomic::{AtomicBool, Ordering};

// ---------------------------------------------------------------------------
// GenAI semantic-convention attribute keys.
//
// These are the canonical OpenTelemetry GenAI attribute names. Keeping them as
// named constants means the runtime instrumentation and any downstream
// consumer agree on the exact strings (the smooai monorepo + MS Agent
// Framework key off these).
// ---------------------------------------------------------------------------

/// `gen_ai.system` — the GenAI system / provider name.
pub const GEN_AI_SYSTEM: &str = "gen_ai.system";
/// `gen_ai.request.model` — the model requested for the turn.
pub const GEN_AI_REQUEST_MODEL: &str = "gen_ai.request.model";
/// `gen_ai.conversation.id` — the conversation this turn belongs to.
pub const GEN_AI_CONVERSATION_ID: &str = "gen_ai.conversation.id";
/// `gen_ai.usage.input_tokens` — prompt tokens consumed by the turn.
pub const GEN_AI_USAGE_INPUT_TOKENS: &str = "gen_ai.usage.input_tokens";
/// `gen_ai.usage.output_tokens` — completion tokens produced by the turn.
pub const GEN_AI_USAGE_OUTPUT_TOKENS: &str = "gen_ai.usage.output_tokens";
/// `gen_ai.tool.name` — the name of an invoked tool.
pub const GEN_AI_TOOL_NAME: &str = "gen_ai.tool.name";
/// `gen_ai.tool.call.arguments` — the (redacted) JSON arguments passed to a tool.
pub const GEN_AI_TOOL_ARGUMENTS: &str = "gen_ai.tool.call.arguments";
/// `gen_ai.agent.name` — the agent/persona driving the turn.
pub const GEN_AI_AGENT_NAME: &str = "gen_ai.agent.name";
/// `smooai.org_id` — the owning org. Matches the monorepo TS chat handler's
/// attribute exactly so the observability studio groups Rust + TS turns by org.
pub const SMOOAI_ORG_ID: &str = "smooai.org_id";

/// `otel.status_code` — the tracing-opentelemetry magic field that maps onto an
/// OTLP span's status. Set to `"ERROR"` on a failed tool call.
pub const OTEL_STATUS_CODE: &str = "otel.status_code";
/// `otel.status_message` — the tracing-opentelemetry magic field carrying the
/// OTLP span status description (the tool error text on failure).
pub const OTEL_STATUS_MESSAGE: &str = "otel.status_message";

/// The value emitted for [`GEN_AI_SYSTEM`] — identifies these traces as coming
/// from this agent runtime.
pub const SYSTEM_NAME: &str = "smooth-operator";

/// The agent name both the reference streaming path
/// ([`runner::run_streaming_turn`](../../smooth_operator_server/runner/index.html))
/// and [`KnowledgeChatRuntime`](crate::runtime::KnowledgeChatRuntime) build their
/// `AgentConfig` with; emitted as [`GEN_AI_AGENT_NAME`] on the turn span.
pub const AGENT_NAME: &str = "smooth-agent-chat";

/// Max length of a serialized tool-arguments string recorded on a span, so a
/// pathological argument blob can't bloat span export.
const MAX_TOOL_ARGS_LEN: usize = 2048;

/// Span name for the per-turn GenAI chat span (`gen_ai.chat`).
pub const SPAN_CHAT: &str = "gen_ai.chat";
/// Span name for a per-tool-call child span (`gen_ai.tool`).
pub const SPAN_TOOL: &str = "gen_ai.tool";

/// Redact a tool's serialized JSON arguments for span recording.
///
/// Tool arguments can carry credentials a host tool needs (an API key, a bearer
/// token, a password). We never want those in a span exported to ClickHouse, so
/// this walks the parsed JSON and replaces the value of any object key whose name
/// looks secret-bearing (`secret`, `token`, `password`, `api_key`, `apikey`,
/// `authorization`, `bearer`, `credential`, `access_key`, `private_key` — case-
/// insensitive, substring match) with `"[REDACTED]"`. Non-JSON (or unparseable)
/// input is passed through as-is. The result is always length-capped at
/// [`MAX_TOOL_ARGS_LEN`].
///
/// This is a best-effort scrub keyed on argument *names*, not a secret scanner —
/// a secret passed under an innocuous key still lands. Narc's value-pattern
/// detection is the deeper net; this keeps the obvious cases out of traces.
#[must_use]
pub fn redact_tool_arguments(arguments: &str) -> String {
    let redacted = match serde_json::from_str::<serde_json::Value>(arguments) {
        Ok(mut value) => {
            redact_json_in_place(&mut value);
            value.to_string()
        }
        // Not JSON — record the raw string; still length-capped below.
        Err(_) => arguments.to_string(),
    };
    truncate(&redacted, MAX_TOOL_ARGS_LEN)
}

/// True if an object key name looks like it holds a secret value.
fn is_secret_key(key: &str) -> bool {
    const NEEDLES: [&str; 10] = [
        "secret",
        "token",
        "password",
        "api_key",
        "apikey",
        "authorization",
        "bearer",
        "credential",
        "access_key",
        "private_key",
    ];
    let lower = key.to_ascii_lowercase();
    NEEDLES.iter().any(|n| lower.contains(n))
}

/// Recursively replace secret-named object values with `"[REDACTED]"`.
fn redact_json_in_place(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if is_secret_key(k) {
                    *v = serde_json::Value::String("[REDACTED]".to_string());
                } else {
                    redact_json_in_place(v);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items.iter_mut() {
                redact_json_in_place(item);
            }
        }
        _ => {}
    }
}

/// Truncate to at most `max` bytes on a char boundary, appending `…` when cut.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Env var that, when set, switches [`init_telemetry`] from the local-only
/// `fmt` layer to a real OTLP exporter pointed at the given endpoint.
pub const OTLP_ENDPOINT_ENV: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";

/// Set once `init_telemetry` has successfully installed a global subscriber, so
/// subsequent calls are no-ops (idempotent).
static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Initialize tracing → OpenTelemetry for the process.
///
/// - When `OTEL_EXPORTER_OTLP_ENDPOINT` is set, installs an OTLP (gRPC/tonic)
///   span exporter via a batch span processor and bridges `tracing` spans to it
///   with [`tracing_opentelemetry`], alongside a `fmt` layer for local logs.
/// - When it is unset, installs only a local `fmt` layer — no exporter, no
///   collector needed. This is the path the test suite and a collector-less
///   binary take.
///
/// Idempotent: the first successful call installs the global subscriber;
/// later calls return immediately. Safe to call from both the server `main`
/// and the lambda `main` at startup.
///
/// Returns `true` if this call performed the install, `false` if telemetry was
/// already initialized.
pub fn init_telemetry() -> bool {
    // Compare-and-swap so concurrent callers race exactly one winner.
    if INITIALIZED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return false;
    }

    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{EnvFilter, Registry};

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,smooth_operator=info"));
    let fmt_layer = tracing_subscriber::fmt::layer();

    match std::env::var(OTLP_ENDPOINT_ENV) {
        Ok(endpoint) if !endpoint.trim().is_empty() => {
            // OTLP path: build the exporter + tracer provider, then bridge
            // `tracing` into it. If exporter construction fails, fall back to
            // local-only logging rather than panicking — a bad endpoint should
            // never take down the agent.
            match build_otlp_layer(&endpoint) {
                Ok(otel_layer) => {
                    Registry::default()
                        .with(env_filter)
                        .with(fmt_layer)
                        .with(otel_layer)
                        .init();
                    tracing::info!(endpoint = %endpoint, "telemetry: OTLP exporter installed");
                }
                Err(e) => {
                    Registry::default().with(env_filter).with(fmt_layer).init();
                    tracing::warn!(
                        error = %e,
                        endpoint = %endpoint,
                        "telemetry: OTLP exporter init failed; using local-only logging"
                    );
                }
            }
        }
        _ => {
            // No endpoint configured — local-only logging. No collector needed.
            Registry::default().with(env_filter).with(fmt_layer).init();
            tracing::debug!(
                "telemetry: {OTLP_ENDPOINT_ENV} unset; local-only logging (no OTLP exporter)"
            );
        }
    }

    true
}

/// Build the `tracing-opentelemetry` layer backed by an OTLP exporter at
/// `endpoint`. Factored out so [`init_telemetry`] can fall back cleanly if it
/// errors.
///
/// Returns a boxed `Layer` so the two `init_telemetry` arms have a single type.
fn build_otlp_layer<S>(
    endpoint: &str,
) -> anyhow::Result<Box<dyn tracing_subscriber::Layer<S> + Send + Sync>>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a> + Send + Sync,
{
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry::KeyValue;
    use opentelemetry_otlp::WithExportConfig;
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use opentelemetry_sdk::Resource;
    use tracing_subscriber::Layer;

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()?;

    let resource = Resource::builder()
        .with_attribute(KeyValue::new("service.name", SYSTEM_NAME))
        .build();

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build();

    let tracer = provider.tracer(SYSTEM_NAME);

    // Keep the provider alive for the process lifetime so the batch processor
    // keeps flushing. It's intentionally leaked (process-global, like the
    // installed subscriber) rather than dropped at the end of this fn.
    opentelemetry::global::set_tracer_provider(provider);

    Ok(tracing_opentelemetry::layer().with_tracer(tracer).boxed())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_secret_named_keys_but_keeps_the_rest() {
        let args = r#"{"query":"weather","api_key":"sk-live-123","nested":{"authToken":"abc"}}"#;
        let out = redact_tool_arguments(args);
        assert!(
            out.contains("\"query\":\"weather\""),
            "non-secret kept: {out}"
        );
        assert!(
            !out.contains("sk-live-123"),
            "api_key value scrubbed: {out}"
        );
        assert!(
            !out.contains("abc"),
            "nested authToken value scrubbed: {out}"
        );
        assert_eq!(
            out.matches("[REDACTED]").count(),
            2,
            "both secrets redacted: {out}"
        );
    }

    #[test]
    fn passes_through_non_json_and_caps_length() {
        assert_eq!(redact_tool_arguments("not json"), "not json");
        let long = "x".repeat(MAX_TOOL_ARGS_LEN + 100);
        let out = redact_tool_arguments(&long);
        assert!(
            out.len() <= MAX_TOOL_ARGS_LEN + 4,
            "capped near MAX: {}",
            out.len()
        );
        assert!(out.ends_with('…'), "truncation marker appended");
    }
}
