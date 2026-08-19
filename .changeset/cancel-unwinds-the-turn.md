---
'@smooai/smooth-operator': patch
---

go-server / dotnet-server: make cancelling a turn a **stop** button rather than a **mute** button.

After a client cancels, the Go and .NET servers walked away from the turn — the runner returns at its first cancellation check and the connection's sink is gagged — but the **agent loop kept running**. The engine folds every tool failure back to the model as a tool result and iterates, and after a cancel that failure is the `context canceled` a tool returns, or the denial the write-confirmation gate returns once `TryCancelActiveTurn` unparks it. So the loop went on to another model call and acted on the answer, with every trace of it discarded. Rust, TypeScript and Python unwind properly; this brings the two ports in line.

Neither engine's loop has a cancellation check of its own, and cancellation in Go/.NET is cooperative rather than the preemptive future-drop the Rust reference gets for free. The loop is therefore stopped at the one place it re-enters shared state — the model call: the turn's chat client is wrapped so a cancelled context fails the call instead of issuing it, which unwinds `RunStream` / `RunStreamingAsync` and ends the turn. In production the gateway client would have failed that call on its own cancelled context; the servers now stop the turn themselves instead of relying on the transport to do it.

This also clears the `DATA RACE` the Go race detector reports on the shared conformance corpus's `cancel-mid-turn` scenario, where the cancelled turn's goroutine and the next turn's goroutine drove the engine's mock provider concurrently — independent proof that the cancelled turn was still running.

Regression tests: `TestCancelledTurnMakesNoFurtherModelCall` (Go) and `ACancelledTurn_MakesNoFurtherModelCall_SoTheNextTurnKeepsItsResponse` (.NET). Both assert the model-call count at a settle point rather than on timing, and both fail without the fix.
