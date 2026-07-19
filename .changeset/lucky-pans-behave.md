---
'@smooai/smooth-operator': minor
---

Python server: emit `stream_preamble`. When `SMOOTH_AGENT_PREAMBLE_MODEL` is set, a small fast model runs concurrently with each streaming turn and emits one ephemeral "what I'm about to do" sentence, covering the reasoning model's time-to-first-token — matching the Rust reference server (same system prompt, same 64-token cap, same gateway/key with only the model id overridden).

The preamble never delays or gates the real turn, is dropped the instant the first real answer token is emitted, is never persisted or folded into `eventual_response`, and any failure is swallowed at debug. Unset, empty, or whitespace ⇒ off (the default): no extra LLM call, behavior byte-for-byte unchanged.
