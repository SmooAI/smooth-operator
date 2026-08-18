---
'@smooai/smooth-operator': patch
---

Make Go, Python, TypeScript and .NET tool spans self-identifying, and record cost on all four.

All four engines omitted `gen_ai.system` from the tool span. OTLP consumers that gate on that attribute — SmooAI's does — therefore discarded every tool span those engines ever emitted, exactly as the Rust engine did. Each now carries its own `gen_ai.system`, `gen_ai.operation.name` (literally `chat` / `tool`, taken verbatim by ingest), `gen_ai.conversation.id`, and `smooai.org_id` where the engine has one. Child spans need their own copies: an ingest merges resource attributes with *that span's* attributes and does not inherit from the parent.

Cost reaches the span for the first time in all four: `gen_ai.usage.cost_usd` when positive, otherwise `smooai.gen_ai.cost_unavailable` = `"unpriced"`. A gateway zero means the model is unpriced, not free, so it is never recorded as a cost.

Two fabricated values found and removed rather than ported:

- .NET returned a literal `TurnUsage(0, …)` on the cost fallback path, under an XML doc note reading *"0 means 'nothing priced it', not 'free'"* — the comment was correct and the code shipped the zero anyway.
- .NET published `input_tokens = 0` because it guarded on `sawUsage` alone; a usage chunk with null counts sets that flag while both totals collapse to `0`. The other three engines guard on the counts themselves.

Each engine has a test that fails without its change, verified by reverting and restoring.
