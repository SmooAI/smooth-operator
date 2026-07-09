---
"@smooai/smooth-operator": patch
---

Fix auto-title producing empty titles. The auto-title model (`groq-gpt-oss-20b`) is a reasoning model whose reasoning tokens count against `max_tokens`, so the original 32-token cap was fully consumed by reasoning and left the completion content empty — the titler then silently kept the default `Session <uuid>` name. Raise the auto-title budget to 512 (the title itself is still capped to `TITLE_MAX` chars by `sanitize_title`), extract `title_request_body` so the budget is unit-tested, and add tracing at each auto-title bail point (debug for the expected "already named" skip, warn for real failures).
