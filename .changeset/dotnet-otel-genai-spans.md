---
"@smooai/smooth-operator": patch
---

feat(dotnet): emit `gen_ai.chat` / `gen_ai.tool` OpenTelemetry spans on the turn path (th-873430, M parity)

The .NET server emitted no OpenTelemetry spans, so its turns were invisible in the observability
studio next to the Rust server's. This brings it to parity: `TurnRunner.RunAsync` now opens a
`gen_ai.chat` activity per turn — carrying `gen_ai.system`, `gen_ai.request.model`,
`gen_ai.conversation.id`, `gen_ai.agent.name`, and, on completion, the `gen_ai.usage.input_tokens` /
`gen_ai.usage.output_tokens` counts — and each tool call opens a child `gen_ai.tool` activity with
`gen_ai.tool.name` and the (secret-redacted) `gen_ai.tool.call.arguments`. The attribute keys and
span names are byte-identical to the Rust `smooth_operator::telemetry` module.

Spans flow from a named `ActivitySource` ("smooth-operator"); the host wires an OTLP exporter only
when `OTEL_EXPORTER_OTLP_ENDPOINT` is set (the same env gate as the Rust host's `init_telemetry`),
so a collector-less run has zero telemetry overhead. `gen_ai.request.model` is read from the same
`SMOOTH_AGENT_MODEL` → `SMOOAI_MODEL` → `SMOOTH_MODEL` env chain the host resolves the gateway model
from. An xUnit test mirrors `tests/telemetry.rs` with an in-memory `ActivityListener`.
