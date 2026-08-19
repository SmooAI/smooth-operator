---
'@smooai/smooth-operator': patch
---

go-server: fix the P1 connection deadlock (and permanent goroutine/socket leak) when a client disconnects mid-turn.

The per-connection outbound sink is a **bounded** channel (64), and the writer goroutine `return`ed on its first failed `conn.Write`. With no reader left, a streaming turn's 65th `send` blocked forever — and `send` holds `sendMu` across that send, whose only escape (`ioCtx`) is cancelled by `teardown`, which is itself waiting on `WaitForTurns()`. Circular wait: the turn never reached its `ctx.Err()` check even though `CancelTurn()` had already fired, so the connection goroutine and its `s.conns` WaitGroup entry leaked for the life of the process and `Shutdown()` never returned. The read loop and backplane wedged with it on `sendMu`.

The writer now keeps **draining** (discarding) the sink once the socket is dead instead of returning — matching the Rust reference, whose unbounded `sink_tx` can never block a turn on a dead socket.

Also from the same review:

- **Panic containment.** There was no `recover()` anywhere in `go/`, and a turn runs on a bare goroutine — so one panicking host store/config/hook killed the whole process and dropped every other live connection. A panicking turn now settles as a clean `INTERNAL_ERROR` with the connection still usable. (A panic inside a _tool_ is still fatal: the engine runs the tool loop on its own recover-less goroutine in `smooth-operator-core`, so that guard belongs there.) The optional preamble goroutine is guarded too.
- **Nil-deref crash path.** `TurnRunner.Run` called `stream.Events()` with no nil check, so an `AgentExecutor` returning `(nil, nil)` panicked the turn; it now fails the turn cleanly.

Regression tests: `TestClientDisconnectMidStreamDoesNotWedgeTurn` (asserts `Shutdown()` actually returns after an RST mid-burst — it hangs without the fix; note `-race` cannot catch a wedged goroutine) and `TestPanickingTurnDoesNotKillTheProcess`.
