---
"@smooai/smooth-operator-server": patch
---

TypeScript server: wire the OpenAI client's streaming surface. The server always drives `runStream`, which needs `chat.completions.createStream`, but the raw `openai` SDK only exposes `create` — so every live turn threw "requires a streaming-capable client" and clients saw a bare `INTERNAL_ERROR`. `buildChatClient` now adapts `create({ ...body, stream: true })` into the engine's `createStream` async-iterable, and the two swallowed turn-failure `catch`es now log the underlying error to stderr instead of hiding it. Validated end-to-end: smooth-web drives the TS server to a real streamed reply.
