---
"@smooai/smooth-operator": minor
---

Add the SEP (Smooth Extension Protocol) spec — Phase 0.

New `spec/extension/` tree: `envelope.md` (JSON-RPC 2.0 over ndjson framing, method
catalog, error codes, context tiers, deferred WS binding), `methods/*.schema.json` (draft
2020-12, snake_case: initialize, shutdown, ping, event, hook, tool/execute, tool/update,
$/cancel, command/execute, registry/update, tools/set_active, session/*, exec/run,
ui/request, kv/*, bus/publish, log, plus the JSON-RPC frame envelope), and
`conformance/fixtures.json` (43 valid + 6 invalid instances) with the dependency-free
`echo.mjs` demo extension. A new `extension-conformance.test.ts` validates every fixture
against its schema, mirroring the existing operator-protocol conformance harness. SEP is a
sibling of the operator WebSocket protocol — it reuses the spec machinery, not the
envelope.
