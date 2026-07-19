---
'@smooai/smooth-operator': minor
---

Emit `stream_preamble` from the **.NET server**. It already had the generated protocol type but never produced the event and never read `SMOOTH_AGENT_PREAMBLE_MODEL`, so a host running on the C# server could not turn the feature on at all — this closes that gap and brings the .NET lane to parity with the Rust reference.

When `SMOOTH_AGENT_PREAMBLE_MODEL` is set (e.g. `groq-gpt-oss-20b`), `TurnRunner` fires a small fast model IN PARALLEL with the agent loop — same gateway and key as the turn, with only the model id and a 64-token output cap overridden — and emits ONE short present-tense "what I'm about to do" sentence as an ephemeral `stream_preamble` event, covering the reasoning model's time-to-first-token. The system prompt is byte-identical to the other servers'.

It is deliberately defined by what it must never do: the turn never awaits it (it can't delay or gate the answer), an atomic first-answer-token guard drops it the moment real answer tokens start streaming, any failure (timeout, gateway error, bad model id) is logged at debug and swallowed with no error event reaching the client, and the text is never persisted nor folded into `eventual_response`. Unset, empty, or whitespace ⇒ the feature is off, no extra LLM call is made, and behavior is byte-for-byte unchanged.
