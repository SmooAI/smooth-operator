---
'@smooai/smooth-operator': minor
---

Phase 4: streaming turn execution across the Python, TypeScript, and Go cores (C#
already streams via MEAI's `RunStreamingAsync`). A new streaming run method alongside
the existing `run()` — TS `runStream` (`AsyncGenerator<StreamEvent>`), Python
`run_stream` (`AsyncIterator[StreamEvent]`), Go `RunStream` (returns a `*Stream` whose
`Events()` channel carries `StreamEvent`s and whose `Err()` reports a mid-turn model
error) — drives the SAME agentic loop (system/knowledge/memory build, compaction, cost
tracking, budget early-stop, deferred tools, clearance + human-gate, checkpoint/thread
persistence) but calls the model in STREAMING mode and yields incremental events: a
`text` event per content delta, a `tool_call` event per requested call (before
dispatch), a `tool_result` event per finished tool (in original call order even under
`parallelToolCalls`), and exactly one terminal `done` event carrying the same
`AgentRunResponse` `run()` would return. The provider seam gains an OpenAI-style
streaming call (`createStream` / `create(..., stream=True)` / `ChatStream`) that
accumulates content + `tool_calls` deltas by index into a full assistant message, so
the rest of the loop is unchanged; usage is read from the final chunk for cost/budget.
The reusable mock LLM providers replay their FIFO script as chunked deltas (text split
into pieces, tool-call arguments split across two chunks). Retry-with-backoff is
intentionally not applied to streaming (re-running would re-emit chunks), mirroring C#.
