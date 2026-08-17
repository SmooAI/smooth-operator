---
"@smooai/smooth-operator": patch
---

feat(python): emit gen_ai OpenTelemetry spans on the Python server's turn/tool path (M parity)

The Python server emitted no OpenTelemetry spans while the Rust server emits
`gen_ai.chat` / `gen_ai.tool` spans on the turn path. `TurnRunner.run` now opens a
`gen_ai.chat` turn span (`gen_ai.system`, `gen_ai.request.model`,
`gen_ai.conversation.id`, `gen_ai.agent.name`, `smooai.org_id`, and, on completion,
`gen_ai.usage.input_tokens` / `output_tokens`) and a child `gen_ai.tool` span per
tool call (`gen_ai.tool.name` + redacted `gen_ai.tool.call.arguments`), mirroring the
Rust `run_streaming_turn` span points and attribute names so the observability studio
groups Python + Rust + TS turns together. A tracer provider is installed at server
boot, env-gated on `OTEL_EXPORTER_OTLP_ENDPOINT` (unset ⇒ zero external deps; the OTLP
exporter is the optional `otel` extra).
