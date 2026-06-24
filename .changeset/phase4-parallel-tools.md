---
'@smooai/smooth-operator': minor
---

Phase 4: concurrent (parallel) tool-call execution across the Python, TypeScript, Go,
and C# cores. A new opt-in `parallelToolCalls` option (Python `parallel_tool_calls`,
Go/C# `ParallelToolCalls`), default false, dispatches an assistant turn's tool calls
concurrently (`asyncio.gather` / `Promise.all` / goroutines + `sync.WaitGroup` /
`Task.WhenAll`) when there are two or more. The tool-result messages are still appended
in the original tool-call order, so the transcript stays deterministic regardless of
completion order; a failing or human-denied tool keeps its error result in its correct
position. With the flag off (the default) — or for single-tool-call turns — dispatch is
unchanged from today's sequential behavior. Per-tool semantics (clearance, human-gate
approval, tool_search promotion, JSON-arg parsing) are untouched.
