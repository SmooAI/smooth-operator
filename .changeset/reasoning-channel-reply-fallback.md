---
'@smooai/smooth-operator': patch
---

Fix `eventual_response` still shipping an empty reply (blank `responseParts` + empty `suggestedNextActions`) on gpt-oss-120b via the LiteLLM/groq gateway, which 1.22.1 did not cover.

Confirmed empirically against the real SSE parser: this gateway/model emits the WHOLE answer on the reasoning channel (`delta.reasoning_content`) with `delta.content` never populated. The engine accumulates reasoning into a separate buffer and drops it from `response.content`, so BOTH `last_assistant_content()` and the 1.22.1 `streamed_reply` (content tokens) come back empty — even though the answer streams to the client as `stream_reasoning` and persists. The "streamed tokens" observed in prod were `stream_reasoning` frames (protocol-identical to `stream_token`), not content.

`rust/smooth-operator-server/src/runner.rs`: accumulate the turn's reasoning stream and use it as a LAST-RESORT fallback for the final reply — after `last_assistant_content()` and `streamed_reply`, only when no answer content exists anywhere. A normal reasoning model always populates `content`, so it never surfaces its thinking as the answer; this rung fires solely for the degenerate answer-in-reasoning case where the alternative is an empty response. The suggested-replies trailer is preserved through the fallback so suggestions are recovered.

Adds `tests/gateway_wire_empty_reply.rs`, a regression that drives the real `LlmClient` against a local mock speaking the gateway SSE wire format (answer-in-content and answer-in-reasoning shapes) — it fails if the reply goes empty again.
