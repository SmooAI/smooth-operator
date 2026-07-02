---
"@smooai/smooth-extension-sdk": minor
"@smooai/smooth-operator": patch
---

Add the SEP TypeScript extension SDK — Phase 1 (the tool path).

New published package `@smooai/smooth-extension-sdk`: build Smooth Extension Protocol
extensions in TypeScript. `defineExtension`/`defineTool` (zod v4 via `z.toJSONSchema`, with
raw JSON-Schema / TypeBox pass-through), a symmetric JSON-RPC 2.0 `Peer`, an ndjson stdio
transport (plus an in-memory `linkedPair`), `createTestHost` for driving an extension
in-process, and `runConformance` to replay the shared fixtures against a real extension
subprocess. Ships the `hello` demo extension (`hello.greet` — zod schema, streamed
`tool/update` progress, `$/cancel` cancellation). Wired into the TypeScript CI lane.

Extends `spec/extension/conformance/fixtures.json` for the tool path: `is_error` and
`details` tool results, a message-only `tool/update`, and invalid fixtures (missing
`content`, out-of-range `progress`).
