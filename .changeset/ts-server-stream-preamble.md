---
'@smooai/smooth-operator': minor
---

TypeScript server: emit `stream_preamble` (pearl th-8e0a52).

The TS server now honours `SMOOTH_AGENT_PREAMBLE_MODEL`, matching the Rust reference. When set, a small fast model runs in parallel with each turn on the same gateway/key (model id + a 64-token cap are the only overrides) and emits ONE ephemeral "what I'm about to do" sentence to cover the reasoning model's time-to-first-token.

Off by default: unset, empty, or whitespace means no extra LLM call, no extra event, behaviour byte-for-byte unchanged. The preamble is suppressed once the real answer starts streaming, is never persisted or folded into `eventual_response`, and any failure is swallowed at debug so it can never fail or delay a turn.
