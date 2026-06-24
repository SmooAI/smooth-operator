---
"@smooai/smooth-operator": minor
---

Thread the turn's `conversation_id` and resolved per-org `gateway_key` into `ToolProviderContext`.

A host's injected `ToolProvider` now receives the conversation the turn runs in and the LLM-gateway key that turn was billed/scoped to — alongside the existing `org_id` + `access`. This lets SmooAI's conversation-persisting tools correlate to the right conversation (instead of degrading to a no-op on an empty conversation id) and lets agent-brain's `knowledge_search` obtain the gateway key.

Purely additive and behavior-preserving: both new fields are `Option`, default to `None` via `ToolProviderContext::new`, and existing `ToolProvider` impls that ignore them are unaffected. New builders `with_conversation_id` / `with_gateway_key` set them; the runner populates both from the turn it already has in hand.
