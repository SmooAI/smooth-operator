---
'@smooai/smooth-operator': patch
---

docs: refresh the .NET server docs to match the shipped 1.23.x surface. `dotnet/server/README.md`'s "What's shipped/Next" list and `docs/Architecture/Polyglot Cores.md`'s service-layer intro both lagged the published dll — knowledge grounding, ACL-filtered retrieval, citations, the reranker, GitHub ingestion + connectors, HITL write-confirmation, the `/admin/*` API, and the deployable host all ship in C# now. Corrected the stale "not yet built in C#" framing and marked the genuinely-open items (Notion/Slack connectors in-flight, checkpoint-adapter resume wiring).
