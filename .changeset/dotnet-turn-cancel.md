---
'@smooai/smooth-operator': patch
---

.NET server: implement turn cancellation (the "Stop button") — the `cancel` action, ported from the Rust reference. `FrameDispatcher` now tracks the connection's single in-flight `send_message` turn with a per-turn `CancellationTokenSource`: a `{"action":"cancel","requestId":"<turn>"}` frame cancels it (dropping the turn at its next await, abandoning the in-flight LLM/tool call) and emits the terminal `cancelled` event (`status: 499`, echoing the turn's `requestId`) in place of `eventual_response`. The partial assistant message is discarded — the user's message, persisted before the agent loop, stays. A cancel with no active turn is a silent no-op; a second `send_message` while a turn is in flight is rejected with `TURN_IN_PROGRESS` rather than run concurrently. A client disconnect aborts the in-flight turn as well, while graceful shutdown still drains it. No engine change.
