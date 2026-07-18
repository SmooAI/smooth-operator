---
'@smooai/smooth-operator': patch
---

dotnet server: TurnRunner degrades gracefully when knowledge retrieval fails. When the embedding gateway / vector store is down, `QueryAsync` used to propagate out of the turn and the dispatcher surfaced `INTERNAL_ERROR`, killing the whole turn. Now the retrieval failure is caught: the turn proceeds with empty grounding (no citations, and the failing store isn't handed to the engine's own RAG query), and a warning is logged. Only the retrieval is wrapped — the rest of the turn is unchanged.
