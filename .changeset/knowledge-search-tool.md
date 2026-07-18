---
'@smooai/smooth-operator': minor
---

Add a built-in ACL-scoped `knowledge_search` tool to the .NET server. Registering an `IAccessKnowledge` already grounds turns via RAG auto-context; this exposes the same store as a model-callable tool a host enables by name (`knowledge_search`) — no hand-wrapped `AIFunction` required. It's built per-turn over the connection's `IAccessKnowledge.ForAccess(access)` handle, so every search is document-level access-controlled (a doc outside the caller's ACL is never a candidate), and matches the Rust server's tool for parity: same name, args (`query` required + `limit` clamped 1..10, default 3), and text result shape.
