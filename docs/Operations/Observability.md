# Observability — OpenTelemetry GenAI Tracing

smooth-operator instruments each agent turn with OpenTelemetry spans that
follow the [GenAI semantic conventions](https://opentelemetry.io/docs/specs/semconv/gen-ai/).
This makes our traces interoperate with the smooai monorepo's existing
`gen_ai.*` spans and with the Microsoft Agent Framework — the same attribute
names, so a single trace backend can correlate turns across all of them.

The implementation lives in
[`smooth-operator/src/telemetry.rs`](../../rust/smooth-operator/src/telemetry.rs)
and the instrumentation point is
[`KnowledgeChatRuntime::run_turn`](../../rust/smooth-operator/src/runtime.rs).

## What gets emitted

### `gen_ai.chat` span — one per turn

`run_turn` opens an `info`-level span named **`gen_ai.chat`** that wraps the
whole turn (engine loop + message persistence). It carries:

| Attribute                     | Source                                    | Notes |
| ----------------------------- | ----------------------------------------- | ----- |
| `gen_ai.system`               | constant `"smooth-operator"`        | Identifies the GenAI system. |
| `gen_ai.request.model`        | `LlmConfig.model`                         | The model requested for the turn (e.g. `openai/gpt-4o`). |
| `gen_ai.conversation.id`      | the `conversation_id` arg                 | Ties the turn to its conversation. |
| `gen_ai.usage.input_tokens`   | `AgentEvent::Completed.prompt_tokens`     | Recorded on completion **only when the engine reported usage** (non-zero). Omitted otherwise — e.g. a mock turn — per the convention's "omit if unknown" rule. |
| `gen_ai.usage.output_tokens`  | `AgentEvent::Completed.completion_tokens` | Same gating as input tokens. |

### `gen_ai.tool` span — one per tool call

For every `AgentEvent::ToolCallComplete` the engine emits, `run_turn` opens a
child span named **`gen_ai.tool`** (parented to the turn's `gen_ai.chat` span)
carrying:

| Attribute            | Source                                      |
| -------------------- | ------------------------------------------- |
| `gen_ai.tool.name`   | `ToolCallComplete.tool_name`                |
| `duration_ms`        | `ToolCallComplete.duration_ms` (wall clock) |
| `is_error`           | `ToolCallComplete.is_error`                 |

The attribute-name constants (`GEN_AI_SYSTEM`, `GEN_AI_REQUEST_MODEL`, …) and the
span names (`SPAN_CHAT` = `gen_ai.chat`, `SPAN_TOOL` = `gen_ai.tool`) are
exported from `telemetry.rs` so downstream consumers key off the exact same
strings.

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

Because the exporter is gated, **tests never need a live collector**: the
telemetry test ([`smooth-operator/tests/telemetry.rs`](../../rust/smooth-operator/tests/telemetry.rs))
installs its own capturing `tracing` layer, runs a `MockLlmClient` turn, and
asserts on the recorded `gen_ai.chat` / `gen_ai.tool` span fields directly.

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
