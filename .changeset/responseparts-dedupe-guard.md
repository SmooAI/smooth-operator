---
'@smooai/smooth-operator': patch
---

Server: deterministic backstop against a degenerate LLM repetition loop spamming
the chat widget. `general_agent_response` now collapses runaway near-identical
filler in the finalized reply — splits on paragraph breaks, drops paragraphs
near-identical to one already kept, and caps the count — before it reaches the
widget. A healthy reply is returned byte-for-byte unchanged.
