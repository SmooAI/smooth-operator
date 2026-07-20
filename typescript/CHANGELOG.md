# @smooai/smooth-operator

## 1.34.0

### Minor Changes

- b79184f: **SECURITY (cross-user data leak) — Go server: scope conversations to the authenticated user.**

  `SessionStore.ListConversations` took no user filter, so `list_conversations` returned **every user's conversations** to any authenticated caller. The resume path and `get_conversation_messages` were not owner-checked either, so a caller could also open and read another user's conversation by id. Any authenticated user could enumerate and read anyone else's chats.

  Conversations are now owned by the **authenticated principal** and every read is filtered by it:

  - `Principal` carries `Email` (the JWT `email` claim); `AccessContext.ConversationScope()` derives the connection's visibility. Ownership comes from the connection's principal only — the client-supplied `userName` / `userEmail` frame fields were the spoofing vector and no longer influence who may read what (`userEmail` still serves as the OTP delivery contact).
  - `ListConversations` filters during selection, before the handler's limit — filtering after a limit silently returns short/empty pages.
  - `get_session`, `get_conversation_messages`, `send_message` and `verify_otp` all route session lookups through one owner-checked chokepoint.
  - **Not-yours is indistinguishable from never-existed.** A denied session read returns the identical `SESSION_NOT_FOUND` payload an unknown id returns, and resuming another user's conversation mints a fresh conversation exactly as an unknown id does — so neither path can be used as an oracle to enumerate other users' session or conversation ids.
  - Fails **closed**: auth enabled with a principal that has no email (including a rejected/expired token) sees nothing, rather than falling back to unscoped.
  - Auth **disabled** (no verifier configured — local/dev single-tenant) stays unscoped and is unchanged. That is the only unscoped path.

  **BREAKING for `SessionStore` implementers (deliberate).** `ListConversations`, `CreateSession` and `ResumeSession` now take a required `ConversationScope`. The parameter is required rather than optional-defaulting-to-unscoped precisely so that every downstream implementation gets a **compile error** and must confront who may see what; a default-to-unscoped parameter would be fail-open and would leave downstream stores silently vulnerable.

  Migration: thread the scope from `AccessContext.ConversationScope()` into your store, persist the owning email on conversation creation, and filter reads by `scope.Allows(ownerEmail)`. `ConversationScope`'s zero value denies everything, so a partially-migrated store leaks nothing.

- 011db17: **SECURITY** — fix a cross-user conversation data leak in the TypeScript server, and scope every conversation read to the connection's authenticated principal (th-8fe998).

  **The vulnerability.** `SessionStore.listConversations()` took no user filter, so the `list_conversations` action returned EVERY user's conversations to any caller. The resume path (`create_conversation_session` with a `conversationId`) and `get_conversation_messages` performed no owner check either, so any authenticated user could enumerate other users' conversation ids from the list and then open, read, and post into those conversations. `get_session`, `send_message`, and `verify_otp` were exposed through the same missing check.

  **The fix.** A session now records an owner — the authenticated principal's email, taken from the connection's `email` claim. Every conversation read is checked against it:

  - `list_conversations` is scoped to the principal, with the filter applied inside the store selection (ahead of any limit, so a scoped page is never silently short or empty).
  - `get_session`, `get_conversation_messages`, `send_message`, and `verify_otp` return `SESSION_NOT_FOUND` for a session the caller doesn't own — byte-identical to the response for an id that never existed, so the pair can't be used as an existence oracle to enumerate other users' session ids.
  - Resuming another user's conversation is treated exactly like resuming an unknown id: the id is dropped and a fresh conversation is minted. Erroring on a real-but-not-yours id while silently minting for an unknown one would itself confirm which ids exist.
  - The client-supplied `userName` / `userEmail` frame fields no longer determine identity. They were the spoofing vector: a caller could claim any email and receive that user's scope. The principal always wins; on an auth-enabled server the frame values are ignored for ownership (`userEmail` still serves as the OTP delivery contact).

  **Fail-closed rules.** Auth enabled and the principal has an email → scoped to it. Auth enabled and the principal is missing or emailless (including a missing, expired, or forged token) → empty list and denied reads, never a silent fall back to unscoped. Auth disabled (no verifier configured — local/dev single-tenant) → unscoped, unchanged; this is the only unscoped path.

  **BREAKING for custom `SessionStore` implementations** — deliberately, and the break is the point:

  - `listConversations()` gains a **required** `userEmail: string | undefined` parameter. It is required, not optional-defaulting-to-unscoped, because an optional parameter is fail-OPEN: existing implementations would keep compiling and keep leaking every user's conversations. The compile error forces each implementation to make an explicit scoping decision.
  - `getConversation()` now returns `{ conversationId, userEmail }`, with `userEmail` required so a store that doesn't track ownership fails to compile rather than silently reporting every conversation as ownerless.
  - `StoredSession` gains an optional `userEmail` (the owner). Implementations must persist it at create time and must NOT let a resume rewrite it, or a second caller could take ownership of a conversation by resuming it.

  Migration: filter conversations by the passed `userEmail` in the query itself (`WHERE user_email = ?`), never after applying a limit; return `undefined` for `userEmail` only when the row genuinely has no owner. `AccessContext` also gains a required `authEnabled` flag, set by the verifier, which distinguishes "auth is off" (unscoped) from "auth is on but this connection didn't authenticate" (fail closed) — custom `AuthVerifier` implementations must set it.

## 1.33.0

### Minor Changes

- b38cb4b: **SECURITY — Python server: per-user conversation scoping (th-8fe998).** Fixes a cross-user data leak: `list_conversations` took no user filter and returned **every** user's conversations, and neither the resume path nor the sessionId-bearing actions were owner-checked, so any authenticated user could enumerate and open anyone else's chats.

  Conversations are now owned by the **authenticated principal's** email (the JWT `email` claim, plumbed onto `Principal`) — never the client-supplied `userName` / `userEmail` frame fields, which were the spoofing vector. With auth enabled the principal's email also replaces `userEmail` as the OTP contact, so a verification code can't be delivered to a client-chosen address.

  - `list_conversations` is scoped to the caller, with the filter applied **in the store's selection** — not after the dispatcher's limit, which would silently return short or empty pages.
  - `create_conversation_session` (resume), `get_session`, `send_message`, and `verify_otp` are owner-checked. Someone else's id is reported **byte-identically** to an id that never existed — the resume path mints a fresh conversation, the rest return the same `SESSION_NOT_FOUND` payload — so none of them can be used as an existence oracle to enumerate other users' ids.
  - Fail-closed: auth enabled + a principal with no email lists nothing and can resume nothing; it never falls back to unscoped. A session stored with no owner is invisible to everyone. Auth **disabled** (the local single-tenant flavor) is the only unscoped path and is unchanged.

  **BREAKING (`SessionStore` implementers).** `list_conversations` now takes a **required** `user_email` parameter — deliberately not an optional defaulting to `None`/unscoped, which would be fail-open and would let a downstream store ship cross-user-leaking without ever confronting the question. `create_session` gains a keyword-only `owner_email`, and `StoredSession` gains `owner_email`.

  Migration: pass the authenticated principal's email through both and filter your selection by it. Pass `None` **only** for a single-tenant, auth-disabled deployment, where it means "unscoped". If you implement this protocol in your own store, treat a not-owned row exactly as a missing one.

## 1.32.1

### Patch Changes

- 3acca21: .NET server: implement turn cancellation (the "Stop button") — the `cancel` action, ported from the Rust reference. `FrameDispatcher` now tracks the connection's single in-flight `send_message` turn with a per-turn `CancellationTokenSource`: a `{"action":"cancel","requestId":"<turn>"}` frame cancels it (dropping the turn at its next await, abandoning the in-flight LLM/tool call) and emits the terminal `cancelled` event (`status: 499`, echoing the turn's `requestId`) in place of `eventual_response`. The partial assistant message is discarded — the user's message, persisted before the agent loop, stays. A cancel with no active turn is a silent no-op; a second `send_message` while a turn is in flight is rejected with `TURN_IN_PROGRESS` rather than run concurrently. A client disconnect aborts the in-flight turn as well, while graceful shutdown still drains it. No engine change.

## 1.32.0

### Minor Changes

- d17ede9: Emit `stream_preamble` from the **.NET server**. It already had the generated protocol type but never produced the event and never read `SMOOTH_AGENT_PREAMBLE_MODEL`, so a host running on the C# server could not turn the feature on at all — this closes that gap and brings the .NET lane to parity with the Rust reference.

  When `SMOOTH_AGENT_PREAMBLE_MODEL` is set (e.g. `groq-gpt-oss-20b`), `TurnRunner` fires a small fast model IN PARALLEL with the agent loop — same gateway and key as the turn, with only the model id and a 64-token output cap overridden — and emits ONE short present-tense "what I'm about to do" sentence as an ephemeral `stream_preamble` event, covering the reasoning model's time-to-first-token. The system prompt is byte-identical to the other servers'.

  It is deliberately defined by what it must never do: the turn never awaits it (it can't delay or gate the answer), an atomic first-answer-token guard drops it the moment real answer tokens start streaming, any failure (timeout, gateway error, bad model id) is logged at debug and swallowed with no error event reaching the client, and the text is never persisted nor folded into `eventual_response`. Unset, empty, or whitespace ⇒ the feature is off, no extra LLM call is made, and behavior is byte-for-byte unchanged.

- bfaf1a8: Go server: emit `stream_preamble`. When `SMOOTH_AGENT_PREAMBLE_MODEL` is set, a small fast model runs in parallel with the turn and streams one ephemeral "what I'm about to do" sentence, covering the reasoning model's time-to-first-token — matching the Rust reference server's prompt, 64-token cap, and first-answer-token race guard. Unset/empty/whitespace leaves behavior and the model-call count unchanged. The preamble is best-effort (failures swallowed) and ephemeral (never persisted, never folded into `eventual_response`).
- ff2e4d9: Python server: emit `stream_preamble`. When `SMOOTH_AGENT_PREAMBLE_MODEL` is set, a small fast model runs concurrently with each streaming turn and emits one ephemeral "what I'm about to do" sentence, covering the reasoning model's time-to-first-token — matching the Rust reference server (same system prompt, same 64-token cap, same gateway/key with only the model id overridden).

  The preamble never delays or gates the real turn, is dropped the instant the first real answer token is emitted, is never persisted or folded into `eventual_response`, and any failure is swallowed at debug. Unset, empty, or whitespace ⇒ off (the default): no extra LLM call, behavior byte-for-byte unchanged.

## 1.31.0

### Minor Changes

- 2fc8486: TypeScript server: emit `stream_preamble` (pearl th-8e0a52).

  The TS server now honours `SMOOTH_AGENT_PREAMBLE_MODEL`, matching the Rust reference. When set, a small fast model runs in parallel with each turn on the same gateway/key (model id + a 64-token cap are the only overrides) and emits ONE ephemeral "what I'm about to do" sentence to cover the reasoning model's time-to-first-token.

  Off by default: unset, empty, or whitespace means no extra LLM call, no extra event, behaviour byte-for-byte unchanged. The preamble is suppressed once the real answer starts streaming, is never persisted or folded into `eventual_response`, and any failure is swallowed at debug so it can never fail or delay a turn.

## 1.30.0

### Minor Changes

- a15fd43: .NET server: `get_conversation_messages` pages by an opaque `cursor` (a message id) instead of the `before` ISO-timestamp cursor, and returns `nextCursor` alongside `hasMore`.

  A timestamp cursor is broken by design — two messages can share a timestamp at any precision the wire keeps, so a `created_at < cursor` filter drops or repeats the collisions. An id cursor names exactly one message. The paging path no longer compares timestamps at all, and the 500-message `before` rescan window (and its paging ceiling) is gone. The .NET client SDK's `GetMessagesAction.Before` becomes `Cursor`, and `GetMessagesResult` gains `NextCursor`. Breaking wire change for clients still sending `before`.

- 5c0fb98: Python server: `get_conversation_messages` pages by opaque `cursor`, not the `before` timestamp.

  The handler now reads `cursor` (a message id today), locates that message in the conversation log, and returns the page immediately older than it. Responses carry `nextCursor` — the id of the oldest message in the page, non-null exactly when `hasMore` is true. An unknown or stale cursor is a `VALIDATION_ERROR` rather than a silently empty page. `createdAt` stays on every message for display, with microsecond precision intact; it is simply no longer the cursor.

  This removes code. The old `_BEFORE_SCAN_WINDOW = 500` bounded rescan existed only because a timestamp cannot locate a position in the log — it capped `before` paging to the newest 500 messages. An id cursor locates the position exactly, so the window, the ISO parsing (`_parse_before`), and the `created_at <` comparison are all gone. There is no timestamp comparison left on the paging path.

  Matches the spec change in #279 and the Rust reference. Tests cover round-trip paging to exhaustion, the identical-`created_at` collision case a timestamp cursor provably cannot survive (the bug the Go server shipped), and the unknown-cursor error.

### Patch Changes

- 075d6e4: Go: commit the type-generation command as `scripts/generate-go.sh` and regenerate `go/protocol/types_gen.go`.

  The command that produced `go/protocol/types_gen.go` was never committed — `go/README.md` deferred to "the original spec" — so Go was the one language whose wire types could not be regenerated. It is now a runnable script, verified to reproduce the previously committed file byte-for-byte from the spec at the commit that last generated it.

  Regenerating picked up everything Go had missed since: `get_messages` now takes an opaque `Cursor *string` (replacing `Before *time.Time`) and returns `NextCursor`, plus the `stream_reasoning` / `stream_preamble` / `cancel` events and the rich-interaction types.

## 1.29.0

### Minor Changes

- 441d198: Go server: page `get_conversation_messages` by opaque id cursor instead of an ISO timestamp.

  The request field `before` (ISO 8601) is replaced by `cursor` (opaque, a message id today), and the response now carries `nextCursor` — the id of the oldest message in the page, non-null exactly when `hasMore` is true. Breaking wire change.

  This removes the failure mode rather than renaming it: a timestamp cursor cannot separate two messages that share a timestamp, so `created_at <` paging silently dropped every message colliding on the cursor's instant. An id names exactly one row. The paging path now contains no timestamp comparison, and the bounded 500-message rescan the timestamp cursor required is gone — an id cursor locates its position in the log directly, so paging has no depth ceiling. `createdAt` is still returned (RFC3339Nano) for display.

  An unknown or stale cursor now returns a `VALIDATION_ERROR` instead of a silent empty page.

- bd836c3: TypeScript server: `get_conversation_messages` now pages on an opaque `cursor` (a message id) instead of the `before` ISO-timestamp cursor, and returns `nextCursor` alongside `hasMore`.

  Breaking wire change. A timestamp cursor cannot page a log correctly — two messages can share a `createdAt` at any precision the wire keeps, so a `createdAt < cursor` filter drops or repeats the messages that collide. The cursor now names exactly one message: the page starts immediately after it, on the older side. `nextCursor` is the oldest message in the page, non-null exactly when `hasMore` is true; an unknown cursor is a `VALIDATION_ERROR` rather than a silent empty page.

  This also removes the 500-message bounded rescan the timestamp cursor required, so paging is no longer capped at the newest 500 messages. `createdAt` stays on every message for display.

## 1.28.0

### Minor Changes

- b135852: Protocol: `get_conversation_messages` pagination moves from a timestamp cursor to an opaque cursor.

  `before` (ISO 8601 timestamp) is replaced by `cursor` (opaque, storage-defined — a message id today), and the response gains `nextCursor`, non-null exactly when `hasMore` is true. Page by feeding `nextCursor` back as the next request's `cursor`.

  A timestamp is the wrong cursor: two messages can share a timestamp at any precision the wire format preserves, so a `created_at < cursor` filter either drops or repeats the messages that collide. This is not hypothetical — the Go server shipped whole-second `RFC3339` and silently dropped every message sharing a second from page two. An id names exactly one message and cannot collide. The Rust server already paginated this way; this makes the spec match the design that was already correct.

  Breaking on paper, inert in practice: a survey of every consumer (smooai, smooth, heypage) found no caller that pages — all are single-fetch, none passes `before`, none reads `hasMore` or `nextCursor`.

  Also regenerates all client type sets from `spec/`, which had drifted badly. The regen pulls in schema changes that landed without regeneration (`cancel`, `submit_interaction`, `interaction_required`/`interaction_invalid`) and surfaces a latent bug: `organizationId` became required on `Session` in spec PR #97, but Python's model was never regenerated, so the Python client has been accepting sessions a conformant server would reject ever since.

## 1.27.7

### Patch Changes

- 95524bc: Python server: regression test pinning sub-second precision on `get_conversation_messages`' `createdAt`. The handler already emits full microsecond precision (`datetime.isoformat()` on a tz-aware UTC value), but nothing guarded it — clients page by handing the oldest `createdAt` back as `before`, and a second-truncated cursor makes the strict `<` filter drop every message sharing that second. Matches the Go (#264) and TypeScript (#273) fixes.
- d730dac: TypeScript server: user-initiated turn cancellation (the "Stop button"), mirroring the Rust reference (PR #259). A client stops the in-flight turn with `{"action":"cancel","requestId":"<the send_message requestId>"}`; the server aborts that turn and emits a terminal `cancelled` event (`status: 499`, requestId echoed at the envelope level and inside `data`) **in place of** the `eventual_response` — so a turn always emits exactly one terminal event. A cancel with no active turn is a silent no-op. Only ONE turn runs per connection: a second `send_message` while one is in flight is rejected with error code `TURN_IN_PROGRESS` rather than run concurrently (`confirm_tool_action` / `verify_otp` are turn _resumes_, so they're unaffected). A cancelled turn's partial assistant reply is DISCARDED (never persisted); the user's message, persisted at the start of the turn, stays. A client disconnect mid-turn now also aborts the turn, while the graceful SIGTERM drain still lets an in-flight turn finish.

  Implementation is connection-local, matching the Rust approach: the turn is already spawned as a background task (so the reader stays free to receive `confirm_tool_action` while a turn is parked), so the dispatcher tracks it as the connection's single active turn along with a per-turn `AbortController`, and fires it on cancel/disconnect. Cancellation is cooperative — JS can't drop an in-flight `await` the way tokio drops a future — so a turn parked inside a long tool call stops at the next stream event; the observable protocol contract is identical either way.

## 1.27.6

### Patch Changes

- 91078ac: TypeScript server: regression test pinning sub-second `createdAt` precision on `get_conversation_messages`. A server that formats `createdAt` at whole-second precision breaks the documented paging loop — clients feed page one's oldest `createdAt` back as `before`, and a strictly-less-than filter against a truncated cursor silently drops every message sharing that second. The TS server was already correct (`Date#toISOString`, millisecond precision, passed through unreformatted); the test locks it in.

## 1.27.5

### Patch Changes

- ac1da05: Go server: implement turn cancellation — the `cancel` action (the "Stop button"), porting the Rust reference. A connection now runs at most ONE agent turn at a time: `send_message` registers its turn with a cancellable context, a `cancel` frame cancels it and emits the terminal `cancelled` event (`status: 499`, echoing the cancelled turn's `requestId` at the envelope level and inside `data`), and a second `send_message` while a turn is in flight is rejected with `TURN_IN_PROGRESS` rather than run concurrently. A cancel with no active turn is a silent no-op. A cancelled turn discards its partial assistant message (never persisted) and emits no `eventual_response`; the user's message stays persisted. A client disconnect mid-turn aborts the turn the same way, while the SIGTERM graceful-drain path still lets an in-flight turn finish.

## 1.27.4

### Patch Changes

- b910a11: Python server: implement the `get_conversation_messages` action. It previously fell through to `UNSUPPORTED_ACTION`, so a web client resuming a conversation against the Python server rendered no history. The handler mirrors the merged Go/Rust reference and the `spec/actions/get-messages.schema.json` contract: newest-first `messages` (id, direction, content.text, createdAt) plus `hasMore`, with `limit` (1..100, default 50) and an optional ISO 8601 `before` cursor. `StoredMessage` gains a defaulted `created_at` timestamp to back the `createdAt` field and the cursor.
- c64b97b: TypeScript server: implement the `get_conversation_messages` action. Its dispatcher switch stopped at `verify_otp`, so the action fell through to `UNSUPPORTED_ACTION` and a web client resuming a conversation against the TS server rendered no history. The handler mirrors the merged Go/Rust references and the `spec/actions/get-messages.schema.json` contract: newest-first `messages` (id, direction, content.text, createdAt) plus `hasMore`, with `limit` (1..100, default 50) and an optional ISO 8601 `before` cursor. `StoredMessage` gains an optional `createdAt` timestamp (set by `InMemorySessionStore.appendMessage`) to back the `createdAt` field and the cursor.

## 1.27.3

### Patch Changes

- 0e65c59: Go server: emit `createdAt` with sub-second precision (`RFC3339Nano`) from `get_conversation_messages`. Clients page by handing the oldest `createdAt` back as `before`, which is filtered strictly-less-than against the store's full-precision timestamp — whole-second `RFC3339` truncation put the cursor _before_ the message it named, so every message sharing that second silently vanished from page two. Also aligns the Go wire format with the .NET server, which already round-trips full precision.

## 1.27.2

### Patch Changes

- e5bb69c: .NET server: implement the `get_conversation_messages` action

  The .NET `FrameDispatcher` answered `UNSUPPORTED_ACTION` for
  `get_conversation_messages`, so a C#-hosted server couldn't page conversation
  history the way the Rust/Go/TS servers can — a client resuming a conversation
  had no way to load prior messages. It now returns `{messages, hasMore}`
  newest-first per `spec/actions/get-messages.schema.json`, with `limit` (1–100,
  default 50) and an optional ISO 8601 `before` cursor.

  `StoredMessage` gained a `CreatedAt` init-only property (not a positional
  parameter, so downstream `ISessionStore` implementations keep compiling) that
  the Postgres store now reads from — and returns on append via — the existing
  `conversation_messages.created_at` column.

## 1.27.1

### Patch Changes

- d6c63d7: Go server: implement the `get_conversation_messages` action. It previously fell through to `UNSUPPORTED_ACTION`, so a web client resuming a conversation against the Go server rendered no history. The handler mirrors the Rust reference and the `spec/actions/get-messages.schema.json` contract: newest-first `messages` (id, direction, content.text, createdAt) plus `hasMore`, with `limit` (1..100, default 50) and an optional ISO 8601 `before` cursor. `StoredMessage` gains a `CreatedAt` timestamp to back the `createdAt` field and the cursor.

## 1.27.0

### Minor Changes

- 1765f6e: Add a built-in ACL-scoped `knowledge_search` tool to the .NET server. Registering an `IAccessKnowledge` already grounds turns via RAG auto-context; this exposes the same store as a model-callable tool a host enables by name (`knowledge_search`) — no hand-wrapped `AIFunction` required. It's built per-turn over the connection's `IAccessKnowledge.ForAccess(access)` handle, so every search is document-level access-controlled (a doc outside the caller's ACL is never a candidate), and matches the Rust server's tool for parity: same name, args (`query` required + `limit` clamped 1..10, default 3), and text result shape.
- 508de9d: dotnet: add a Notion `IConnector` (`NotionConnector`) to the server. Recurses `blocks/{id}/children` (paginated, `Notion-Version: 2022-06-28`, integration-token auth), flattens `paragraph`/`heading_1-3`/`bulleted_list_item`/`numbered_list_item`/`quote`/`code`/`toggle` rich_text (plus nested toggle/list-item bodies) into document text, and emits a `child_page` block as its own recursed document rather than inlining it. The document id is the canonical Notion page id and the source is the page URL, so citations link back and re-ingesting overwrites in place. Each configured `NotionRoot` carries a `DocumentAcl`, stamped onto every document under that root (`SourceDocument` gains an optional `Acl`).

### Patch Changes

- c6f202b: dotnet server: TurnRunner degrades gracefully when knowledge retrieval fails. When the embedding gateway / vector store is down, `QueryAsync` used to propagate out of the turn and the dispatcher surfaced `INTERNAL_ERROR`, killing the whole turn. Now the retrieval failure is caught: the turn proceeds with empty grounding (no citations, and the failing store isn't handed to the engine's own RAG query), and a warning is logged. Only the retrieval is wrapped — the rest of the turn is unchanged.

## 1.26.0

### Minor Changes

- 798f447: Per-agent write-confirmation (HITL) patterns. `AgentConfig` gains a
  `ConfirmToolPatterns` field so a multi-agent host can gate tools behind a
  `confirm_tool_action` round-trip per agent instead of sharing the single global
  `ConfirmTools` DI singleton. The dispatcher uses the per-agent patterns when the
  agent specifies them (an explicit empty list disables gating for that agent) and
  falls back to the global `ConfirmTools` when it doesn't — fully backward
  compatible.

### Patch Changes

- 8a0eae9: .NET ingestion parity: paragraph-aware chunker + content-hash IngestLedger

  Bring the .NET `Chunker` to parity with the Rust ingestion chunker — ~500-char
  paragraph-aware chunks (blank-line units, oversized paragraphs hard-split on word
  boundaries, greedy packing) with 64-char whole-word trailing overlap and stable
  `{documentId}#{index}` chunk ids (replacing the old whitespace-break 1200/150
  sliding-window splitter). Add a new `IngestLedger` with FNV-1a content-hash
  idempotency (byte-identical to Rust's `content_hash`) so re-ingesting identical
  content is a no-op while changed content is reprocessed; wire it through
  `IngestPipeline` (skips unchanged documents, dedupes identical chunks).

## 1.25.0

### Minor Changes

- a69d091: Add a .NET Slack `IConnector` (`SlackConnector`) for knowledge ingestion. Resolves author names
  via `users.list`, lists channels via `conversations.list`, and pages messages via
  `conversations.history`. Emits one document per channel per day with a stable id
  `slack:{channel}:{date}` (today re-hashes as messages land, past days dedupe on the pipeline's
  (id, hash) key), `source` = the day's first-message permalink (`chat.getPermalink`), incremental
  pulls via an `oldest` cursor, and a per-channel ACL label. `SourceDocument` gains an optional
  `Acl` field to carry per-document access labels (mirrors the Rust `RawDocument.acl`). Threaded
  replies (`conversations.replies`) are deferred to a follow-up.

### Patch Changes

- aa72bb0: Make the two .NET Server add-on packages publishable to NuGet and bump the Core pin. `SmooAI.SmoothOperator.Server.AspNetCore` (the ASP.NET Core WebSocket host) and `SmooAI.SmoothOperator.Server.Postgres` (the durable Postgres session store) now carry NuGet packaging metadata, get their `<Version>` stamped in lockstep by `sync-versions.mjs`, and are packed + pushed by `ci-publish.mjs` alongside the base `SmooAI.SmoothOperator.Server` package — so downstream hosts can `PackageReference` them instead of vendoring the extension source. The Server package's `SmooAI.SmoothOperator.Core` pin is also bumped from 1.5.0 to the latest published 1.7.0.

## 1.24.0

### Minor Changes

- 14070ec: Add a host-callable seam to start an agent turn server-side (`IServerInitiatedTurns`, registered by `AddSmoothOperatorServer`). A host — e.g. `POST /webhooks/datadog` saying "investigate this alert" — can now create a conversation and run a turn without a client `send_message` frame. It reuses the same `TurnRunner` + `ISessionStore` path as the client flow, so the inbound message and streamed reply persist identically: a client that later lists or resumes that conversation sees it the same as a client-initiated one. Interactive per-connection concerns (write-confirmation HITL, OTP gating) are intentionally omitted. Live push to already-connected sockets is deferred — the durable message log is the surface clients read.

## 1.23.4

### Patch Changes

- 607f81d: docs: refresh the .NET server docs to match the shipped 1.23.x surface. `dotnet/server/README.md`'s "What's shipped/Next" list and `docs/Architecture/Polyglot Cores.md`'s service-layer intro both lagged the published dll — knowledge grounding, ACL-filtered retrieval, citations, the reranker, GitHub ingestion + connectors, HITL write-confirmation, the `/admin/*` API, and the deployable host all ship in C# now. Corrected the stale "not yet built in C#" framing and marked the genuinely-open items (Notion/Slack connectors in-flight, checkpoint-adapter resume wiring).

## 1.23.3

### Patch Changes

- 7a53f95: Docs: add branded, NuGet-page READMEs for `SmooAI.SmoothOperator.Server.AspNetCore`
  and `SmooAI.SmoothOperator.Server.Postgres`. Each explains what the package is,
  how to install and use it (real API surface — `AddSmoothOperatorServer` /
  `MapSmoothOperatorWebSocket` / `ConfirmTools`; `PostgresSessionStore` /
  `PostgresAclKnowledgeStore`), and cross-references the rest of the .NET family
  (Core, Server, AspNetCore, Postgres, client). Wired each via `PackageReadmeFile`
  so it renders on nuget.org once the packages are published.

## 1.23.2

### Patch Changes

- 4b2b5d7: Conversation-workflow adherence (th-d57a1d): the rendered `<ConversationWorkflow>` step section now instructs the agent to ask the current step's question directly and never re-ask for permission / re-confirm readiness / repeat an answered question (gpt-oss-class models over-indexed on the old "you don't have to force the step to close" line and looped on re-confirmation). The workflow judge now counts brief/terse answers that address the step ("a four", "sure") as satisfying it instead of holding out for elaboration. Same wording change applied across all five language servers (TS, Rust, Python, Go, .NET).

## 1.23.1

### Patch Changes

- b60234e: Wire Changesets to drive lockstep publishing for every polyglot server artifact — npm + NuGet + PyPI + crates.io — closing the npm-only gap.

  - `scripts/sync-versions.mjs` now also stamps the .NET server package (`SmooAI.SmoothOperator.Server.csproj` `<Version>`) and the PyPI server package (`python/server/pyproject.toml`), and fails loudly if any manifest anchor is missing (never publishes an out-of-lockstep set).
  - New `scripts/ci-publish.mjs`: a single idempotent orchestrator that runs sync-versions first, then publishes npm → NuGet → PyPI (client + server) → crates.io, each existence-checked + skip-if-already-published, with a `DRY_RUN=1` path that packs/validates but uploads nothing. One registry's failure no longer skips the others; any hard failure exits non-zero. `ci:publish` now points at it.
  - `release.yml` folds the previously-inline crates.io/PyPI steps into `ci:publish` and adds the NuGet publish token, so the whole polyglot release goes through one orchestrator.

- b60234e: Docs: elevate the server + registry-landing READMEs into a narrative story. Root
  README gets a sharper problem→vision hook, a "safe by construction" section
  (ToolHook auth-gate + per-agent allow-list + document ACLs + SEP allowlist), and
  a clean language→client→server→registry table. Each per-language server README
  (Rust crates.io crate, TypeScript, Python, Go, .NET) now leads with a hook, a
  "spin up a real agent server in N lines" snippet, an honest "extending via
  tools + guardrails" example in that language's real API, badges, and the polyglot
  table. No code changes; accuracy verified against the shipped surface.

## 1.23.0

### Minor Changes

- d3d3abe: Two additive SEP-protocol enhancements on the streaming path (directive nav + business-card images), both optional and back-compatible.

  **Directive-over-SEP.** `eventual_response` gains an optional `directive` field — an opaque client-side directive (e.g. a Navigate / ApplyView instruction) a host tool emitted this turn. The runner threads a `directive_sink` into the `ToolProviderContext` (new `with_directive_sink` builder), drains it after the turn (last-write-wins, mirroring the citation sink), and carries the value onto `TurnResult::directive`. The protocol layer never interprets the shape — the host client owns it, exactly like `response`. Absent when no host tool wrote one, so the event is byte-for-byte unchanged for existing clients. Added to `spec/events/eventual-response.schema.json` and `spec/actions/send-message.schema.json` `$defs/Response`, and to the TypeScript SDK.

  **Image-through-SEP.** `send_message` gains an optional `images` array (`{ url, detail? }`) for multimodal turns. A new facade `UserImage` type flows from the inbound request into `TurnRequest::images` and the `ToolProviderContext` (new `with_images` builder); when non-empty the runner maps each onto a core `ImageContent` and attaches them to the engine's user message via `AgentConfig::with_user_images` (requires core `0.16.2`). Parsing is fail-soft (a malformed `images` entry is dropped, never rejects the turn). Empty/absent ⇒ a text-only turn, unchanged. Added to `spec/actions/send-message.schema.json` `$defs/Request` and the TypeScript SDK.

## 1.22.17

### Patch Changes

- 57c7a02: Add an optional fast-model **preamble** to streaming turns to cover the reasoning model's time-to-first-token. When the server is configured with `SMOOTH_AGENT_PREAMBLE_MODEL` (e.g. `groq-gpt-oss-20b`), a small fast model runs IN PARALLEL with the main turn and streams ONE short present-tense "what I'm about to do" sentence over a new `stream_preamble` wire event — an ephemeral status line the real answer replaces. It's best-effort (any error/slowness is swallowed on its own task) and guarded: it's dropped if the real answer has already begun streaming, so it can never block or corrupt a turn. Unset ⇒ no extra call and byte-for-byte unchanged behavior. Adds `stream-preamble.schema.json` to the SEP spec and `StreamPreamble` to the TypeScript SDK union.

## 1.22.16

### Patch Changes

- 33a92bd: Persist conversation-workflow step state to shared storage (th-c12df5). The step pointer (`currentStepId`) and per-step attempt counter were held in the per-pod in-memory session map, so on a widget reconnect or a pod hop they reset to step 0 — the workflow froze on its first step, the judge/attempt-cap could never advance it, and any per-step rich elements (quick-reply chips today, richer message elements later) were pinned to that first step. They now live on the conversation's `metadata_json` (shared storage, keyed by the stable `conversation_id`) and load per turn, so a workflow resumes on the right step across reconnects and replicas. Element-agnostic — the fix moves the step pointer, not the emitted content.

## 1.22.15

### Patch Changes

- fa6d913: Deterministic workflow chips (th-d57a1d). `ConversationWorkflowStep` gains an optional `suggestedReplies: string[]`; when the agent is on a step that declares it, the server emits those canonical answers as the response's `suggestedNextActions`, overriding any model-invented chips. This makes quick-reply chips fire on every such step (reliable, not model-dependent) and — because a tapped chip is clean, canonical input — fixes the assessment stalling where the judge would not advance on terse free-text answers. Free-form steps declare none, leaving model behavior unchanged.

## 1.22.14

### Patch Changes

- 2476916: Add a per-step attempt cap to the conversation-workflow judge so a guided assessment can't stall forever on one step. The judge only advances on `yes`; when a step's criteria demand evidence the judge never accepts, the step re-asks indefinitely and a multi-step flow (e.g. the public Transformation Posture agent) never reaches its scoring / lead-capture step (th-d57a1d). The step pointer already persists and advances correctly — this adds the missing escape hatch: `apply_step_cap` force-advances to the next step after `WORKFLOW_STEP_ATTEMPT_CAP` (3) consecutive non-advancing turns, resetting the counter on any advance. The counter persists in session metadata (`stepAttempts`) alongside the existing `currentStepId` pointer. With tuned criteria the cap rarely fires — it's the safety net for a pathological non-answering visitor.

## 1.22.13

### Patch Changes

- 98e7c06: Server: deterministic backstop against a degenerate LLM repetition loop spamming
  the chat widget. `general_agent_response` now collapses runaway near-identical
  filler in the finalized reply — splits on paragraph breaks, drops paragraphs
  near-identical to one already kept, and caps the count — before it reaches the
  widget. A healthy reply is returned byte-for-byte unchanged.

## 1.22.12

### Patch Changes

- Harden chat streaming + fix gpt-oss suggested-reply chips.

  - `chat_stream` now retries retryable HTTP statuses (429/5xx) before reading any
    stream bytes, mirroring the non-streaming `chat()` path. A transient gateway
    5xx (groq/LiteLLM 502/503) previously propagated as an `AGENT_ERROR` and the
    chat widget rendered an empty reply. Bumps the core dep to 0.16.2 (where the
    retry lives).
  - `extract_suggested_replies` now also parses a trailing markdown
    `Suggested replies:` list, so models that ignore the `<suggested_replies>`
    marker (gpt-oss-120b) still populate chips.

## 1.22.11

### Patch Changes

- fix(rust): require core `^0.16.1` so `with_model_ceiling` resolves from crates.io

  Server 1.22.10 calls `AgentConfig::with_model_ceiling` / `LlmClient::with_model_ceiling`,
  but its published manifest required core `^0.16` — and crates.io topped out at core
  **0.16.0**, which predates those methods. So any external `cargo build` against the
  published server resolved the broken 0.16.0 and failed to compile (the chips/empty-reply
  reasoning-channel fix was un-buildable off crates.io). Core **0.16.1** is now published
  with the API; pin the floor at `0.16.1` and drop the stopgap `git`/`rev` pin so the
  published server resolves the fixed engine.

## 1.22.10

### Patch Changes

- 22b193e: Fix `eventual_response` still shipping an empty reply (blank `responseParts` + empty `suggestedNextActions`) on gpt-oss-120b via the LiteLLM/groq gateway, which 1.22.1 did not cover.

  Confirmed empirically against the real SSE parser: this gateway/model emits the WHOLE answer on the reasoning channel (`delta.reasoning_content`) with `delta.content` never populated. The engine accumulates reasoning into a separate buffer and drops it from `response.content`, so BOTH `last_assistant_content()` and the 1.22.1 `streamed_reply` (content tokens) come back empty — even though the answer streams to the client as `stream_reasoning` and persists. The "streamed tokens" observed in prod were `stream_reasoning` frames (protocol-identical to `stream_token`), not content.

  `rust/smooth-operator-server/src/runner.rs`: accumulate the turn's reasoning stream and use it as a LAST-RESORT fallback for the final reply — after `last_assistant_content()` and `streamed_reply`, only when no answer content exists anywhere. A normal reasoning model always populates `content`, so it never surfaces its thinking as the answer; this rung fires solely for the degenerate answer-in-reasoning case where the alternative is an empty response. The suggested-replies trailer is preserved through the fallback so suggestions are recovered.

  Adds `tests/gateway_wire_empty_reply.rs`, a regression that drives the real `LlmClient` against a local mock speaking the gateway SSE wire format (answer-in-content and answer-in-reasoning shapes) — it fails if the reply goes empty again.

## 1.22.9

### Patch Changes

- 01c434e: Fix auto-title producing empty titles. The auto-title model (`groq-gpt-oss-20b`) is a reasoning model whose reasoning tokens count against `max_tokens`, so the original 32-token cap was fully consumed by reasoning and left the completion content empty — the titler then silently kept the default `Session <uuid>` name. Raise the auto-title budget to 512 (the title itself is still capped to `TITLE_MAX` chars by `sanitize_title`), extract `title_request_body` so the budget is unit-tested, and add tracing at each auto-title bail point (debug for the expected "already named" skip, warn for real failures).

## 1.22.8

### Patch Changes

- 6e994ad: SDK: `SmoothAgentClient.listConversations()` + `conversationId` resume typing — the client surface for a conversation sidebar (pearl th-2f028f).

  - New `listConversations({ limit? })` method wrapping the server's `list_conversations` action; resolves to `{ conversations: [{ conversationId, title, updatedAt, messageCount }] }` (most-recent-first). Exports `ConversationSummary` / `ListConversationsResponse`.
  - `createConversationSession` now accepts an optional `conversationId` (already honored by the server) to RESUME an existing conversation; pair it with `getMessages` to load the transcript.
  - Additive and back-compat.

  Also adds `examples/web-chat` — a private, runnable Vite + React reference chat client built on this SDK (token streaming, inline tool-call/result blocks, HITL approvals, conversation sidebar, oldest-first history). Not published.

## 1.22.7

### Patch Changes

- 487d10b: Rust server: conversation auto-title (small model) + `rename_conversation` (pearl th-d5b446).

  - **Auto-title** — after the first assistant turn on a conversation still carrying its default `Session <uuid>` name, a best-effort, detached, non-blocking task asks the fast/cheap `groq-gpt-oss-20b` model for a short 3-6 word title over the first exchange and stores it as the conversation `name`. Fail-safe: any error (no gateway key, gateway failure, empty output, storage error) simply leaves the default name — a turn is never slowed or broken. The default-name guard (re-checked right before the write) means a manual rename is never clobbered, and a titled conversation won't re-fire.
  - **`rename_conversation`** — new WS action `{action, requestId, conversationId, title}`: sanitizes/trims the title (rejects empty), 404s an unknown conversation, persists `name` via the storage adapter's existing `update_conversation`, and replies `immediate_response` (200) with `{ conversationId, title }`.
  - `list_conversations` now surfaces a **meaningful** conversation `name` (auto-title or manual rename — anything not the default `Session <uuid>`) as the sidebar title, falling back to the first-inbound message preview for un-titled conversations. Back-compat: every pre-titling conversation carried the default name, so the message-preview behavior is unchanged for them.

  Additive + back-compat. New tests cover title sanitization (quotes/markdown/whitespace/length), the default-name-only auto-title guard (mock gateway, never clobbers a manual name, no-key fail-safe), rename success + list surfacing, empty-title rejection, and unknown-id 404.

## 1.22.6

### Patch Changes

- 9b842d7: .NET server: conversation-history / resume substrate for the WS protocol (pearl th-d5b446) — C# parity with the merged Rust reference (and the Go/TS mirrors) so every client (daemon PWA, `th code` TUI, chat-widget) can build a conversation sidebar + resume against the .NET server too.

  - New WS action `list_conversations` (`{action, requestId, limit?}`, default limit 50): replies via `immediate_response` (200, message "Conversations") with `{ conversations: [ { conversationId, title, updatedAt, messageCount } ] }`, most-recent-first, filtered to conversations with `messageCount > 0` (drops the empty conversations every page-load mints). `title` = a ~60-char preview of the first inbound message with leading markdown/control chars stripped, falling back to a generic name; `updatedAt` = ISO-8601.
  - `create_conversation_session` gains an optional `conversationId`: when it names a known conversation, the new session RESUMES — reuses that conversation's id and keeps its message log, so `send_message` appends to it and the runner replays its history. Absent/unknown id ⇒ a fresh conversation is minted (byte-for-byte unchanged behavior).
  - Additive and back-compat: no `conversationId` / no `list_conversations` call = unchanged behavior. `ISessionStore` grows `ResumeSessionAsync` + `ListConversationsAsync` (+ a `ConversationSummary` record), implemented by both `InMemorySessionStore` (tracks per-conversation last-activity) and `PostgresSessionStore`; the shared `SessionStoreContractTests` cover both.

## 1.22.5

### Patch Changes

- b367240: Python server: `list_conversations` + resume-by-`conversationId` (pearl th-d5b446) — Python parity with the merged Rust/Go/TS reference so every client (daemon PWA, `th code` TUI, chat-widget) can build a conversation sidebar + resume against the Python server too.

  - New WS action `list_conversations` (`{action, requestId, limit?}`, default limit 50): replies via `immediate_response` (200, "Conversations") with `{ conversations: [ { conversationId, title, updatedAt, messageCount } ] }`, most-recent-first, filtered to conversations with `messageCount > 0` (drops the empty conversations every page-load mints). `title` = a ~60-char preview of the first inbound message with leading markdown/control chars stripped, falling back to a generic name; `updatedAt` = ISO-8601.
  - `create_conversation_session` gains an optional `conversationId`: when it names a known conversation, the new session RESUMES — reuses that conversation's id and keeps its message log, so `send_message` appends to it and the runner replays its history. Absent/unknown id ⇒ a fresh conversation is minted (unchanged behavior).
  - Additive and back-compat: no `conversationId` / no `list_conversations` call = unchanged behavior. `SessionStore` gains `list_conversations()` + an optional `conversation_id` arg on `create_session`; the in-memory store tracks per-conversation last-activity for the sort key.

## 1.22.4

### Patch Changes

- 9ba82d1: Go server: conversation-history / resume substrate for the WS protocol (pearl th-d5b446) — Go parity with the merged Rust reference so every client (daemon PWA, `th code` TUI, chat-widget) can build a conversation sidebar + resume against the Go server too.

  - New WS action `list_conversations` (`{action, requestId, limit?}`, default limit 50): replies via `immediate_response` (200, message "Conversations") with `{ conversations: [ { conversationId, title, updatedAt, messageCount } ] }`, most-recent-first, filtered to conversations with `messageCount > 0` (drops the empty conversations every page-load mints). `title` = a ~60-char preview of the first inbound message with leading markdown/control chars stripped, falling back to a generic name; `updatedAt` = ISO-8601 (RFC 3339).
  - `create_conversation_session` gains an optional `conversationId`: when it names a known conversation, the new session RESUMES — reuses that conversation's id and keeps its message log, so `send_message` appends to it and the runner replays its history. Absent/unknown id ⇒ a fresh conversation is minted (byte-for-byte unchanged behavior).
  - Additive and back-compat: no `conversationId` / no `list_conversations` call = unchanged behavior. `go/server/{dispatcher,session_store}.go` only. In-memory store tracks per-conversation last-activity for the sort key.

## 1.22.3

### Patch Changes

- 1644852: Rust server: conversation-history / resume substrate for the WS protocol (pearl th-d5b446) — the contract every client (daemon PWA, `th code` TUI, chat-widget) builds a conversation sidebar + resume against.

  - New WS action `list_conversations` (`{action, requestId, limit?}`, default limit 50): replies via `immediate_response` (200) with `{ conversations: [ { conversationId, title, updatedAt, messageCount } ] }`, most-recent-first, filtered to conversations with `messageCount > 0` (drops the empty conversations every page-load mints). `title` = a ~60-char preview of the first inbound message, falling back to the conversation `name`; `updatedAt` = ISO-8601.
  - `create_conversation_session` gains an optional `conversationId`: when it names an existing conversation, the new session RESUMES — reuses that conversation's id + org and skips `create_conversation`, so `send_message` appends to it and the runner replays its history via `thread_id`. Absent/unknown id ⇒ a fresh conversation is minted (byte-for-byte unchanged behavior).
  - Additive and back-compat: no `conversationId` / no `list_conversations` call = unchanged behavior. `handler.rs` only.

## 1.22.2

### Patch Changes

- 6306e36: TypeScript server: model-output ceiling clamp + raised starvation-prone defaults (EPIC th-1cc9fa), matching the Rust/Python server reference.

  - `typescript/server/src/modelCeiling.ts`: best-effort per-model output ceiling from the gateway's `/model/info` (`extractModelCeilings` + `createGatewayModelCeilingResolver`), cached once per process, `undefined` on any error ⇒ engine leaves `max_tokens` unclamped.
  - `turnRunner.ts`: raise `DEFAULT_MAX_TOKENS` 512→8192 and `DEFAULT_MAX_ITERATIONS` 6→20 (chat-widget sizing starved reasoning models), thread the per-turn ceiling into the engine via `AgentOptions.modelMaxOutput`, and set an explicit `DEFAULT_MODEL` shared by the request and the ceiling lookup.
  - Thread `model` + `modelCeiling` through `FrameDispatcher`, `ServerOptions`, `serveLocal`; `main.ts` builds the resolver from `SMOOAI_GATEWAY_URL`/`KEY` (undefined on the keyless local path ⇒ unclamped, behaviour unchanged).
  - Bump `@smooai/smooth-operator-core` pin to `^0.20.4` (the published release introducing `modelMaxOutput` / `effectiveMaxTokens`).

## 1.22.1

### Patch Changes

- 17e1ad9: Fix intermittently empty `eventual_response` on the streaming turn (blank `responseParts` + dropped `suggestedNextActions`) even though the full reply streamed and persisted.

  The runner sourced the final reply from `Conversation::last_assistant_content()`. On reasoning models (e.g. `groq-gpt-oss-120b`) a turn can end on a tool-call or reasoning-only assistant entry whose `content` is empty, so that returned `""` — shipping an empty `eventual_response` and losing the parsed suggestions.

  `rust/smooth-operator-server/src/runner.rs`: accumulate THIS turn's raw streamed answer tokens (pre-suppressor, reasoning excluded — identical to the engine's assistant `content`) and fall back to it when `last_assistant_content()` is empty. The suggested-replies trailer is preserved in the fallback so `extract_suggested_replies` strips it and recovers the suggestions exactly as on the normal path. The non-empty path is byte-for-byte unchanged.

## 1.22.0

### Minor Changes

- 998e270: SMOODEV-2172 — per-agent `model` and `max_iterations` overrides. `AgentBehaviorConfig`
  now carries `model: Option<String>` (per-agent gateway model id) and
  `max_iterations: Option<u32>` (per-agent agent-loop cap), parsed from optional
  `agents.model` (text) and `agents.max_iterations` (integer) row values. Blank models
  are ignored; `max_iterations` is clamped to `1..=64` with a `warn` on clamp.

  At turn time the operator server threads both through: the model resolves highest-wins
  as per-turn `send_message.model` (Smooth Modes) → per-agent `agents.model` →
  `SMOOTH_AGENT_MODEL`; the loop cap resolves per-agent `agents.max_iterations` →
  `SMOOTH_AGENT_MAX_ITERATIONS`. `None` at every layer falls back to the global env
  default exactly as before, so a standalone deploy is byte-for-byte unchanged. The
  reference Postgres adapter reads both columns tolerantly — a DB predating them degrades
  to the global default (no migration-ordering dependency).

## 1.21.4

### Patch Changes

- 2d2ab24: Consume `smooai-smooth-operator-core` from crates.io (0.16) instead of the sibling
  path dep, and collapse the image build to a single-repo Docker context.

  - `rust/Cargo.toml`: `smooai-smooth-operator-core` path dep → `"0.16"` (published crate).
  - `Dockerfile`: drop the sibling `smooth-operator-core` COPY; context is this repo alone (cargo fetches the engine crate from crates.io).
  - `deploy/scripts/kind-smoke.sh`: build from the repo root, drop `PARENT_DIR`/`SIBLING_DIR`.
  - `.github/workflows/pr-kind-deploy-smoke.yml`: drop the sibling checkout + `ref:` pin + `PARENT_DIR` env.

  `Cargo.lock` regen + `cargo build --locked` verification happen AFTER 0.16.0 is
  published to crates.io.

## 1.21.3

### Patch Changes

- 0da6007: SMOODEV-2328 — OpenTelemetry GenAI agent spans on the production streaming path.

  The reference server drives every real turn through `runner::run_streaming_turn`,
  which previously emitted **no** `gen_ai.*` spans (only the secondary
  `KnowledgeChatRuntime::run_turn` was instrumented). Both paths now emit the
  identical span shape so agent turns flow via OTLP to the observability studio:

  - Per-turn `gen_ai.chat` span now also carries `gen_ai.agent.name` and — on the
    streaming path — `smooai.org_id` (matching the monorepo TS chat handler's
    attribute exactly, so the studio groups Rust + TS turns by org), alongside the
    existing system / model / conversation.id and aggregated token usage.
  - Per-tool `gen_ai.tool` child span now carries the tool's `gen_ai.tool.call.arguments`
    (redacted via `telemetry::redact_tool_arguments`, which scrubs secret-named JSON
    keys and caps length) plus an `otel.status_code`=`ERROR` + message on failure,
    in addition to the existing tool name / latency / is_error.

  OTLP export was already wired end-to-end (`init_telemetry()` in both server and
  lambda `main.rs`, gated on `OTEL_EXPORTER_OTLP_ENDPOINT`). No per-LLM-call
  inference span yet — that needs `smooth-operator-core` to emit per-call usage +
  finish-reason, tracked separately.

## 1.21.2

### Patch Changes

- 25adb5c: th-6784a6 — sync to core@main + pin the CI core checkout so a moving core can't
  silently break every PR.

  `pr-kind-deploy-smoke.yml` checked out `SmooAI/smooth-operator-core` with no
  `ref`, so when core@main advanced (multimodal `Message.images` field), this
  repo's `main` stopped compiling against it and `cargo build --locked` failed the
  lock check — turning every open PR red for reasons unrelated to its own diff.

  - Add `images: vec![]` to the two `EngineMessage` constructions (replayed
    text-only history) in `runtime.rs` and `runner.rs`.
  - Fix stale test literals missing new struct fields: `suggested_replies.rs`
    (`identity_intake` → `interactions`, removed in #176) and `serve_smoke.rs`
    (`ServerConfig` + `TurnRequest` new fields).
  - Regenerate `Cargo.lock` against core@main so `--locked` passes.
  - Pin the CI core checkout to a known-good SHA
    (`3c7b21dbde4f31519b2eab3d5343f154119fe655`), documented as interim until
    core publishes to crates.io. Bump it deliberately alongside
    `cargo update -p smooai-smooth-operator-core`.

## 1.21.1

### Patch Changes

- 909443a: SMOODEV-2259 — per-agent SEP extension enablement: `AgentBehaviorConfig` now carries
  `enabled_extensions` (parsed from the `agents.extension_config` jsonb, camelCase
  `enabledExtensions[{extensionId, enabled, config}]`), and the operator server's extension
  host intersects the server allowlist (`SMOOTH_EXTENSIONS_ALLOW`) with the per-agent enabled
  extension ids.

  Fail-closed for resolved agents: any agent that resolves to a config (exists in the agents
  DB) but enables no extensions loads ZERO extensions, even when the server allowlist is
  non-empty — extensions can intercept & mutate tool calls, so a public agent must never
  silently inherit one. Backward-compatible when no per-agent config resolves at all
  (bare/standalone operator): the server allowlist alone decides, unchanged. The Postgres
  resolver now keys "no per-agent config" off row existence (not `is_empty()`), so a
  found-but-blank agent is distinguishable from an unknown one; the `extension_config` column
  read degrades to `None` on a standalone deploy whose table predates the column (no migration
  ordering dependency).

## 1.21.0

### Minor Changes

- 85e5643: Rich Interactions — generalize the just-shipped identity-intake seam into an extensible structured-interaction framework (`docs/Architecture/Rich Interactions.md`). One generic wire surface serves every interaction kind: `interaction_required` / `interaction_invalid` events + the single `submit_interaction` resume verb (with `interactionId` echo so stale submits can't resolve newer parks); per-kind precision lives in `spec/interactions/<kind>.schema.json` and the per-kind raise tools. Adding a kind (date picker, choice chips, file upload, …) = one `InteractionKind` impl (server-side validator + conversational-fallback directive + raise-tool schema) + a spec entry + a widget card — no new events, no client-library release. `identity_intake` (capability `identity_form`) ships as the first kind through the framework. Supersedes 1.19.0's typed `identity_intake_*` events (removed — zero external consumers). TypeScript client: regenerated types and the generic `submitInteraction()` verb (replaces `submitIdentityIntake()`).

## 1.20.0

### Minor Changes

- af9ac05: Suggested quick replies: the Rust server's `eventual_response` now carries live `suggestedNextActions` instead of a hardcoded empty array. The runner appends a machine-parsed trailer contract (`<suggested_replies>["…"]</suggested_replies>`) to every turn's system prompt, suppresses the trailer from the live token stream, strips it from the persisted/final reply, and surfaces the parsed suggestions (capped at 4) on `TurnResult.suggested_next_actions` and the `eventual_response` payload. `runner::general_agent_response` now takes the suggestions slice. Rust server only; other language servers still emit an empty array (parity follow-up).

## 1.19.0

### Minor Changes

- 3a9d29e: Identity intake — a channel-normalized lead/identity capture primitive (`docs/Architecture/Identity Intake.md`). New protocol surface: `supports` client-capability declaration on `create_conversation_session`, `identity_intake_required` / `identity_intake_invalid` events, and the `submit_identity_intake` resume action (with server-side validation: required fields, email shape, E.164 phone normalization). Rust reference implementation: `request_identity_intake` / `submit_identity_intake` agent tools in `smooai-smooth-operator` (park-and-resume on form-capable sessions; validated conversational turn-by-turn fallback on text-only channels — both resume with the same structured payload), server wiring (pending-intake registry, session identity attach onto the OTP contact keys) in `smooai-smooth-operator-server`. TypeScript client: regenerated spec types, `supports` on `createConversationSession`, and the `submitIdentityIntake()` resume verb. Parity for the TS/Python/Go/.NET servers is tracked as follow-ups; the spec + conformance fixtures are the complete contract.

## 1.18.0

### Minor Changes

- 21016e5: SEP Phase 8 (spec + SDK + demo) — long-tail pi parity.

  **Spec.** `initialize.schema.json` registrations gain `hooks` (declared intercept
  hooks, so the host can skip the per-turn `context` hook) and `message_renderers`
  (declarative `tag` → render-block templates). New `RenderBlock` `$def` — the
  render-block DSL (`markdown`/`keyvalue`/`table`/`diff`/`progress`/`stack` + the
  interactive `widget` kind with keybindings, each with a `text` fallback) — plus
  `MessageRendererRegistration`. `ui/request` `set_widget` documents its widget as a
  render block (kept permissive since SEP carries no cross-file `$ref`s). New
  conformance fixtures: `event_bus_fanout` (`bus/event`), `event_widget_key`
  (`widget/key`), `registrations_phase8` (hooks + message renderer), and
  `render_block_widget`.

  **SDK.** `render.*` builders for the render-block DSL; `smooth.events`
  (`publish`/`on`) for the inter-extension bus; `smooth.registerMessageRenderer(tag,
template)`; `ctx.ui.setWidget` now takes a typed `RenderBlock`; the `context` +
  `before_agent_start` hooks and `widget/key` events are exercised end-to-end.
  `buildRegistrations` emits `hooks` + `message_renderers`. `createTestHost` records
  `bus/publish` (`busPublishes`) and services it. New `eventName` constants
  (`BUS_EVENT`, `WIDGET_KEY`) and `method.BUS_PUBLISH`.

  **Demo.** `snake` — pi's game ported to the render-block v2 widget DSL: `play`
  pushes an interactive `widget` block; each `widget/key` advances a pure game core
  and re-renders. Full-fidelity on web, reduced-fidelity (ASCII grid + score) on the
  TUI, identical keybinding DSL.

  **Docs + scaffold.** `PORTING.md` — the pi → SEP parity checklist (every pi
  `ExtensionAPI` member → equivalent, port delta, or documented N/A). New `provider`
  scaffold template in `create-smooth-extension` (registers a provider; builds and
  tests green with a canned response, marked where the real call goes).

## 1.17.0

### Minor Changes

- f370ae9: SEP — the .NET operator server (`dotnet/server`) now hosts extensions (ui/confirm producer).

  The C# server wires the engine `ExtensionHost` (from `SmooAI.SmoothOperator.Core` 1.4.0)
  into each `send_message` turn. With `SMOOTH_EXTENSIONS_ALLOW` set (a default-deny allowlist —
  the server has no interactive trust prompt), `ExtensionServerHost.BuildAsync` discovers
  `extension.toml` extensions, spawns them as JSON-RPC/ndjson subprocesses, and exposes their
  tools. Those tools join the turn's tool set so they flow through the SAME per-agent
  `enabled_tools` filtering + auth gate as native tools (dotted `<ext>.<tool>` names match
  `toolId`), and the host is torn down (subprocesses killed) at turn end.

  An extension's `ui/confirm` bridges onto the operator protocol's
  `write_confirmation_required`/`confirm_tool_action` frames via `ConfirmUiProvider` — parking
  on the same session-keyed `ConfirmationRegistry` the native write-tool HITL uses. Every other
  `ui/*` degrades headless. Only the `confirm` capability is advertised at handshake.

  Additive: with the allowlist empty (the default) no host is ever built, so behavior is
  byte-for-byte unchanged. Verified by an integration test that runs the spec's Node echo peer
  through a real server turn and asserts `enabled_tools` filtering drops an extension tool
  exactly like a native one.

## 1.16.0

### Minor Changes

- 49bd798: SEP — the TypeScript operator server now hosts extensions (`ui/*` producer),
  mirroring the Rust reference (`rust/smooth-operator-server/src/extensions.rs`).

  `typescript/server` wires the engine `ExtensionHost`
  (`@smooai/smooth-operator-core/extension`) into each turn: with
  `SMOOTH_EXTENSIONS_ALLOW` set (a default-deny, comma-separated trust allow-list)
  it discovers `extension.toml` extensions, spawns them as JSON-RPC/ndjson
  subprocesses, and registers their `<ext>.<tool>` tools into the turn's tool set
  BEFORE the per-agent `enabled_tools` filter — so an allow-list drops them exactly
  like a built-in (SMOODEV-590 parity). A `ConfirmUiProvider` bridges an
  extension's `ui/confirm` onto the existing `write_confirmation_required` /
  `confirm_tool_action` frames via the session-keyed `ConfirmationRegistry`; every
  other `ui/*` degrades headless (render-only → `{}`, select/input → `{cancelled}`).
  The host and its subprocesses are torn down at turn end. Unset
  `SMOOTH_EXTENSIONS_ALLOW` (the default) builds no host — behavior is unchanged.

## 1.15.1

### Patch Changes

- 35806b2: Go server: host SEP extensions in a turn + ui/confirm bridge (th-829d9f).

  Wires the engine's SEP `ExtensionHost` (new in smooth-operator-core) into the Go
  operator server's send_message turn:

  - **Default-deny discovery** — `SMOOTH_EXTENSIONS_ALLOW` (comma-separated names)
    is the trust decision; empty (the default) builds no host, so behavior is
    byte-for-byte unchanged. Allowlisted `extension.toml` extensions are discovered
    (`SMOOTH_EXTENSIONS_DIR` or the engine default) and spawned per turn.
  - **Tool composition** — an extension's tools (`<ext>.<tool>`) are folded into the
    turn's tool set before the SMOODEV-590 `enabled_tools` / authLevel filter, so
    they gate exactly like a built-in tool.
  - **ui/confirm bridge** — `confirmUIProvider` projects an extension's `ui/confirm`
    onto the existing `write_confirmation_required` / `confirm_tool_action` frames via
    the per-connection confirmation registry; other `ui/*` degrade headless.

  Covered by an end-to-end test that drives a scripted model calling an
  extension-registered tool through the real WS/dispatcher turn (echo peer via a
  self-re-exec of the test binary), asserting execution and `enabled_tools` filtering
  parity, plus default-deny. Race-clean.

## 1.15.0

### Minor Changes

- b88d39c: Python server: host SEP extensions in a turn (ui/\* producer) — pearl th-66251a.

  Wires the engine's `ExtensionHost` (ported to the Python core in smooth-operator-core#33) into the Python operator server, the Python sibling of the Rust reference server wiring (#159). A turn can now host `extension.toml` extensions: their tools reach the agent and their `ui/confirm` bridges onto the chat-native confirmation frame.

  - **Trust — default deny.** `SMOOTH_EXTENSIONS_ALLOW` (comma-separated names) IS the trust decision; empty/unset (the default) means no extension is ever spawned and the host is never built, so behavior is byte-for-byte unchanged. `SMOOTH_EXTENSIONS_DIR` overrides the discovery dir.
  - **Tools + `enabled_tools` parity.** An allowlisted extension's eager tools are added to the turn's tool set and flow through the SAME per-agent `enabled_tools` filter (`filter_tools`, by tool name) the built-ins get — so an allow-list drops an extension tool (`echo.say`) exactly like a built-in.
  - **`ui/confirm` → the confirmation frame.** `ConfirmUiProvider` (a `HostDelegate`) projects an extension's `ui/confirm` onto the existing `write_confirmation_required` / `confirm_tool_action` frames via the same session-keyed `ConfirmationRegistry` the native write HITL uses; every other `ui/*` degrades headless (interactive → `{cancelled}`, render-only → `{}`). Only the `confirm` capability is advertised at handshake.
  - **Teardown.** The per-turn host is shut down (subprocesses stopped, parked confirmation cleared) at turn end.

  New module `smooth_operator_server.extensions` (`build_extension_host`, `ConfirmUiProvider`, `parse_allowlist`), wired into `turn_runner.py`. Integration tests drive a real echo-peer extension through a live `send_message` turn (tool runs + result streams back) and assert `enabled_tools` filtering parity, plus the `ui/confirm` bridge unit tests.

## 1.14.0

### Minor Changes

- be6b62f: SEP — the Rust operator server now hosts extensions (`ui/*` producer).

  The reference operator server (`smooth-operator-server`) wires the engine
  `ExtensionHost` into each turn: with `SMOOTH_EXTENSIONS_ALLOW` set (a default-deny
  allowlist — the server has no interactive trust prompt), it discovers
  `extension.toml` extensions, spawns them as JSON-RPC/ndjson subprocesses, and
  attaches the host to the agent. An extension's tools land in the turn's
  `ToolRegistry` and flow through the same per-agent `enabled_tools` filtering +
  authLevel gating as built-ins (SMOODEV-590), and its hooks/events run in the
  agent loop.

  `ui/confirm` is projected onto the existing `write_confirmation_required` /
  `confirm_tool_action` HITL frames — the same out-of-band bridge the native
  write-tool `ConfirmationHook` uses, so a hosted extension's confirm prompt pauses
  and resumes the turn end-to-end. Every other `ui/*` degrades headless (only the
  `confirm` capability is advertised at handshake). Unconfigured (empty allowlist),
  no host is built and behavior is byte-for-byte unchanged.

  This is the first operator server to host extensions. The other four polyglot
  servers (TypeScript, Python, Go, .NET) have the agent-loop + HITL landing pad
  wired but their engine cores have no SEP `ExtensionHost` yet — porting it to each
  engine is tracked as follow-up work.

## 1.13.0

### Minor Changes

- 70bd271: SEP Phase 7 (spec + SDK + demo) — registerProvider: declarative providers, OAuth,
  proxied streaming, and set_model.

  **Spec.** New `provider.schema.json` covering `provider/complete` (params +
  result), `provider/delta`, and `provider/oauth_login`/`oauth_refresh` (params +
  credentials). `initialize`/`registry-update` registrations gain `providers`
  (`ProviderRegistration` + `ProviderModel`); `session/set_model` params gain
  optional `provider` + `thinking`; `capabilities_enabled` gains `providers`. New
  conformance fixtures for every provider shape (valid + `$invalid`), replayed by
  both the TypeScript schema conformance test and the Rust host's vendored copy.

  **SDK.** `smooth.registerProvider(defineProvider({ name, models, complete,
oauthLogin?, oauthRefresh? }))` — the extension owns the request/stream, emitting
  `ctx.delta(event)` chunks while streaming. `session.setModel(model, { provider,
thinking })` completes the Phase 4 session surface. `createTestHost` gains
  `complete()` (with `onDelta`), `oauthLogin()`, `oauthRefresh()`, and routes
  `provider/delta` by `request_id` — the in-process mirror of the engine's
  `ProviderStreams`.

  **Demo.** `corporate-proxy` registers a provider that proxies an OpenAI-compatible
  endpoint: it streams the upstream SSE back as `provider/delta` chunks, maps
  tool-call responses, and mediates OAuth (login prompt over `ui/input`, token
  exchange). Exercised end-to-end in `provider-path.test.ts` against a real mock
  upstream serving scripted SSE.

## 1.12.0

### Minor Changes

- 7a05f00: SEP Phase 6 (chat-widget) — render agent confirmation prompts as chat-native
  buttons.

  The embeddable chat widget now renders a `write_confirmation_required` HITL
  event as an inline Yes/No button prompt inside the assistant bubble instead of
  silently ignoring it. Clicking a button sends the `confirm_tool_action` resume
  frame and un-pauses the turn; the chosen answer sticks in the transcript. This
  is the chat-native projection of SEP `ui/confirm` (a hosted extension's confirm
  prompt maps onto the existing `write_confirmation_required` frame).

  `ConversationController` gains `answerPrompt(requestId, value)` and an optional
  client-options constructor arg (a transport seam for tests). `ChatMessage` gains
  an optional `prompt` field (`ChatPrompt`) carrying the buttons; the multi-option
  shape also backs a future `ui/select` chat frame.

## 1.11.4

### Patch Changes

- 0953584: SEP Phase 4 (spec + SDK) — commands, flags, shortcuts, and session actions.

  **Spec.** New `command-complete.schema.json` (argument autocomplete). `session.schema.json` now carries the dispatch `context` on every params object (the wire form of the command-tier + epoch guard the host enforces) and adds `send_user_message` (`deliver_as` steer/follow_up/next_turn). `initialize.schema.json` gains a `flags` delivery map on the params and a `shortcuts` list (+ `ShortcutRegistration`) on the registrations. New conformance fixtures for command/complete, session send_user_message/append_entry, shortcuts, and flag delivery; new `$invalid` cases proving `context` is required on a session action and `value` on a completion. The reference `echo.mjs` registers a command + shortcut and answers command/execute + command/complete.

  **SDK.** `smooth.registerCommand` (with an optional `complete` completer), `registerFlag` (+ `smooth.getFlag`), and `registerShortcut`. Command handlers receive a `CommandContext` bound to their command-tier context, exposing `session.sendMessage` / `sendUserMessage` / `appendEntry`, `ui`, `hasUI`, and `args`. `createTestHost` gains `runCommand`, `completeCommand`, and a `session/*` service that enforces the same command-tier guard the engine does (event-tier → -32003), recording every session call for assertions. `runConformance` now replays command/execute + command/complete.

  **Demo.** `plan-mode` — the flagship extension that exercises phases 2–4 together: a `--plan` flag and a `/plan` command toggle plan mode; a `tool_call` intercept blocks write/edit/apply_patch/bash while it is on; each toggle pushes a `set_widget` render block and persists an LLM-invisible `appendEntry`, so the state survives a hot reload (the flag re-seeds it, the transcript keeps the history).

## 1.11.3

### Patch Changes

- a36ee69: SEP Phase 3 (SDK + spec) — the `ui/request` surface.

  The extension SDK now exposes the capability-negotiated UI surface. An extension
  reads the host's declared `ui_capabilities` from the `initialize` handshake and
  gates on `smooth.hasUI(kind)` / `ctx.hasUI(kind)`; `ctx.ui` (and `smooth.ui`)
  speak `ui/request` back to the host: `select`/`confirm`/`input` return the user's
  answer (or `{ cancelled: true }`), and `notify`/`setStatus`/`setWidget`/`setTitle`
  push to the frontend. A headless or uncapable host rejects with `RpcError` code
  -32001 (NoUI). `createTestHost(ext, { onUiRequest })` scripts the host side; its
  default mimics a headless frontend.

  Ships the `todo` demo extension (pi's todo, ported): stateful list whose tools
  push a `keyvalue` `set_widget` render block and whose `clear` asks for `confirm`
  first — both `hasUI`-gated, so it degrades cleanly headless.

  Extends `spec/extension/conformance/fixtures.json` with the remaining `ui/request`
  kinds (input/notify/set_status/set_widget/set_title), select/input/cancelled
  results, and invalid cases (unknown kind, missing `options`/`message`, extra
  property).

## 1.11.2

### Patch Changes

- 1c8f26f: SEP Phase 2 (SDK + spec) — hooks + the observe event bus.

  `@smooai/smooth-extension-sdk` gains **hook handlers**: `smooth.on(name, handler)`
  now covers both observe events (return ignored) and intercept hooks (return a
  `HookResult` — `{ block, reason? }` to veto or `{ patch }` to rewrite the input).
  The extension answers the `hook` request by folding its own handlers in
  registration order (first `block` short-circuits; `patch`es shallow-merge and
  thread to the next), and the host chains the outcome across extensions. Hook
  names are kept out of the reported event `subscriptions`. `createTestHost` gains
  `callHook(hook, input)`; new `permission-gate` demo extension blocks dangerous
  `bash` commands via a fail-closed `tool_call` hook.

  `spec/extension`: the event schema gains an optional `seq` (per-connection
  monotonic sequence; absent on the out-of-band `events_lost` marker) with a
  `model_select → AgentEvent::ModelResolved` parity note, and fixtures add a
  seq-numbered event, the `events_lost` marker (drop-N → count), a
  `tool_execution_start` event, and the `tool_result` hook input + a result-shaped
  `modify` outcome. Rust and TypeScript conformance replays stay green.

## 1.11.1

### Patch Changes

- 940560b: Add the SEP TypeScript extension SDK — Phase 1 (the tool path).

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

## 1.11.0

### Minor Changes

- ec80d14: Add the SEP (Smooth Extension Protocol) spec — Phase 0.

  New `spec/extension/` tree: `envelope.md` (JSON-RPC 2.0 over ndjson framing, method
  catalog, error codes, context tiers, deferred WS binding), `methods/*.schema.json` (draft
  2020-12, snake*case: initialize, shutdown, ping, event, hook, tool/execute, tool/update,
  $/cancel, command/execute, registry/update, tools/set_active, session/*, exec/run,
  ui/request, kv/\_, bus/publish, log, plus the JSON-RPC frame envelope), and
  `conformance/fixtures.json` (43 valid + 6 invalid instances) with the dependency-free
  `echo.mjs` demo extension. A new `extension-conformance.test.ts` validates every fixture
  against its schema, mirroring the existing operator-protocol conformance harness. SEP is a
  sibling of the operator WebSocket protocol — it reuses the spec machinery, not the
  envelope.

## 1.10.4

### Patch Changes

- 00b2623: C# server: OTP / session-identity seam parity with the Rust reference (SMOODEV pearl th-8078dd).

  Brings the .NET reference server (`SmooAI.SmoothOperator.Server`) to behavioral parity with the Rust server's OTP / session-identity seam (PR #132), so a public agent's `end_user`-gated tools can offer a one-time-code identity flow while the server stays credential-free.

  - New host seam `IOtpService` (`SendOtpAsync(sessionId, contact) -> OtpDelivery`; `VerifyOtpAsync(sessionId, code) -> OtpVerifyOutcome.Verified | Invalid`) with the `OtpChannel` / `OtpContact` / `OtpDelivery` / `OtpError` value types. Registered via DI; absent ⇒ unchanged (the `end_user` gate fail-closed-refuses and no OTP is offered).
  - When a turn's auth gate refuses an `end_user` tool on an unverified session, an `IOtpService` is installed, and the session has a contact, the server emits `otp_verification_required`, calls `SendOtpAsync`, and emits `otp_sent` — before the terminal response. Admin refusals are never offered OTP.
  - New `verify_otp` action: a `Verified` outcome marks the session identity-verified (`otp_verified`); an `Invalid` outcome emits `otp_invalid` with the host's remaining-attempt count. Validation order mirrors Rust (requestId → sessionId → code → session-exists → service); no service installed ⇒ fail closed (`otp_invalid` / `NOT_FOUND`).
  - Per-conversation verified state is persisted in the session store and threaded into the auth gate via a store-backed `ISessionAuthenticator` default (replacing the hardcoded deny-all), so a verified caller's `end_user` tools run. The caller's email contact is captured at create-session time. Both are backed in the in-memory and Postgres stores with a shared contract test.

  The reference server does not park/auto-resume the original turn; the client re-sends its message after `otp_verified`. Event shapes validate against the same `spec/events/otp-*.schema.json`.

## 1.10.3

### Patch Changes

- f3ace72: Go server: OTP / session-identity seam parity for end-user tool auth (th-8078dd).

  Brings the Go reference server to parity with the Rust server's OTP / session-identity seam (PR #132). A public agent's `end_user`-gated tools can now offer a one-time-code identity flow, while the Go server stays credential-free — it never generates, delivers, or validates a code.

  - New `OtpService` seam (`SendOtp` / `VerifyOtp`) plus the `OtpContact`, `OtpDelivery`, `OtpChannel`, `OtpErrorCode`, and `OtpVerifyOutcome` value types, mirroring the existing resolver seams. Installed via `server.WithOtpService`; absent ⇒ unchanged fail-closed behavior (the gate refuses, no OTP offered).
  - The session's OTP-verified bit (`StoredSession.OtpVerified`, set by a successful `verify_otp`) is threaded into the auth gate so a verified caller's `end_user` tools run.
  - On an `end_user` refusal, with a service installed and a session contact captured at create-session time, the server emits `otp_verification_required`, calls `SendOtp`, and emits `otp_sent` (before the terminal `eventual_response`, matching the Rust ordering). `admin` refusals are never offered OTP.
  - New `verify_otp` action: validation order `requestId → sessionId → code → session-exists → no-service`; a correct code emits `otp_verified` and marks the session authenticated, a rejected code emits `otp_invalid` with the host's remaining attempts, and no installed service fails closed (`otp_invalid` / `NOT_FOUND`).

  Semantics match the Rust reference exactly. Exhaustive tests (seam types, verify_otp happy/invalid/no-service/unknown-session/missing-fields, offer-flow event order, admin-not-offered, verified-session-runs-tool); server events validate against the shared `spec/events/*` schemas.

## 1.10.2

### Patch Changes

- 8535264: Python server: OTP / session-identity seam parity with the Rust reference (SMOODEV pearl th-8078dd).

  Brings the Python operator server to behavioral parity with the Rust server's end-user OTP identity-verification seam (landed for Rust in #132). Like the reference, the Python server never generates, delivers, or validates a code — a new host seam, `OtpService` (`smooth_operator_server.otp`, with `OtpContact` / `OtpDelivery` / `OtpChannel` / `OtpError` / `OtpVerified` / `OtpInvalid`), owns generation, delivery, expiry, and attempt counting. Install one via `ServerState.otp_service` (or `FrameDispatcher(..., otp_service=...)`); absent (the default), behavior is unchanged — the `end_user` auth gate fail-closed-refuses and no OTP is offered.

  - When a turn's gate refuses an `end_user` tool on an unverified session and an `OtpService` is installed and the session has a contact (the caller's email, captured at create-session time), the server emits `otp_verification_required`, calls `send_otp`, and emits `otp_sent` — in that order, before the terminal `eventual_response`. An `admin` refusal is never OTP-remediable, so it is not offered.
  - A new `verify_otp` action validates a submitted code via `OtpService.verify_otp`: an `OtpVerified` outcome marks the session identity-verified (persisted on the session store) and emits `otp_verified`; an `OtpInvalid` outcome emits `otp_invalid` with the host's remaining-attempt count and optional machine-readable reason. Validation order mirrors Rust (requestId, sessionId, code required; unknown session → `SESSION_NOT_FOUND`; no service → fail closed `otp_invalid` / `NOT_FOUND`).
  - Per-session verified state is tracked on the session store and threaded into the tool auth gate as the resolved `session_authenticated` bit (the session's OTP-verified state OR'd with the existing `SessionAuthenticator` seam), so a verified caller's `end_user` tools run.

  The reference server does not park/auto-resume the original turn; the client re-sends after `otp_verified`. The four OTP event builders reproduce the shared conformance fixtures byte-for-byte; exhaustive tests cover verify happy/invalid/no-service/unknown-session/missing-field, the offer flow's emission order, admin-not-offered, no-contact/no-service/send-failure edges, and a verified session running the gated tool.

## 1.10.1

### Patch Changes

- 9352c87: TS server: OTP / session-identity seam parity with the Rust reference (pearl th-8078dd).

  Brings `typescript/server` to parity with the Rust server's end-user OTP / session-identity seam (#132). The native TS server can now offer a one-time-code identity-verification flow behind a public agent's `end_user` tool auth gate, without holding any credentials itself.

  - New host seam `OtpService` (`typescript/server/src/otp.ts`) with `sendOtp` / `verifyOtp`, mirroring the shape of the server's other pluggable seams (`AgentConfigResolver`, `SessionAuthenticator`). Installed via the `otpService` server option; absent → unchanged fail-closed behavior (the `end_user` gate refuses and no OTP is offered). The server never generates, delivers, or validates a code — the host owns generation, delivery, expiry, and attempt counting.
  - When a turn's auth gate refuses an `end_user` tool on an unverified session and an `OtpService` is installed and the session has a contact, the server emits `otp_verification_required`, calls `sendOtp`, and emits `otp_sent` — in that order, before the terminal `eventual_response`.
  - New `verify_otp` action validates a submitted code: a `verified` outcome marks the session identity-verified and emits `otp_verified`; a non-verified outcome emits `otp_invalid` with the host's remaining-attempt count. No service installed → fail closed (`otp_invalid` / `NOT_FOUND`).
  - The session's OTP-verified bit is tracked on the session store (`contactEmail` captured at create-session time, `otpVerified` set by `verify_otp`) and threaded into the `end_user` auth gate, so a verified caller's gated tools run on the re-sent message. Admin refusals are never offered OTP.

  The server does not park/auto-resume the original turn; the client re-sends its message after `otp_verified`. Four protocol event builders + the shared `spec/conformance/fixtures.json` OTP fixtures + exhaustive tests (verify_otp happy/invalid/no-service/unknown-session/missing-fields, offer-flow event order, admin-not-offered, verified-session tool execution) added.

## 1.10.0

### Minor Changes

- 86d9e4f: Server-side OTP / session-identity seam so hosts can wire end-user tool auth (SMOODEV pearl th-8e8a89).

  The Rust reference server can now offer a one-time-code identity-verification flow behind a public agent's `end_user` tool auth gate, without holding any credentials itself. A new host seam, `OtpService` (`smooth_operator::otp`), owns code generation, delivery, expiry, and attempt counting; the reference server only orchestrates the wire flow around it. Install one via `AppState::with_otp_service`; absent, behavior is unchanged (the `end_user` gate fail-closed-refuses and no OTP is offered).

  - When a turn's auth gate refuses an `end_user` tool on an unverified session and an `OtpService` is installed and the session has a contact, the server emits `otp_verification_required`, calls `send_otp`, and emits `otp_sent`.
  - A new `verify_otp` action validates a submitted code via `OtpService::verify_otp`: a `Verified` outcome marks the session identity-verified and emits `otp_verified`; an `Invalid` outcome emits `otp_invalid` with the host's remaining-attempt count. With no service installed, verification fails closed (`otp_invalid` / `NOT_FOUND`).
  - Per-session verified state is tracked in session metadata and threaded into the auth gate as the real `session_authenticated` bit (previously hardcoded `false`), so a verified caller's `end_user` tools run.

  The reference server does not park/auto-resume the original turn; the client re-sends its message after `otp_verified`. Rust-only for now (mirrors how per-agent config landed as separate per-language PRs); parity in the Python/TS/Go/.NET servers is follow-up work.

## 1.9.0

### Minor Changes

- 0e29a9b: Per-agent behavior config: honor `instructions` + run `conversation_workflow` (SMOODEV-590).

  The reference server resolved a turn's system prompt from **per-org** settings, so every agent in an org spoke with the same voice and `conversation_workflow` was never applied — a public chat agent ignored its own persona and behaved as the generic customer-support bot.

  Config-delivery seam (matches the sibling Python/TS/C#/Go lanes): `AgentConfigResolver::resolve(agent_id)` — the ws protocol's `create_conversation_session` carries only an agent UUID, so config is resolved **server-side by id**. Default `StaticAgentConfigResolver` (empty ⇒ no-op, behavior unchanged); a `PgAgentConfigResolver` reads the monorepo `agents` table on the adapter's existing pool. The runner now:

  - uses the agent's `instructions` (+ `personality.persona`) as the system prompt, overriding the org default;
  - injects the agent's `greeting` into the prompt only on the first turn of a conversation;
  - restricts the turn's tools to `tool_config.enabledTools` (`enabled == true` entries by snake_case `toolId`; empty/absent ⇒ full set; unknown ids ignored), and delivers each entry's `config` to the tool via `ToolProviderContext`;
  - enforces per-tool `authLevel` at execution against the agent's `visibility` (a `ToolHook` gate: admin blocked on public agents; internal auto-satisfies; end_user on public requires an identity-verified session, fail-closed — the OTP flow is a host seam);
  - when a `conversation_workflow` is set, injects the current step's intent/criteria and, after each turn, runs a cheap failure-tolerant judge on the configurable `judge_model` (haiku-tier default) to advance the step; the step id is tracked per session.

  Per-agent isolation, malformed-jsonb tolerance (degrade to org default, never crash the turn), judge-failure tolerance (stay on the current step), and the authLevel branches (admin/end_user/internal, authed vs not) are covered by unit + integration tests.

- 9db9007: C# server: honor per-agent config + implement conversation workflows. An agent's `instructions.prompt` now drives its system prompt (overriding the org/default persona), so agents in the same org behave as themselves rather than a generic customer-support persona. `conversation_workflow` (goal + intent/criteria steps) is now implemented as a stepped, judge-advanced guided-agency flow: the current step's intent + criteria are rendered into the system prompt, and a cheap post-turn judge decides whether the step's criteria were met to advance (explicit `next` or sequential), with the current step id persisted per conversation. Per-agent `greeting` is woven into the agent's first reply only (first-turn prompt seed), and `tool_config.enabledTools` restricts the server's tool set to the agent's enabled snake_case toolIds per turn (empty/absent ⇒ the full set, unchanged). At tool-execution time each entry's `authLevel` is enforced (admin blocked on public agents; `end_user` needs a verified session via the new `ISessionAuthenticator` seam, default fail-closed; internal agents auto-satisfied; only tools declaring `supportsAuthRequirement` are gated) and its per-tool `config` is delivered to the executing tool. The workflow judge model is the uniform `judgeModel` option. Per-agent config reaches the server through a new `IAgentConfigResolver` DI seam (`ResolveAsync(agentId)`, default dict-backed `StaticAgentConfigResolver`) — `create_conversation_session` carries only an agent UUID, so config is resolved server-side per turn from the session's agent (mirroring the TS / Python lanes' `AgentConfigResolver`). jsonb parsing is tolerant (malformed config degrades to the default persona, never crashes a session) and the judge is failure-tolerant (any error keeps the conversation on the current step). Mirrors the Rust server change and the monorepo SMOODEV-590 behavior.
- a69a799: C# server local flavor: serve a prebuilt SPA same-origin from `SMOOTH_WEB_DIR` with the local token injected into `index.html` as `window.__SMOOTH_TOKEN__`, a `SMOOTH_LOCAL_TOKEN` → `LocalTokenVerifier` for same-origin `/ws` auth, and `SMOOTH_PERSONA` to set the agent's system prompt. Lets the .NET server be a drop-in "Big Smooth" backend behind the shared smooth-web Presence UI (validated end-to-end: SPA + WS + streamed persona reply).
- a6fab4a: Go server: honor per-agent config + implement conversation workflows (SMOODEV-590).

  Agents served by the Go operator now respect their own per-agent config instead of all sharing one generic org persona. A new `AgentConfigResolver` seam resolves a session's `agentId` into its `AgentConfig` (instructions, `Workflow`, greeting, personality, tool allow-list); resolution is server-side because the `create_conversation_session` payload carries only an `agentId`. An un-configured agent (no resolver, or resolver returns nil) falls back to the server/org default prompt + full tool set, so existing behavior is unchanged. Wire one in via `server.WithAgentConfigResolver`.

  `conversationWorkflow` is implemented as a stepped, judge-advanced guided-agency flow: the current step's intent + criteria are rendered into the system prompt (`<ConversationWorkflow>` block), and after each turn a cheap failure-tolerant judge LLM call decides whether the criteria were met and advances the pointer (following `next` or array order), tracked as `CurrentStepID` on the session. Malformed config degrades to the default flow and never crashes a session. Mirrors the TS/Python server siblings and the Rust reference's `agent-config-instructions-workflow` design.

- ebd2ad2: Python server: honor per-agent config + implement conversation workflows (SMOODEV-590).

  Agents served by the Python server previously ignored their per-agent config and always used the generic server-wide "customer support agent" persona. Now:

  - **Per-agent `instructions`** drive the system prompt for that agent's conversations, overriding the server-wide default (falling back to it when unset). Per-agent `personality` and first-turn `greeting` are plumbed into the prompt; `tool_config.enabledTools` (`[{ toolId, enabled, authLevel, config }]`, the monorepo `AgentToolConfig` shape) is a tool allow-list restricting the agent's turns to the `enabled=true` tools by `toolId` (empty/absent → full set; unknown toolIds ignored), matching the Go/TS lanes. Per-tool `authLevel` is enforced at execution against the agent's `visibility` and a `SessionAuthenticator` seam (admin blocked on public agents; internal auto-satisfies; end_user on public requires identity verification, fail-closed), and each entry's `config` is delivered to the tool at execution. The post-turn judge model is a `judge_model` server option (haiku-tier default).
  - **`conversation_workflow`** is implemented as a stepped, judge-advanced guided flow: the current step's intent + criteria are rendered into the system prompt, and a cheap post-turn judge call decides whether the criteria were met and advances to the next step (explicit `next` → sequential → terminal). The current step id is tracked per conversation.

  Config parsing is tolerant — a malformed workflow or config degrades to the server default and never crashes a session. The judge is failure-tolerant — any judge error leaves the conversation on the current step. Delivery seam: `ServerState.agent_config_resolver` (`AgentConfigResolver.resolve(agentId)`, default dict-backed `StaticAgentConfigResolver`) is resolved per turn from the session's agent — the ws protocol carries only an agent UUID, so config is looked up server-side. Empty resolver → behavior unchanged. Mirrors the Rust reference PR.

## 1.8.0

### Minor Changes

- 023c531: feat(auth): JWKS-based JWT verification (ES256 + any algorithm, with rotation) for `smoo`/`jwt` modes

  The auth verifier could only validate tokens against a **static RS256 PEM**
  (`AUTH_JWT_RS256_PUBLIC_KEY`). SmooAI's `auth.smoo.ai` (the `smoo` issuer) signs
  dashboard tokens with **ES256** (`/.well-known/jwks.json` → `alg: ES256, kty: EC`),
  so every real SmooAI token was rejected — blocking `AUTH_MODE=smoo` for the SmooAI
  K8s flavor.

  This adds a JWKS-backed verification path (additive, behavior-preserving):

  - New optional `AUTH_JWT_JWKS_URL`, and auto-derivation of
    `{AUTH_JWT_ISSUER}/.well-known/jwks.json` when an issuer is set and no static
    key is given.
  - Keys are fetched, **cached** (TTL) and **rotation-aware** (refresh-on-unknown-`kid`),
    selected per-token by `kid`, and validated with the key's algorithm via
    `DecodingKey::from_jwk` — so **any** advertised JWS algorithm works
    (ES256/ES384/RS256/PS256/EdDSA/…), not just RS256.
  - Wired into both `SmooIdentityVerifier` (the `smoo` path) and `JwtVerifier`
    (BYO), so any OIDC issuer works. `AuthVerifier::verify` stays **synchronous**
    (the keyset is read from cache; the network fetch is off the hot path).

  Key-source precedence (`jwt`/`smoo`): static `AUTH_JWT_RS256_PUBLIC_KEY` →
  static `AUTH_JWT_HS256_SECRET` → JWKS (`AUTH_JWT_JWKS_URL`, else issuer-derived).
  The static-RS256/HS256 paths are unchanged. With this, `AUTH_MODE=smoo` needs
  only `AUTH_JWT_ISSUER` (+ optional audience) — no static public key.

## 1.7.1

### Patch Changes

- 86dd6f8: local flavor: serve the canonical `@smooai/chat-widget` (Aurora Glass) bundle

  The local-flavor server now vendors and serves the published **`@smooai/chat-widget`**
  (Aurora Glass) standalone bundle instead of a parallel copy of the widget. One canonical
  public widget, consumed — not two. Same `<smooth-agent-chat>` element + `endpoint`/`agent-id`
  attributes, so it's a drop-in for the host page.

## 1.7.0

### Minor Changes

- 1d9c60e: feat: thread `organization_id` into `AccessContext` for per-turn knowledge scoping

  `StorageAdapter::knowledge_for_access(&self, access)` carried only `user_id` +
  `groups` — no org — so a multi-tenant relational backend (SmooAI) could not scope
  RAG to the turn's organization and was forced to a single static org. This was the
  last multi-tenant gap on the knowledge path.

  `AccessContext` now carries an additive `organization_id: Option<String>`
  (default `None`, set via the new `with_organization_id(...)` builder). The
  authenticated-principal path (`Principal::access_context`) stamps the principal's
  org automatically; the reference server / lambda send-message paths fall back to
  the turn's **session** org (every session carries `organization_id` since the
  create-session path derives it) when the requester has no org of its own. The org
  is then **available** to a host adapter's `knowledge_for_access` so it can scope
  retrieval to the right tenant.

  The operator's built-in single-tenant ACL ignores the org (org isolation already
  happens upstream), so this is behavior-preserving for the reference/local flavor.
  The Postgres knowledge adapter additionally uses the context's org — when present
  — to **override** its construction-time org as a cheap SQL pre-filter
  (`organization_id = $1`), so one adapter instance can serve per-turn tenants
  instead of being pinned to a single static org; an org-less context leaves the
  construction-time org unchanged.

## 1.6.0

### Minor Changes

- bdbf868: feat(server): derive org + agent from auth in `create_conversation_session`

  `handle_create_session` no longer hard-codes the seed org. It now derives the
  session's `organization_id` from the authenticated request, in priority order:

  1. the agent's widget-auth policy `organization_id` (widget visitors authenticate
     via origin + `authContext`, not a JWT, so their org rides on the agent policy —
     new optional `AgentWidgetAuth.organization_id` field),
  2. the connection's authenticated JWT principal org (dashboard / authed clients —
     the principal's `org_id` is now threaded from the `/ws` handshake through to the
     handler instead of being dropped at `AccessContext` reduction),
  3. the server's seed org as a behavior-preserving fallback for the no-auth/local
     flavor.

  The agent id continues to come from the inbound `agentId` payload. The same
  JWT-org-then-configured-org derivation is applied to the lambda dispatch
  create-session path. All existing in-memory/seed flows are unchanged.

## 1.5.0

### Minor Changes

- f2ecef9: Add `organizationId` to the `Session` domain type so org-scoping is uniform across every core domain type (`Conversation`, `Participant`, and `Message` already carry it). Storage backends can now write the session's org directly instead of re-deriving it from the conversation. The built-in Postgres adapter gains an `organization_id` column (additive, `DEFAULT ''`) on `conversation_sessions` plus an org index; the in-memory and DynamoDB adapters thread the new field through automatically; server/runner create-session paths populate it from the conversation/turn org already in scope.

## 1.4.0

### Minor Changes

- 45fd77e: Thread the turn's `conversation_id` and resolved per-org `gateway_key` into `ToolProviderContext`.

  A host's injected `ToolProvider` now receives the conversation the turn runs in and the LLM-gateway key that turn was billed/scoped to — alongside the existing `org_id` + `access`. This lets SmooAI's conversation-persisting tools correlate to the right conversation (instead of degrading to a no-op on an empty conversation id) and lets agent-brain's `knowledge_search` obtain the gateway key.

  Purely additive and behavior-preserving: both new fields are `Option`, default to `None` via `ToolProviderContext::new`, and existing `ToolProvider` impls that ignore them are unaffected. New builders `with_conversation_id` / `with_gateway_key` set them; the runner populates both from the turn it already has in hand.

## 1.3.0

### Minor Changes

- 12d348a: Add two host provider-injection seams to the chat runner so a deployment flavor can run a turn with its OWN tools and persona without forking the runner:

  - **Custom tool injection** — a new `ToolProvider` trait (`tools_for(&ToolProviderContext) -> Vec<Arc<dyn Tool>>`) plus `AppState::with_tools(provider)`. When installed, the runner merges the provider's per-turn tools into the turn's `ToolRegistry` alongside the built-ins; the `ToolProviderContext` carries the turn's `org_id` + `AccessContext` so a host can return per-org tools. No provider ⇒ the registry is exactly today's built-ins.
  - **Per-org agent persona** — an optional `AgentSettings.persona: Option<String>`; the runner uses the resolved persona as the turn's system prompt when present, else falls back to the existing const `KNOWLEDGE_CHAT_SYSTEM_PROMPT`. No persona ⇒ identical prompt to today.

  Both seams are behavior-preserving by default — the local/default flavor is unaffected.

- ab1aa9d: feat(server): `confirm_tool_action` — write-confirmation human-in-the-loop pause/resume

  The reference WebSocket server can now gate write tools behind human approval.
  When an agent turn calls a tool whose name matches `SMOOTH_AGENT_CONFIRM_TOOLS`
  (comma-separated substrings), the turn **parks** and emits a
  `write_confirmation_required` event (matching
  `spec/events/write-confirmation-required.schema.json`) carrying
  `{ toolId, actionDescription }`. The client resumes it by sending
  `confirm_tool_action` (`{ sessionId, requestId, approved }`, per
  `spec/actions/confirm-tool-action.schema.json`): on `approved: true` the parked
  tool executes; on `false` it is skipped with a rejection result the model sees,
  and the turn still completes.

  Built entirely on the existing smooth-operator-core human-gate primitive
  (`ConfirmationHook` + `human_channel()` + `AgentConfig::with_human_channel`) —
  **no core change required**. The server wires the hook's `HumanRequest` stream to
  a WS event and bridges an inbound `confirm_tool_action` back to the hook's
  `HumanResponse`, keyed by session. The `send_message` turn now runs in a spawned
  task so the socket reader stays free to receive the confirmation on the same
  connection (the turn would otherwise deadlock awaiting a frame it is blocking).

  With `SMOOTH_AGENT_CONFIRM_TOOLS` unset (the default), no `ConfirmationHook` is
  installed, no tool ever parks, and behavior is byte-for-byte unchanged. The
  local/default flavor is unaffected.

- feec0b5: Add a per-org LLM gateway-key resolution seam so a multi-tenant flavor can
  bill/scope each org's turns to its own gateway key (e.g. a per-tenant LiteLLM
  virtual key), while the local/default flavor keeps using the single environment
  key.

  - New `GatewayKeyResolver` trait (`smooth_operator::gateway_key`) — the public,
    contributable hook: `async fn resolve(&self, org_id: &str) -> Option<String>`.
  - Default `EnvGatewayKeyResolver` returns the single `SMOOAI_GATEWAY_KEY` for
    every org, so behavior is unchanged unless a host injects a per-org resolver.
  - `resolve_gateway_key(resolver, org_id, env_key)` helper centralizes the
    resolve-then-fall-back-to-env contract used by the per-turn LLM-config build.
  - The server's `AppState` holds an `Arc<dyn GatewayKeyResolver>` (default =
    `EnvGatewayKeyResolver`) with a `with_gateway_key_resolver(...)` builder for
    injection. `send_message` resolves the turn's `org_id` from its conversation,
    resolves the key, and falls back to the env key when the resolver returns
    `None`.

  Behavior-preserving by default: with no resolver injected, every turn uses the
  env key exactly as before. No SmooAI/DB specifics live in the shared code — only
  the trait and the env default; a host injects its own per-org key store.

- 45be211: Add a `get_conversation_messages` WebSocket action to `smooth-operator-server`. Returns paginated message history for a session's conversation (`{ conversationId, messages, nextCursor, hasMore }`), wrapping the existing `StorageAdapter::list_messages_by_conversation` (the same call the admin API + turn runner use). Optional `limit` (default 50) + opaque `cursor`, newest-first. Completes wire-compat for chat clients that page history over the socket (previously only `/admin` exposed it).
- cf6fab4: feat(server): graceful SIGTERM/ctrl_c drain of WebSocket connections.

  The reference WebSocket server (`smooth-operator-server`) now drains in-flight
  turns on shutdown instead of being killed mid-flight. Previously `run()` did a
  plain `axum::serve(listener, app).await` with no `with_graceful_shutdown`, so on
  a Kubernetes pod termination (scale-down / rollout) the process was killed while
  turns were in progress — in-flight WebSocket turns dropped and connections never
  `detach`ed from the `Backplane`, leaving stale registry entries in Valkey/NATS.

  A single shared `tokio_util::sync::CancellationToken` is now threaded through
  `AppState` (`shutdown`, defaulted to a fresh never-cancelled token in
  `AppState::new`, plus a `with_shutdown` builder). Each per-connection reader loop
  `select!`s on that token (`biased`, shutdown wins ties) with the inbound-frame
  read — and keeps `handle_frame(...).await` inside the frame arm so a turn already
  in flight finishes before the next shutdown check. After the loop the existing
  `backplane.detach(...)` runs, so the connection always leaves the registry clean.
  The serve loop (`run`) wires `axum::serve(...).with_graceful_shutdown(...)` to
  SIGTERM (k8s) or ctrl_c (interactive), cancelling the token to fan the drain out
  to every connection within the chart's `terminationGracePeriodSeconds` window.

### Patch Changes

- 7545ea8: Add an unauthenticated `GET /health` HTTP route to `smooth-operator-server`. A WebSocket `/ws` upgrade can't answer a plain GET healthcheck, so HTTP load balancers (AWS ALB, nginx ingress) had nothing to probe; `GET /health` now returns `200 OK`, dependency-free (no storage/LLM touch). Enables HTTP health checks for the K8s deployment flavor.

## 1.2.0

### Minor Changes

- 5971864: Phase 4: streaming turn execution across the Python, TypeScript, and Go cores (C#
  already streams via MEAI's `RunStreamingAsync`). A new streaming run method alongside
  the existing `run()` — TS `runStream` (`AsyncGenerator<StreamEvent>`), Python
  `run_stream` (`AsyncIterator[StreamEvent]`), Go `RunStream` (returns a `*Stream` whose
  `Events()` channel carries `StreamEvent`s and whose `Err()` reports a mid-turn model
  error) — drives the SAME agentic loop (system/knowledge/memory build, compaction, cost
  tracking, budget early-stop, deferred tools, clearance + human-gate, checkpoint/thread
  persistence) but calls the model in STREAMING mode and yields incremental events: a
  `text` event per content delta, a `tool_call` event per requested call (before
  dispatch), a `tool_result` event per finished tool (in original call order even under
  `parallelToolCalls`), and exactly one terminal `done` event carrying the same
  `AgentRunResponse` `run()` would return. The provider seam gains an OpenAI-style
  streaming call (`createStream` / `create(..., stream=True)` / `ChatStream`) that
  accumulates content + `tool_calls` deltas by index into a full assistant message, so
  the rest of the loop is unchanged; usage is read from the final chunk for cost/budget.
  The reusable mock LLM providers replay their FIFO script as chunked deltas (text split
  into pieces, tool-call arguments split across two chunks). Retry-with-backoff is
  intentionally not applied to streaming (re-running would re-emit chunks), mirroring C#.

## 1.1.0

### Minor Changes

- a89045d: Phase 4: concurrent (parallel) tool-call execution across the Python, TypeScript, Go,
  and C# cores. A new opt-in `parallelToolCalls` option (Python `parallel_tool_calls`,
  Go/C# `ParallelToolCalls`), default false, dispatches an assistant turn's tool calls
  concurrently (`asyncio.gather` / `Promise.all` / goroutines + `sync.WaitGroup` /
  `Task.WhenAll`) when there are two or more. The tool-result messages are still appended
  in the original tool-call order, so the transcript stays deterministic regardless of
  completion order; a failing or human-denied tool keeps its error result in its correct
  position. With the flag off (the default) — or for single-tool-call turns — dispatch is
  unchanged from today's sequential behavior. Per-tool semantics (clearance, human-gate
  approval, tool_search promotion, JSON-arg parsing) are untouched.

## 1.0.0

### Major Changes

- 6f6f622: Unified 1.0.0 polyglot publish — all five language implementations now ship from one changeset at one shared version via the existing lockstep release.

  - **Rust** reclaims the crate name `smooai-smooth-operator` (the predecessor standalone engine 0.13.x is superseded by `smooai-smooth-operator-core`) and publishes the full set: the reference lib plus 7 library crates (`-ingestion`, the `-adapter-*` storage/backplane adapters, and `-server`) to crates.io.
  - **Python** distributions are renamed to `smooai-smooth-operator` and `smooai-smooth-operator-core` (PyPI), keeping the `smooth_operator` / `smooth_operator_core` import packages unchanged.
  - **Go** is published by tag `go/v1.0.0` (subdir module `github.com/SmooAI/smooth-operator/go`).
  - **npm** (`@smooai/smooth-operator`) and **NuGet** (`SmooAI.SmoothOperator.Core`) continue as before.

  One changeset → one shared version → npm + NuGet + crates.io + PyPI + Go tag, all stamped by `scripts/sync-versions.mjs`.

## 0.9.0

### Minor Changes

- 08f1780: Phase 2: human-in-the-loop approval (HumanGate) across the Python, TypeScript, and
  Go cores, at parity with the C# reference. The agent consults an optional approval
  gate before running any tool flagged by a `requires_approval` predicate; a denial is
  fed back to the model as the tool result (the tool never runs) and an approval lets
  it execute normally. With no gate configured, behavior is unchanged.

## 0.8.0

### Minor Changes

- a8bfb62: HTTP-backed widget auth (SMOODEV-1890): `HttpWidgetAuth`, a generic `WidgetAuthProvider` that resolves each agent's embed policy (`allowed_origins` + `public_key`) by GETting `{base_url}/{agentId}` from a host policy service, with TTL caching. Response handling fails safe: 2xx caches the policy, 404 caches a no-policy result (denied under `WIDGET_AUTH_STRICT`), and 5xx/network/malformed responses return `None` without caching so the next connect retries. The server now installs it from env — set `WIDGET_AUTH_URL` (plus optional `WIDGET_AUTH_BEARER` / `WIDGET_AUTH_TTL_SECS`) to enforce embeddable-widget auth against a host's policy service with no custom binary; unset leaves the permissive default. This is the reusable mechanism a host backs with its own agent store (SmooAI points it at an api-prime route).
- bc901d7: Persistent + semantic agent memory (SMOODEV-1470, parity gap Phase 3): `PgMemory`, a pgvector-backed implementation of the core `Memory` trait in the `adapters/postgres` crate. Before this the only `Memory` backend was the core `InMemoryMemory` (a `Vec` behind a `Mutex`, keyword recall, lost on restart). `PgMemory` gives the general agent cross-thread user memory that survives restarts and recalls by semantic similarity — the Rust equivalent of the TS `store`/`store_vectors` namespaced by `['memories', orgId, userId]`.

  Each `PgMemory` instance is bound to one `(organization_id, user_id)` namespace at construction (built via `PostgresAdapter::memory(org, user)`; `user_id = None` for org-wide memory), mirroring how `PgKnowledgeBase` binds an org — the core `Memory::recall(query, limit)` signature carries no scoping, so scoping is threaded through the constructor. `store` embeds the entry content and upserts a row in a new `memories` table (`embedding vector(N)` matching the active `Embedder` dim, HNSW cosine index, namespaced by `(organization_id, user_id)`); `recall` embeds the query and returns the namespace's top-K by pgvector cosine distance with `relevance` set to the cosine similarity; `forget` deletes within the namespace. Embedding goes through the shared `Embedder` seam (DeterministicEmbedder offline, GatewayEmbedder live), so memory and knowledge vectors share column width and hashing. Covered by a testcontainers integration test (semantic recall, org/user namespace isolation, namespace-scoped forget, empty recall) that skips cleanly when Docker is unavailable. No change to the core `Memory` trait was required.

## 0.7.0

### Minor Changes

- ed12900: Realtime publish endpoint (SMOODEV-1893): `POST /admin/publish` lets non-AI publishers — job status, ingestion progress, notifications, billing — push an event to a backplane target over the WebSocket fleet without going through an agent turn. Body is `{ target: { type: session|user|org|agent|connection, id }, event }`; it calls `Backplane::publish`, so with a distributed backplane the event fans out across pods. Admin-gated (RBAC role 2); the response reports local deliveries on the serving pod (cross-pod deliveries happen but aren't counted). Targets are opaque ids matched against the connection registry — tenant id-namespacing is a host concern, documented on the handler.

## 0.6.0

### Minor Changes

- e9fa854: Distributed Backplane backends (SMOODEV-1892): `RedisBackplane` and `NatsBackplane` — the horizontal scale-out seam. Both implement the `Backplane` trait by wrapping a per-pod `InMemoryBackplane` for local registry + delivery and adding a pub/sub bus (Redis/Valkey channel or NATS subject) for cross-pod fan-out: `publish(Target, event)` delivers to local sockets immediately, then broadcasts a `BackplaneEnvelope` so every other pod re-resolves the target against its own registry and delivers to its sockets (the origin pod skips its own echo). This makes the same `publish` call reach a socket on any replica — required to run the WS service with >1 pod, and the cross-pod path for non-AI publishers. Selected at runtime via `SMOOTH_AGENT_BACKPLANE` (`memory` | `redis`/`valkey` | `nats`) + `SMOOTH_AGENT_BACKPLANE_URL`; default stays single-process in-memory. `Target` is now `Serialize`/`Deserialize` and a shared `BackplaneEnvelope` is exposed so a host's own transport adapter can speak the same wire format. New crates: `adapters/backplane-redis`, `adapters/backplane-nats` (cross-pod fan-out proven end-to-end over real Redis + NATS via testcontainers).

## 0.5.0

### Minor Changes

- e6d9dbe: Connection backplane (SMOODEV-1891): a pluggable `Backplane` trait + default `InMemoryBackplane` in the OSS server — the scale-out + event-delivery seam. Each connection's outbound sink is attached on connect and associated with its session/agent; `publish(Target, event)` delivers to every connection for a target. This is the foundation for running >1 replica (a Redis/NATS impl makes delivery cross-pod) and the plug point for non-AI realtime: any service can `publish(Target::Session(...), event)` and reach the connected client over WebSocket. Wired into `AppState` (`with_backplane`) + the connection lifecycle. Runtime-agnostic (the sink is a closure, no tokio dep added to the lib).

## 0.4.0

### Minor Changes

- 715f79c: Embeddable-widget auth (SMOODEV-1878): a pluggable `WidgetAuthProvider` hook in the Rust server that enforces a per-agent **origin allowlist** + public-key **`authContext`** (HMAC-SHA256, replay-protected) for `<smooth-agent-chat>` connections. The `Origin` header is captured at the WebSocket handshake and validated at `create_conversation_session`; hosts plug in a concrete provider (backed by their agent store) while the bundled `PermissiveWidgetAuth` leaves a standalone OSS server unaffected. `WIDGET_AUTH_STRICT=1` fails closed on unknown agents.

## 0.3.0

### Minor Changes

- 0933942: C# server (`SmooAI.SmoothOperator.Server`) + engine hardening, at Rust parity.

  Server (new):

  - Durable Postgres adapters: ACL knowledge store (ACL filtered in SQL via `acl_groups && groups`, leak contract on both in-memory and Postgres backends), session store, and checkpoint store — agent state, sessions, and ACL-scoped knowledge all survive a restart.
  - `GatewayEmbedder` for real semantic retrieval (deterministic fallback when no gateway key).
  - Reranker: opt-in post-retrieval reorder (`SMOOTH_AGENT_RERANK=gateway|lexical|off`) — engine `IReranker`/`NoopReranker`/`LexicalReranker` + server `GatewayReranker` + `RerankSelection`, wired through the turn; fails soft if the reranker errors.
  - Auth-gated `/admin` API: `/admin/health`, `/admin/me`, `/admin/connectors`, and `POST /admin/reindex` (re-ingest without a restart); fail-closed Bearer auth.
  - Tool `stream_chunk`s: tool call/result surfaced over the WebSocket protocol.
  - Deployable host (`SmooAI.SmoothOperator.Server.Host`) + Dockerfile: wires gateway model, storage, JWT/trusted/none auth, and startup GitHub ingestion.

  Engine (`SmooAI.SmoothOperator.Core`):

  - `IReranker` + `NoopReranker` + `LexicalReranker` + `Rerankers.ApplyOptionalAsync`.
  - `RunStreamingAsync` now yields the tool-result update so tool results surface in the stream.

  Robustness fixes:

  - Chunker no longer infinite-loops on long non-whitespace runs (minified code / base64 / long URLs).
  - The dispatcher emits a clean error and keeps the connection alive on any handler exception (was dropping the socket silently).
  - Postgres checkpoint store preserves tool-call/result content (was serializing text only).
  - GitHub connector fails loud on a truncated tree instead of silently indexing a partial repo.
