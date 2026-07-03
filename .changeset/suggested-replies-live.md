---
'@smooai/smooth-operator': minor
---

Suggested quick replies: the Rust server's `eventual_response` now carries live `suggestedNextActions` instead of a hardcoded empty array. The runner appends a machine-parsed trailer contract (`<suggested_replies>["…"]</suggested_replies>`) to every turn's system prompt, suppresses the trailer from the live token stream, strips it from the persisted/final reply, and surfaces the parsed suggestions (capped at 4) on `TurnResult.suggested_next_actions` and the `eventual_response` payload. `runner::general_agent_response` now takes the suggestions slice. Rust server only; other language servers still emit an empty array (parity follow-up).
