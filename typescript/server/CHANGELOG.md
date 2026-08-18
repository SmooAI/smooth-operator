# @smooai/smooth-operator-server

## 1.13.0

### Minor Changes

- c4911c8: Port the Rich Interactions runtime + the `choices` kind (AskUserQuestion) to the **Python** server (Wave 2 of the polyglot effort), mirroring the Rust reference (PR #475).

  New kind-agnostic framework (`interaction.py`): `InteractionKind` (kind / capability / tool*schema / parse_request / validate / fallback_directive), an `InteractionRegistry` catalog (default: `choices`), and a session-keyed `PendingInteractions` park/resume registry that generalizes the write-confirmation `ConfirmationRegistry`. Each turn registers per-kind `request*<kind>`raise tools — parking on a channel that declared the kind's render capability in`supports`(emit`interaction_required`, await `submit_interaction`, resume with the canonical payload), or degrading to the kind's conversational directive on text-only channels, where the model submits through the generic `submit_interaction`tool. A new`submit_interaction`dispatcher action routes values to the kind validator: invalid values emit retryable`interaction_invalid` (turn stays parked), valid values resume the turn.

  The `choices` kind (`choices.py`) mirrors `choices.rs`: `request_choices { questions (1–4), reason }` with 2–4 options and an optional `multiSelect`, the shared `validate_choices` (every question answered, labels ∈ options, single-select one pick XOR `other`, multi-select ≥1, blank `other` dropped, all errors in one pass), the enumerated fallback directive, and capability id `choice_chips`. Validated against the shared `spec/interactions/choices.schema.json` + conformance fixtures.

## 1.12.1

### Patch Changes

- 89c5ca7: fix(ts): cancel discards a HITL-parked confirmation so the parked turn drops cleanly

  Cancelling a turn parked at a write-confirmation (HITL) freed the slot and emitted `cancelled`
  correctly, but the park in `turnRunner.ts` awaits a bare deferred from `ConfirmationRegistry.register`
  (`const approved = await verdict`) that the turn's `cancelSignal` abort does NOT itself complete. The
  cancel path already discarded it via a connection-wide `confirmations.rejectAll()`, but that is a
  broader sweep than the cancel needs.

  `FrameDispatcher.cancelActiveTurn` now discards precisely the cancelled turn's pending confirmation
  (`confirmations.resolve(turn.sessionId, false)`) after aborting the controller, so the parked await
  unblocks immediately (resolves denied; the result is dropped because the sink is gagged and the slot
  is already cleared). `activeTurn` now carries a `sessionId`, stamped where the turn is created in the
  `send_message` handler. The disconnect path still rejects every outstanding confirmation separately
  via `rejectPendingConfirmations`, so nothing dangles there. Mirrors the Rust reference dropping the
  confirmation future on abort and the .NET fix (#460). No behavior change for a non-parked cancel or
  the no-active-turn no-op.

  Adds a parity test (mirroring the .NET `CancelUnparkTests`) that drives a turn to
  `write_confirmation_required`, cancels it, and asserts `cancelled` is emitted, a later
  `confirm_tool_action` returns `NO_PENDING_CONFIRMATION`, the slot is freed (a new `send_message` is
  accepted), and no stray events leak from the abandoned turn.

## 1.12.0

### Minor Changes

- bbefc4c: feat(server): env-gated durable-executor selection seam on the turn path (th-137b91, Q parity)

  The TS server ran every turn by calling `SmoothAgent.runStream` directly, with no
  place for a durable backend to plug in — unlike the Rust server, whose
  `turn_executor` selects the executor in one spot. This adds the sibling seam:
  `turnExecutor(injected?)` returns an injected `AgentExecutor` verbatim, else the
  engine's zero-infra `InProcessExecutor` (a verbatim delegation to `runStream`, so
  behavior is unchanged when nothing opts in). Setting `SMOOTH_AGENT_DURABLE_EXECUTOR`
  without supplying an executor warns and falls back rather than silently pretending
  the turn is durable.

  `TurnRunner` now takes an optional `executor` and runs the turn through
  `executor.executeStreaming(agent, …)` instead of `agent.runStream(…)` — the one
  place ADR-030's durable backend (e.g. `@smooai/smooth-operator-temporal`'s
  `TemporalAgentExecutor`) plugs in. The backend is injected as a **parameter**, so
  this server keeps **no dependency** on the Temporal package.

## 1.11.0

### Minor Changes

- c3e77d4: feat(ts): durable Postgres + pgvector vector-knowledge store (polyglot parity item L)

  Adds `PostgresKnowledgeStore` to the TypeScript server — the TS sibling of the Rust
  `PgKnowledgeBase` (`rust/adapters/postgres/src/knowledge.rs`) and the C#
  `PostgresKnowledgeBase` / `PostgresAclKnowledgeStore`. Documents are embedded (via an
  injected `Embedder`; a network-free `DeterministicEmbedder` ships for tests/offline) and
  stored as rows in the SAME shared `knowledge_vectors` table the Rust adapter creates, so a
  row written here reads back in every other server. Retrieval ranks by pgvector cosine
  distance, and document-level access control lives in the `acl` JSONB column and is filtered
  IN SQL (a restricted document is never fetched) — `forAccess(access)` for the ACL-scoped
  chat path, `withAcl(acl)` for ACL-stamped ingest.

  Contract tests mirror the C# `KnowledgeBaseContractTests` / `AclKnowledgeContractTests`
  against a real pgvector container (testcontainers, skip-if-no-Docker): ingest→retrieve,
  idempotent-by-id, durability across connections, and the anonymous/entitled/unentitled ACL
  leak boundary.

## 1.10.0

### Minor Changes

- caa3678: TS server: resolve `send_message.skill` server-side (Rust PR #338 parity).

  The TS server carried the `skill` field on the wire and ignored it — its own 1.39.0 changelog said so outright ("the TS / Python / Go / .NET servers ignore the field for now"). It now resolves the skill and composes it into the turn:

  - **`skills.ts`** — `isValidSkillName` (ASCII alphanumerics + `-`/`_`, ≤128 chars, making `..`, `/`, `\` and NUL _unrepresentable_ rather than filtered), `stripFrontmatter` (drops the discovery-metadata YAML so the model sees only instructions; unterminated frontmatter is returned untouched rather than swallowing the file), `skillSection`, and `resolveSection`.
  - **`SkillResolver`** — the host seam, via `serve({ skillResolver })`.
  - **`DirSkillResolver`** — `<root>/<name>/SKILL.md` over the `:`-separated roots in `SMOOTH_SKILLS_DIR`, first root wins. `serve()` prefers an explicit resolver, else `DirSkillResolver.fromEnv()`, mirroring Rust's `install_skill_resolver_from_env`. Neither ⇒ no resolver, so a multi-tenant deploy never serves host skills by accident.
  - **Fail-CLOSED**, unlike `images`: an unresolvable skill returns `error { code: "SKILL_NOT_FOUND" }` and the turn does not run — resolved _before_ the 202 ack, so a client never sees "accepted" for a turn that was never going to happen. A blank/whitespace `skill` is treated as absent, matching Rust's trim-then-filter.
  - The body is appended to the **system prompt**, last, so it is the most salient instruction into the turn while the persisted user message stays exactly what the user typed — skill prose never accumulates in history to be replayed on every later turn.

  Tests: all five Rust `skills.rs` unit tests ported under their Rust names, plus over-the-socket coverage for fail-closed (asserting the model is never called), system-prompt-not-user-message placement (including that frontmatter never reaches the model), and blank-as-absent with a resolver installed. 254 server tests green.

  Backward compatible: an absent `skill` field is byte-for-byte the previous behavior.

## 1.9.0

### Minor Changes

- aeb275a: .NET server: resolve `send_message.skill` server-side (Rust PR #338 parity).

  The C# server had the generated `SendMessageRequest.Skill` field but ignored it — the same staging `images` went through. It now resolves the skill and composes it into the turn, closing the last text-path gap with the Rust reference:

  - **`Skills`** — `IsValidSkillName` (ASCII alphanumerics + `-`/`_`, ≤128 chars, so `..`, `/`, `\` and NUL are _unrepresentable_ rather than filtered), `StripFrontmatter` (drops the discovery-metadata YAML block, leaving only the instructions the model should see; unterminated frontmatter is returned untouched rather than swallowing the file), `SkillSection` (the `## Skill: <name>` framing), and `ResolveSectionAsync`.
  - **`ISkillResolver`** — the host seam, injected via the `FrameDispatcher` constructor (the C# analog of Rust's `AppState::with_skill_resolver`).
  - **`DirSkillResolver`** — the working default: `<root>/<name>/SKILL.md` over the `:`-separated roots in `SMOOTH_SKILLS_DIR`, first root wins. The ASP.NET host prefers a DI-registered `ISkillResolver` and otherwise falls back to `DirSkillResolver.FromEnv()`, mirroring Rust's `install_skill_resolver_from_env`. Unset ⇒ no resolver installed, so a multi-tenant deploy never serves host skills by accident.
  - **Fail-CLOSED**, unlike `images`: an unresolvable skill returns `error { code: "SKILL_NOT_FOUND" }` and the turn does not run. A caller that asked for a code-review recipe and silently got a freeform answer has no way to tell. A blank/whitespace `skill` is treated as absent, matching Rust's trim-then-filter.
  - The resolved body goes to the **system prompt**, appended last so it is the most salient instruction into the turn — the persisted user message stays exactly what the user typed, so skill prose never accumulates in conversation history and gets replayed every later turn.

  Tests: `SkillTests` ports all five Rust `skills.rs` unit tests under their Rust names, plus dispatcher-level coverage for fail-closed `SKILL_NOT_FOUND`, system-prompt-not-user-message placement, blank-skill-as-absent, and unchanged behavior when the field is absent. `RecordingChatClient` was promoted out of `FileTransferTests` into a shared `TestChatClients.cs` so both suites use one double.

  Backward compatible: an absent `skill` field is byte-for-byte the previous behavior. Source-only — the engine stays the published NuGet.

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
