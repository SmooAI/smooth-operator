# @smooai/smooth-operator-server

## 1.3.0

### Minor Changes

- a15b3b9: TS server: honor per-agent config + implement conversation workflows (SMOODEV-590).

  Agents served by the TypeScript operator now respect their own per-agent config instead of all sharing one generic org persona. A new `AgentConfigResolver` seam resolves a session's `agentId` into its `AgentConfig` (instructions, `conversationWorkflow`, greeting, personality, tool allow-list); the resolver is server-side because the `create_conversation_session` payload carries only an `agentId`. An un-configured agent (no resolver, or resolver returns undefined) falls back to the server/org default prompt + full tool set, so existing behavior is unchanged.

  `conversationWorkflow` is implemented as a stepped, judge-advanced guided-agency flow: the current step's intent + criteria are rendered into the system prompt, and after each turn a cheap failure-tolerant judge LLM call decides whether the criteria were met and advances the pointer (following `next` or array order), tracked as `currentStepId` on the session. Malformed config degrades to the default flow and never crashes a session. Mirrors the Rust server's `agent-config-instructions-workflow` design.

### Patch Changes

- d7b6377: TypeScript server: wire the OpenAI client's streaming surface. The server always drives `runStream`, which needs `chat.completions.createStream`, but the raw `openai` SDK only exposes `create` — so every live turn threw "requires a streaming-capable client" and clients saw a bare `INTERNAL_ERROR`. `buildChatClient` now adapts `create({ ...body, stream: true })` into the engine's `createStream` async-iterable, and the two swallowed turn-failure `catch`es now log the underlying error to stderr instead of hiding it. Validated end-to-end: smooth-web drives the TS server to a real streamed reply.
