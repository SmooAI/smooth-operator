---
'@smooai/smooth-operator': patch
---

SMOODEV-2328 — OpenTelemetry GenAI agent spans on the production streaming path.

The reference server drives every real turn through `runner::run_streaming_turn`,
which previously emitted **no** `gen_ai.*` spans (only the secondary
`KnowledgeChatRuntime::run_turn` was instrumented). Both paths now emit the
identical span shape so agent turns flow via OTLP to the observability studio:

- Per-turn `gen_ai.chat` span now also carries `gen_ai.agent.name` and — on the
  streaming path — `smooai.org_id` (matching the monorepo TS chat handler's
  attribute exactly, so the studio groups Rust + TS turns by org), alongside the
  existing system / model / conversation.id and aggregated token usage.
- Per-tool `gen_ai.tool` child span now carries the tool's `gen_ai.tool.call.arguments`
  (redacted via `telemetry::redact_tool_arguments`, which scrubs secret-named JSON
  keys and caps length) plus an `otel.status_code`=`ERROR` + message on failure,
  in addition to the existing tool name / latency / is_error.

OTLP export was already wired end-to-end (`init_telemetry()` in both server and
lambda `main.rs`, gated on `OTEL_EXPORTER_OTLP_ENDPOINT`). No per-LLM-call
inference span yet — that needs `smooth-operator-core` to emit per-call usage +
finish-reason, tracked separately.
