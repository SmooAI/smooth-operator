---
'@smooai/smooth-operator': minor
---

Go server: emit `stream_preamble`. When `SMOOTH_AGENT_PREAMBLE_MODEL` is set, a small fast model runs in parallel with the turn and streams one ephemeral "what I'm about to do" sentence, covering the reasoning model's time-to-first-token — matching the Rust reference server's prompt, 64-token cap, and first-answer-token race guard. Unset/empty/whitespace leaves behavior and the model-call count unchanged. The preamble is best-effort (failures swallowed) and ephemeral (never persisted, never folded into `eventual_response`).
