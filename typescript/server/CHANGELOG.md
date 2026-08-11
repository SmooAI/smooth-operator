# @smooai/smooth-operator-server

## 1.8.0

### Minor Changes

- ee2bdaa: `send_message` grows an optional `files[]` array — non-image attachments the host persists, never sent to the model.

  Each entry is `{ name, mimeType?, url }`. Parsing mirrors `images`: fail-soft (an absent key ⇒ empty, a malformed array is dropped rather than rejecting the turn). Unlike `images`, files do NOT reach the model — they ride the turn's `ToolProviderContext` only, so a host tool (paired with the `send_file` directive convention on `eventual_response.directive`) can persist and echo them back.

  - New `smooth_operator::tool_provider::UserFile { name, mime, url }` (`mimeType` ↔ `mime` on the wire), alongside `UserImage`.
  - `ToolProviderContext.files` + `with_files(..)` builder; threaded from the `send_message` frame through `TurnRequest` into the per-turn tool-provider context.

  Backward compatible: an absent `files` field is byte-for-byte the previous behavior. Rust is the reference implementation.

## 1.7.0

### Minor Changes

- 2d5dc0e: .NET server: implement the file-transfer contract (PR #342) — `send_message.images[]` / `files[]` and the `send_file` directive.

  The C# server was text-only on `send_message`; it now reaches Rust parity:

  - **`images[]`** — parsed fail-soft and attached to the turn as OpenAI `image_url` content parts. Because the engine builds the live user turn from a string (no content parts), the images ride on an image-only `ChatMessage` seated on the thread immediately before the text turn — so the model sees them adjacent to the question, with no empty or duplicated message. A `data:` URL becomes a `DataContent`, an `http(s)` URL a `UriContent`; the optional vision `detail` hint rides on `AdditionalProperties`. Malformed/unsupported entries are dropped, never rejecting the turn.
  - **`files[]`** — parsed fail-soft and surfaced on a new per-turn `TurnContext` (files list + directive sink), published as an `AsyncLocal` around the run so a host tool the engine invokes can read them. Never sent to the model.
  - **`send_file` directive** — a host tool writes an opaque directive onto `TurnContext.Current.Directive`; the runner drains it after the turn onto `eventual_response.directive` (last-write-wins, omitted when none for back-compat).
  - Generated `Types.cs` regenerated from the updated spec (adds `SendMessageRequest.Files`, `Skill`).

  Backward compatible: absent `images`/`files` and no host directive is byte-for-byte the previous text-only behavior. Source-only — the engine stays the published NuGet.

## 1.6.0

### Minor Changes

- d41643e: File transfer (TS server): implement the PR #342 contract to Rust parity. `send_message` now parses `images[]` and `files[]`: images are attached to the turn's user message as OpenAI `image_url` content parts (via a `withUserImages` request-body wrapper, since the published core takes a plain-string message and reuses it for retrieval), while files are surfaced — never sent to the model — on a new per-turn `ToolContext`. A new optional `toolProvider` seam (mirroring the Rust `ToolProvider`) hands host tools that context, including a directive sink; a host tool that writes `ctx.directive` (e.g. a `send_file` directive) has it drained onto `eventual_response.directive` (last-write-wins). All attachment parsing is fail-soft. The client SDK's generated protocol types gain the `images`/`files` request fields and the `directive` response field.

## 1.5.0

### Minor Changes

- 5e7b891: Protocol: bidirectional file transfer. Add `send_message.files[]` (non-image attachments the host lands in the agent workspace, distinct from vision `images[]`) and document the `send_file` host directive convention on `eventual_response.directive` (agent → user file delivery). Spec-only in this change; per-language server behavior (parse `files`, wire the directive sink so a host `send_file` tool can emit) follows.
- 20e8c1f: Python server: file-transfer parity with the Rust reference. `send_message.images[]` now attach to the model's user message as OpenAI `image_url` content parts (multimodal turns), `send_message.files[]` are surfaced on the per-turn tool-provider context (a host tool lands them in the workspace; never sent to the model), and a host tool can write a client-side directive (e.g. the `send_file` convention) onto the turn's directive sink — drained after the turn onto `eventual_response.directive`. All fail-soft and back-compatible: a text-only turn with no attachments is byte-identical to before. Regenerated the Python SDK types from the updated spec (`files[]` + directive docs).

## 1.4.1

### Patch Changes

- 4b2b5d7: Conversation-workflow adherence (th-d57a1d): the rendered `<ConversationWorkflow>` step section now instructs the agent to ask the current step's question directly and never re-ask for permission / re-confirm readiness / repeat an answered question (gpt-oss-class models over-indexed on the old "you don't have to force the step to close" line and looped on re-confirmation). The workflow judge now counts brief/terse answers that address the step ("a four", "sure") as satisfying it instead of holding out for elaboration. Same wording change applied across all five language servers (TS, Rust, Python, Go, .NET).

## 1.4.0

### Minor Changes

- 644a123: TS server: `list_conversations` + resume-by-`conversationId`

  Mirror the merged Rust reference (pearl th-d5b446) on the TypeScript
  smooth-operator-server — the conversation-sidebar / resume substrate every client
  builds against.

  - New WS action `list_conversations`: most-recent-first, only conversations with
    `messageCount > 0` (drops empty-on-page-load spam), each with a first-inbound
    title preview (~60 chars, leading markdown/control chars stripped, name
    fallback), ISO-8601 `updatedAt`, and `messageCount`. Optional `limit` (default 50).
  - `create_conversation_session` gains optional `conversationId`: when it names an
    existing conversation, resume — reuse its id + persisted message log, so
    `send_message` appends and the runner replays history. Absent/unknown id keeps
    minting fresh (unchanged).

  Additive + back-compat: no `conversationId` / no `list_conversations` call =
  unchanged behavior. New tests cover list filter/preview/order/limit, resume
  binding + history replay, and unknown-id fallback.

## 1.3.0

### Minor Changes

- a15b3b9: TS server: honor per-agent config + implement conversation workflows (SMOODEV-590).

  Agents served by the TypeScript operator now respect their own per-agent config instead of all sharing one generic org persona. A new `AgentConfigResolver` seam resolves a session's `agentId` into its `AgentConfig` (instructions, `conversationWorkflow`, greeting, personality, tool allow-list); the resolver is server-side because the `create_conversation_session` payload carries only an `agentId`. An un-configured agent (no resolver, or resolver returns undefined) falls back to the server/org default prompt + full tool set, so existing behavior is unchanged.

  `conversationWorkflow` is implemented as a stepped, judge-advanced guided-agency flow: the current step's intent + criteria are rendered into the system prompt, and after each turn a cheap failure-tolerant judge LLM call decides whether the criteria were met and advances the pointer (following `next` or array order), tracked as `currentStepId` on the session. Malformed config degrades to the default flow and never crashes a session. Mirrors the Rust server's `agent-config-instructions-workflow` design.

### Patch Changes

- d7b6377: TypeScript server: wire the OpenAI client's streaming surface. The server always drives `runStream`, which needs `chat.completions.createStream`, but the raw `openai` SDK only exposes `create` — so every live turn threw "requires a streaming-capable client" and clients saw a bare `INTERNAL_ERROR`. `buildChatClient` now adapts `create({ ...body, stream: true })` into the engine's `createStream` async-iterable, and the two swallowed turn-failure `catch`es now log the underlying error to stderr instead of hiding it. Validated end-to-end: smooth-web drives the TS server to a real streamed reply.
