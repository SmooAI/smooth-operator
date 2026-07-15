---
"@smooai/smooth-operator": patch
---

Add an optional fast-model **preamble** to streaming turns to cover the reasoning model's time-to-first-token. When the server is configured with `SMOOTH_AGENT_PREAMBLE_MODEL` (e.g. `groq-gpt-oss-20b`), a small fast model runs IN PARALLEL with the main turn and streams ONE short present-tense "what I'm about to do" sentence over a new `stream_preamble` wire event — an ephemeral status line the real answer replaces. It's best-effort (any error/slowness is swallowed on its own task) and guarded: it's dropped if the real answer has already begun streaming, so it can never block or corrupt a turn. Unset ⇒ no extra call and byte-for-byte unchanged behavior. Adds `stream-preamble.schema.json` to the SEP spec and `StreamPreamble` to the TypeScript SDK union.
