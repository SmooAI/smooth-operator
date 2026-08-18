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
| `gen_ai.operation.name`       | constant `"chat"`                         | The GenAI operation. The api-prime ingest takes this attribute **verbatim** when present, deriving it from the span name only as a fallback — so the spelling has to match what its queries filter on. |
| `gen_ai.request.model`        | `LlmConfig.model`                         | The model requested for the turn (e.g. `openai/gpt-4o`). |
| `gen_ai.conversation.id`      | the `conversation_id` arg                 | Ties the turn to its conversation. |
| `gen_ai.agent.name`           | constant `"smooth-agent-chat"`            | The agent/persona driving the turn. |
| `smooai.org_id`               | the turn's `org_id` (streaming path)      | Set only when an org is resolved. **Matches the monorepo TS chat handler's attribute exactly**, so the observability studio groups Rust + TS turns by org. |
| `gen_ai.usage.input_tokens`   | `AgentEvent::Completed.prompt_tokens`     | Recorded on completion **only when the engine reported usage** (non-zero). Omitted otherwise — e.g. a mock turn — per the convention's "omit if unknown" rule. |
| `gen_ai.usage.output_tokens`  | `AgentEvent::Completed.completion_tokens` | Same gating as input tokens. |
| `gen_ai.usage.cost_usd`       | `AgentEvent::Completed.cost_usd`          | The turn's cost in USD. **Recorded only when positive** — see "A zero cost is never recorded" below. |

#### A zero cost is never recorded

The engine sources cost from LiteLLM's `x-litellm-response-cost` response
header, which `smooth-operator-core` parses off **both** the streaming and the
non-streaming path — on a stream the headers are read before the SSE body is
consumed, so the value survives — and accumulates onto `AgentEvent::Completed`.

`telemetry::record_cost_usd` drops a non-positive or non-finite value instead of
exporting `0`, because at this layer a zero is **ambiguous**: LiteLLM answers
`x-litellm-response-cost: 0` for a model it has no price for (core's
`parse_gateway_cost` already maps that to `None`), and the local `ModelPricing`
fallback prices any model it doesn't recognise at `0`. So a zero always means
"could not price this turn", never "this turn was free" — exporting it would
render a paid turn as a confident `$0.00`. An absent attribute lets a consumer
say "not measured" instead.

A *positive* value may be either the gateway's authoritative number or the
local `ModelPricing` estimate; the engine sums both onto one `f64`, so the
instrumentation cannot tell them apart. Separating them needs a second field on
`AgentEvent::Completed`.

### `gen_ai.tool` span — one per tool call

For every `AgentEvent::ToolCallComplete` the engine emits, a child span named
**`gen_ai.tool`** (parented to the turn's `gen_ai.chat` span) is opened,
carrying:

| Attribute                    | Source                                      |
| ---------------------------- | ------------------------------------------- |
| `gen_ai.system`              | constant `"smooth-operator"` — see "Child spans repeat their identifiers" below |
| `gen_ai.operation.name`      | constant `"tool"` — must stay exactly this; the ingest's queries filter on `operation_name = 'tool'` |
| `gen_ai.conversation.id`     | the `conversation_id` arg                   |
| `smooai.org_id`              | the turn's `org_id` (streaming path only)   |
| `gen_ai.tool.name`           | `ToolCallComplete.tool_name`                |
| `gen_ai.tool.call.arguments` | the matching `ToolCallStart.arguments`, **redacted** (see below) and length-capped |
| `duration_ms`                | `ToolCallComplete.duration_ms` (wall clock) |
| `is_error`                   | `ToolCallComplete.is_error`                 |
| `otel.status_code` / `otel.status_message` | set to `ERROR` + the tool's error text when `is_error` — so a failed tool call surfaces as an OTLP span with error status |

#### Child spans repeat their identifiers

The OTLP ingest builds each span's attribute set from the **resource** attributes
overlaid with **that span's own** attributes. There is **no inheritance from the
parent span**, so anything only `gen_ai.chat` carries is invisible on its
children. That is why the tool span repeats `gen_ai.system`,
`gen_ai.operation.name`, `gen_ai.conversation.id` and `smooai.org_id` rather
than relying on the turn span.

Omitting `gen_ai.system` did more than lose the conversation join: the ingest's
LLM-event gate keyed on that attribute to decide a span was a GenAI event, so
tool spans were **discarded at ingest** — `operation_name = 'tool'` had zero
rows, all time, while the emitter looked healthy. The ingest now also accepts a
`gen_ai.`-prefixed span name, but the span carries the attributes regardless.

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
