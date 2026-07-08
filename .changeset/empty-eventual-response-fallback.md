---
'@smooai/smooth-operator': patch
---

Fix intermittently empty `eventual_response` on the streaming turn (blank `responseParts` + dropped `suggestedNextActions`) even though the full reply streamed and persisted.

The runner sourced the final reply from `Conversation::last_assistant_content()`. On reasoning models (e.g. `groq-gpt-oss-120b`) a turn can end on a tool-call or reasoning-only assistant entry whose `content` is empty, so that returned `""` — shipping an empty `eventual_response` and losing the parsed suggestions.

`rust/smooth-operator-server/src/runner.rs`: accumulate THIS turn's raw streamed answer tokens (pre-suppressor, reasoning excluded — identical to the engine's assistant `content`) and fall back to it when `last_assistant_content()` is empty. The suggested-replies trailer is preserved in the fallback so `extract_suggested_replies` strips it and recovers the suggestions exactly as on the normal path. The non-empty path is byte-for-byte unchanged.
