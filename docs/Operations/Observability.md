# Observability — OpenTelemetry GenAI Tracing

smooth-operator instruments each agent turn with OpenTelemetry spans that
follow the [GenAI semantic conventions](https://opentelemetry.io/docs/specs/semconv/gen-ai/).
This makes our traces interoperate with the smooai monorepo's existing
`gen_ai.*` spans and with the Microsoft Agent Framework — the same attribute
names, so a single trace backend can correlate turns across all of them.

The attribute-name constants and helpers live in
[`smooth-operator/src/telemetry.rs`](../../rust/smooth-operator/src/telemetry.rs).
**Both** turn paths are instrumented with the identical span shape:

- **The production streaming path** —
  [`runner::run_streaming_turn`](../../rust/smooth-operator-server/src/runner.rs)
  (the WS service + lambda drive every real turn through this). Spans are
  materialized after the run, from the collected `AgentEvent` stream, so they
  flow under the process-global OTLP subscriber rather than a spawned task's
  context.
- **The non-streaming reference path** —
  [`KnowledgeChatRuntime::run_turn`](../../rust/smooth-operator/src/runtime.rs).

## What gets emitted

### `gen_ai.chat` span — one per turn

Each turn opens an `info`-level span named **`gen_ai.chat`** that wraps the whole
turn (engine loop + message persistence). It carries:

| Attribute                     | Source                                    | Notes |
| ----------------------------- | ----------------------------------------- | ----- |
| `gen_ai.system`               | constant `"smooth-operator"`        | Identifies the GenAI system. |
| `gen_ai.request.model`        | `LlmConfig.model`                         | The model requested for the turn (e.g. `openai/gpt-4o`). |
| `gen_ai.conversation.id`      | the `conversation_id` arg                 | Ties the turn to its conversation. |
| `gen_ai.agent.name`           | constant `"smooth-agent-chat"`            | The agent/persona driving the turn. |
| `smooai.org_id`               | the turn's `org_id` (streaming path)      | Set only when an org is resolved. **Matches the monorepo TS chat handler's attribute exactly**, so the observability studio groups Rust + TS turns by org. |
| `gen_ai.usage.input_tokens`   | `AgentEvent::Completed.prompt_tokens`     | Recorded on completion **only when the engine reported usage** (non-zero). Omitted otherwise — e.g. a mock turn — per the convention's "omit if unknown" rule. |
| `gen_ai.usage.output_tokens`  | `AgentEvent::Completed.completion_tokens` | Same gating as input tokens. |

### `gen_ai.tool` span — one per tool call

For every `AgentEvent::ToolCallComplete` the engine emits, a child span named
**`gen_ai.tool`** (parented to the turn's `gen_ai.chat` span) is opened,
carrying:

| Attribute                    | Source                                      |
| ---------------------------- | ------------------------------------------- |
| `gen_ai.tool.name`           | `ToolCallComplete.tool_name`                |
| `gen_ai.tool.call.arguments` | the matching `ToolCallStart.arguments`, **redacted** (see below) and length-capped |
| `duration_ms`                | `ToolCallComplete.duration_ms` (wall clock) |
| `is_error`                   | `ToolCallComplete.is_error`                 |
| `otel.status_code` / `otel.status_message` | set to `ERROR` + the tool's error text when `is_error` — so a failed tool call surfaces as an OTLP span with error status |

**Argument redaction.** `telemetry::redact_tool_arguments` parses the JSON args
and replaces the value of any object key whose name looks secret-bearing
(`secret`, `token`, `password`, `api_key`, `authorization`, `bearer`,
`credential`, `access_key`, `private_key`, …) with `"[REDACTED]"` before the
string ever reaches a span. It is a best-effort scrub keyed on argument *names*,
not a value scanner — a secret under an innocuous key still lands (Narc's
value-pattern detection is the deeper net). Non-JSON args pass through as-is;
everything is capped at 2 KiB.

The attribute-name constants (`GEN_AI_SYSTEM`, `GEN_AI_REQUEST_MODEL`,
`SMOOAI_ORG_ID`, …) and the span names (`SPAN_CHAT` = `gen_ai.chat`,
`SPAN_TOOL` = `gen_ai.tool`) are exported from `telemetry.rs` so both turn paths
and any downstream consumer key off the exact same strings.

### Not yet emitted — per-LLM-call inference spans

There is intentionally **no** per-LLM-call child span (`chat {model}` with
per-call `gen_ai.usage.*` + `gen_ai.response.finish_reasons`) yet. Token usage is
only surfaced **aggregated** on the turn span, and finish-reason is not surfaced
at all, because `smooth-operator-core`'s `AgentEvent` stream reports usage only
once (on `Completed`) and carries no finish-reason. Adding a real inference span
requires the engine core to emit per-call usage + finish-reason on its
`LlmResponse` event — a separate `smooth-operator-core` change with the usual
core→server release-ordering implication.

## How `init_telemetry` is gated — no collector needed

`smooth_operator::init_telemetry()` installs the process-global
tracing subscriber. It is **idempotent** (a compare-and-swap guard makes repeat
calls no-ops) and is called once at startup by both binaries:

- the reference server — [`smooth-operator-server/src/main.rs`](../../rust/smooth-operator-server/src/main.rs)
- the lambda — [`smooth-operator-lambda/src/main.rs`](../../rust/smooth-operator-lambda/src/main.rs)

Its behavior depends entirely on one environment variable:

- **`OTEL_EXPORTER_OTLP_ENDPOINT` unset (or empty)** → installs a **local-only**
  `fmt` layer plus an `EnvFilter` (honors `RUST_LOG`, defaults to
  `info,smooth_operator=info`). **No exporter, no collector, no
  network.** This is the path the test suite and a collector-less binary take —
  the spans are still emitted into the `tracing` system (so a test subscriber
  can capture them), they're just not shipped anywhere.
- **`OTEL_EXPORTER_OTLP_ENDPOINT` set** → additionally installs an OTLP
  (gRPC / tonic) span exporter behind a batch span processor, bridged into
  `tracing` via [`tracing-opentelemetry`](https://crates.io/crates/tracing-opentelemetry).
  The OTLP `service.name` resource attribute is set to
  `smooth-operator`. If exporter construction fails (bad endpoint, etc.)
  it logs a warning and falls back to local-only logging rather than panicking —
  a misconfigured collector never takes the agent down.

Because the exporter is gated, **tests never need a live collector**. Each turn
path has a telemetry test that installs its own capturing `tracing` layer, runs a
`MockLlmClient` turn, and asserts on the recorded `gen_ai.chat` / `gen_ai.tool`
span fields directly:

- non-streaming path — [`smooth-operator/tests/telemetry.rs`](../../rust/smooth-operator/tests/telemetry.rs)
- production streaming path — [`smooth-operator-server/tests/telemetry.rs`](../../rust/smooth-operator-server/tests/telemetry.rs)
  (asserts `smooai.org_id` + redacted tool arguments on a real `run_streaming_turn`)

## Pointing at a collector

Set the OTLP endpoint before starting the server or deploying the lambda:

```bash
# Local OpenTelemetry Collector (gRPC OTLP receiver on 4317)
export OTEL_EXPORTER_OTLP_ENDPOINT="http://localhost:4317"
cargo run -p smooai-smooth-operator-server

# Tune log verbosity independently of OTLP export
export RUST_LOG="info,smooth_operator=debug"
```

For the lambda, set `OTEL_EXPORTER_OTLP_ENDPOINT` (and optionally `RUST_LOG`) in
the function's environment. With it unset, the lambda logs locally to CloudWatch
via the `fmt` layer and emits no OTLP traffic.

> The exporter uses the OTLP **gRPC** transport (tonic). Point the endpoint at a
> collector's gRPC OTLP receiver (default port `4317`), not the HTTP receiver
> (`4318`).

---

**In this vault:** [[Home]] · [[Agents, Tools, and Workflows]] · [[Evals]] · [[Configuration]] · [[Architecture Overview]]
