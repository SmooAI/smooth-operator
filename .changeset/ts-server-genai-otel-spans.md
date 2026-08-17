---
"@smooai/smooth-operator": minor
---

feat(ts-server): emit gen_ai OpenTelemetry spans on the turn/tool path (M parity)

The TypeScript server emitted no OpenTelemetry spans; the Rust server emits
`gen_ai.chat` / `gen_ai.tool` spans on the turn/tool path. This brings the TS
server to parity (polyglot item M, th-873430).

`TurnRunner.run` now opens a `gen_ai.chat` span per turn carrying the same
attributes as the Rust runner — `gen_ai.system` (`smooth-operator`),
`gen_ai.request.model`, `gen_ai.conversation.id`, `gen_ai.agent.name`
(`smooth-agent-chat`), and `smooai.org_id` (threaded from the session) — records
`gen_ai.usage.input_tokens` / `gen_ai.usage.output_tokens` from the terminal
`done` event, and emits a child `gen_ai.tool` span per tool call with the tool
name and its redacted `gen_ai.tool.call.arguments`.

New `typescript/server/src/telemetry.ts` holds the GenAI attribute-key constants,
a `redactToolArguments` scrub (secret-named JSON keys → `[REDACTED]`, length-
capped) mirroring the Rust `redact_tool_arguments`, and an `initTelemetry()`
that is env-gated on `OTEL_EXPORTER_OTLP_ENDPOINT` exactly like the Rust
`init_telemetry` — set ⇒ an OTLP (HTTP/protobuf) exporter is registered, unset ⇒
no-op spans, no collector needed. `main()` calls it at boot.

A new `test/telemetry.test.ts` drives a real streaming turn against an in-memory
span exporter and asserts the `gen_ai.chat` (with org) and child `gen_ai.tool`
(with args) spans — the TS parity of `smooth-operator-server/tests/telemetry.rs`.

Green: tsc + 320 server tests.
