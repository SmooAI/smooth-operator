---
'@smooai/smooth-operator': patch
---

Fix multi-tenant org scoping in `handle_send_message`, and make LLM turn spans carry cost and tool spans self-identifying.

**Org scoping (#470).** `handle_send_message` bound `org_id` twice: once correctly from the conversation (used to resolve the per-org gateway key) and again ~120 lines later as `SEED_ORG_ID`, shadowing it. Everything downstream of the second binding — the org persona override and the host tool provider's scope — saw `reference-org` on a multi-tenant host, for every tenant. Because the gateway key came from the *first* binding, per-org billing and per-org LLM keys stayed correct, which is why the split went unnoticed. Now reuses the derived org and falls back to the seed org only when it is empty, so the single-org reference/local flavor is unchanged.

**Turn cost (#471).** The gateway's authoritative per-response cost was already parsed by `smooai-smooth-operator-core` and carried to `TurnUsage.cost_usd`, but nothing recorded it on the span, so `gen_ai.usage.cost_usd` was never emitted and consumers showed "cost not measured". The turn span now records it. A non-positive or non-finite value is dropped rather than written as `0` — a gateway `0` means the model is *unpriced*, not free, and recording it as a real cost would silently under-bill.

**Self-identifying tool spans (#471).** Tool spans carried neither `gen_ai.system` nor `gen_ai.operation.name`, and OTLP consumers that gate on `gen_ai.system` therefore discarded every tool span ever emitted. They now carry `gen_ai.system`, `gen_ai.operation.name`, `gen_ai.conversation.id` and `smooai.org_id`, so a tool span is both ingestable and joinable to its conversation. Fixed at both emission sites — `runner::run_streaming_turn` and `KnowledgeChatRuntime::run_turn` — which had the identical gap.
