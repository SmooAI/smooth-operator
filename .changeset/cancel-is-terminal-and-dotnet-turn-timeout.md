---
"@smooai/smooth-operator": patch
---

fix: make `cancelled` terminal on the servers, and bound a .NET turn on the client

Two terminal states that were not terminal.

**`cancelled` was advisory, not terminal (th-8628bf).** Cancellation is cooperative
everywhere: `JoinHandle::abort()`, `context` cancellation and `CancellationToken` all
take effect at the next yield point, not immediately. A turn that is executing rather
than suspended keeps running — and an `await` on an already-completed future does not
yield either — so work kept happening after the client had been sent the terminal
`cancelled` (499).

- **Python** was the worst: the Rich Interaction raise tool and the SEP extension host's
  `ui/confirm` park both caught `asyncio.CancelledError` alongside their own timeout and
  returned normally. asyncio only treats a task as cancelled if the `CancelledError`
  propagates out of it, so that *un-cancelled* the turn: it resumed, ran the next model
  call, persisted an assistant reply and emitted an `eventual_response` for a requestId
  the client had been told ended at 499. Both sites now re-raise; their own
  `TimeoutError` — a genuinely different thing — still degrades to "no answer".
- **Rust** now drops post-cancel frames at the connection writer, the one point every
  frame passes through. Per-emit-site checks could not do this: frames leave a turn from
  the runner's stream loop, from inside a raise tool, from the write-confirmation gate
  and from the turn tail. The spawned turn also carries a cancelled flag, raised before
  the abort and before `cancelled` goes out, that its tail re-reads immediately before
  each side effect — including the OTP dispatch, which is not a frame and so cannot be
  covered by the writer gate.
- **Go** gates the turn's sink once, for the same reason: a raise tool calls `sink()`
  straight from inside `Execute`, on the engine's goroutine, with no context check of its
  own. `offerOtp` also moved from the connection's context to the turn's, and re-checks
  immediately before dispatching — a host `OtpService` is under no obligation to honor
  the context, and this is a real code to a real person. The outbound persist re-checks
  too, because a store may ignore the context (the in-memory one takes it as `_`).

**A .NET turn had no timeout (th-10ff63).** `SmoothAgentClientOptions` exposed only
`RequestTimeout`, with no counterpart to TypeScript's `turnTimeout`, Go's
`DefaultTurnTimeout` or Python's `turn_timeout`. A turn the server accepted but never
terminated hung for the life of the process — no error, no diagnostic, and a leaked entry
in the client's turn table. `TurnTimeout` now defaults to the same 120s and faults the
turn with a `TurnTimeoutException`; `Timeout.InfiniteTimeSpan` disables it.

The same file's `_ = _transport.SendAsync(...)` sat inside a try/catch that could never
run: `SendAsync` is async, so a send failure faults the returned task instead of throwing
at the call site, and the discarded task was never observed. The turn was therefore never
aborted and leaked with no error. It is now awaited on a helper that aborts the turn.
