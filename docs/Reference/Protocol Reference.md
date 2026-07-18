# The smooth-operator protocol

A **schema-driven WebSocket protocol**. It is the single contract between any client and any smooth-operator service, in any language. It is lifted from the smooai monorepo's `@smooai/realtime` package and made language-neutral so the JSON Schemas in [`spec/`](../../spec) generate native types for TypeScript, Go, C#/.NET, and Python.

> Why protocol-first: `.NET` is a first-class target and the agent core is async + streaming-heavy. Rather than bet streaming FFI codegen on immature .NET/Go generators, the protocol is the spine — each language ships an idiomatic native client. In-process FFI is an optimization, not a requirement.

## Transport

- **AWS**: API Gateway WebSocket API. One Lambda per route.
- **k8s/self-host**: any WebSocket server (the reference Rust/TS services expose one).
- Frames are JSON. (A binary/CBOR profile may be added later for token streaming.)

## Envelope

**Client → server** (an *action*):

```jsonc
{ "action": "send_message", "requestId": "...", /* ...action-specific fields */ }
```

**Server → client** (an *event*):

```jsonc
{
  "type": "stream_chunk",      // event type
  "requestId": "...",          // correlates to the action
  "status": 202,               // HTTP-like: 202 = ack/in-progress, 200 = final
  "data": { /* event payload */ },
  "node": "knowledge_search",  // (stream_chunk) the workflow node that produced this
  "token": "Hel",              // (stream_token) a streamed token
  "error": { "code": "...", "message": "..." },
  "timestamp": 1733600000000
}
```

## Actions (client → server)

| action | purpose | key request fields | response |
| ------ | ------- | ------------------ | -------- |
| `create_conversation_session` | start a session | `agentId`, `userName?`, `userEmail?`, `browserFingerprint?`, `metadata?`, `supports?` (per-kind Rich-Interaction render capabilities, e.g. `["identity_form"]`) | `sessionId`, `conversationId`, `agentId`, `userParticipantId`, `agentParticipantId` |
| `send_message` | a turn | `sessionId`, `message`, `stream?` | streamed events, then `eventual_response` |
| `get_session` | fetch session | `sessionId` | session snapshot |
| `get_messages` | history | `sessionId`, paging | messages |
| `confirm_tool_action` | resume after a write-confirmation | `sessionId`, `requestId`, `approved` | resumed stream |
| `verify_otp` | submit an OTP code after an auth gate | `sessionId`, `requestId`, `code` | `otp_verified` or `otp_invalid` (see below) |
| `submit_interaction` | resume a turn parked on a Rich Interaction (ANY kind) | `sessionId`, `requestId`, `interactionId`, `kind?`, `values?` or `declined: true` | resumed stream, or `interaction_invalid` (turn stays parked) |
| `cancel` | stop the in-flight turn (the "Stop button") | `requestId` (of the `send_message` to cancel), `sessionId?` | `cancelled` (or nothing, if no turn is running) |
| `ping` | keepalive | — | `pong` |

## Events (server → client)

| type | meaning |
| ---- | ------- |
| `immediate_response` | synchronous ack (e.g. session created) |
| `stream_chunk` | a per-node state snapshot from the agent workflow (carries `node` + filtered `state`) |
| `stream_token` | a single streamed model token |
| `eventual_response` | the final turn result (carries optional `citations` — see below) |
| `keepalive` | heartbeat |
| `write_confirmation_required` | a tool wants to perform a write; client must `confirm_tool_action` |
| `otp_verification_required` / `otp_sent` / `otp_verified` / `otp_invalid` | auth-gated tool flow |
| `interaction_required` | the agent raised a Rich Interaction (`kind` + `spec`, e.g. `identity_intake`); only sent to sessions that declared the kind's capability — client renders the kind's card and replies with `submit_interaction` (see [[Rich Interactions]]) |
| `interaction_invalid` | a `submit_interaction` failed the kind's server-side validation (per-field `errors`); the turn stays parked for a resubmit |
| `cancelled` | terminal event for a client-`cancel`led turn — emitted **in place of** `eventual_response`, echoing the cancelled turn's `requestId` with `status: 499`. No answer payload: the streamed tokens are discarded (not persisted); the user's message stays persisted |
| `error` | `{ code, message }` |
| `pong` | reply to `ping` |

## Citations on `eventual_response`

A grounded answer carries the sources it used. The terminal `eventual_response`'s inner payload (`data.data`) gains an **optional** `citations` array alongside `response` / `needsEscalation`:

```jsonc
{
  "type": "eventual_response",
  "data": {
    "data": {
      "messageId": "…",
      "response": { "responseParts": ["Returns are accepted within 30 days…"] },
      "needsEscalation": false,
      "citations": [
        {
          "id": "doc-returns-policy",                                  // knowledge-base document id (dedup key)
          "title": "acme/handbook@main#policies/returns.md",           // source label
          "url": "https://github.com/acme/handbook/blob/main/policies/returns.md", // GitHub blob/issue URL (when web-sourced)
          "snippet": "SmooAI returns are accepted within 30 days…",     // the retrieved chunk, truncated
          "score": 0.91                                                // relevance (similarity) score
        }
      ]
    }
  }
}
```

A `Citation` is `{ id, title, url?, snippet, score }` (schema: [`spec/domain/citation.schema.json`](../../spec/domain/citation.schema.json); the inline array shape is on [`spec/events/eventual-response.schema.json`](../../spec/events/eventual-response.schema.json)).

- **What grounds a citation**: the runtime collects the knowledge-base documents that actually grounded the turn — the engine's auto-injected `[Relevant knowledge]` context (mirrored by the runtime with the same top-k query) plus every `knowledge_search` tool result. It deduplicates by `id`, caps the count (8), and maps each `KnowledgeResult` → `Citation`: `id` ← `document_id`, `title` ← `source`, `url` ← `source` when it is an `http(s)` URL (the GitHub blob/issue URL stamped on at ingest — see [[Connectors]]) else omitted, `snippet` ← the chunk truncated, `score` ← `score`.
- **Back-compat**: `citations` is absent when the turn retrieved nothing, so clients that predate it are unaffected. Generated clients expose it as an optional field (`Citation` type) after regeneration from `spec/`.

## OTP identity verification (auth gate)

A public agent can gate a tool behind `authLevel: end_user` — the tool only runs once the caller's identity is verified. The Rust reference server implements this as a **host seam**, `smooth_operator::otp::OtpService` (install via `AppState::with_otp_service`), so the server never generates, delivers, or validates a code — the host owns the code store, expiry, attempt counting, and email/SMS delivery.

Flow:

1. A turn calls an `end_user` tool on an unverified session; the auth gate refuses it (the model sees a "verify your identity" refusal). If an `OtpService` is installed **and** the session captured a contact (email at `create_conversation_session`), the server emits `otp_verification_required` (with `availableChannels` + the refused `toolId`), calls `OtpService::send_otp`, then emits `otp_sent` (channel + masked destination).
2. The client submits the received code via `verify_otp`. The server calls `OtpService::verify_otp`:
   - **verified** → the session is marked identity-verified and `otp_verified` is emitted;
   - **rejected** → `otp_invalid` is emitted with the host's `attemptsRemaining` (0 ⇒ locked, restart the flow) and an optional machine-readable `error` (`INVALID_CODE` / `MAX_ATTEMPTS` / `NOT_FOUND` / `EXPIRED`).
3. Once verified, the client **re-sends** its original `send_message`; the gate now passes and the tool runs.

> **Reference-server note:** the reference server does not park/auto-resume the original turn across the OTP round-trip (step 3 is a client re-send). Parking + automatic resume is a host concern behind the same events. With no `OtpService` installed, the gate stays fail-closed (the `end_user` tool is refused, no OTP offered), and a stray `verify_otp` returns `otp_invalid` / `NOT_FOUND`. The reference server currently offers only the `email` channel (`create_conversation_session` captures no phone).

## Rich Interactions (structured cards, channel-normalized)

A Rich Interaction is a typed, server-validated mid-turn ask (`interaction_required { interactionId, kind, spec, reason }` → the client card → `submit_interaction`). Sessions declare per-kind render capabilities in `supports` at create; kinds without their capability degrade to a **conversational fallback** (the raise tool returns kind-specific instructions and the model submits through the generic `submit_interaction` tool — same server-side validator, same canonical resume payload). The kind catalog lives in `spec/interactions/` (first kind: `identity_intake`, capability `identity_form` — name/email/phone with email-shape + E.164 validation, attaching to the session's OTP contact keys). Full design + extension recipe: [[Rich Interactions]].

## Turn cancellation (the "Stop button")

A client stops the in-flight turn by sending a `cancel` action. The reference
server's contract (schemas: [`spec/actions/cancel.schema.json`](../../spec/actions/cancel.schema.json),
[`spec/events/cancelled.schema.json`](../../spec/events/cancelled.schema.json);
fixtures: `cancel_request` / `cancelled_event`):

```jsonc
// client → server
{ "action": "cancel", "requestId": "<the send_message requestId>" }
// server → client (only if a turn was actually running)
{ "type": "cancelled", "requestId": "<echoed>", "status": 499,
  "data": { "requestId": "<echoed>", "status": 499 }, "timestamp": 1749340809000 }
```

Semantics every language server + the frontend **must** replicate:

- **One active turn per connection.** A `send_message` runs at most one turn on a
  socket at a time. A second `send_message` arriving while one is in flight is
  rejected with `error` code `TURN_IN_PROGRESS` — it is **not** run concurrently.
  (`confirm_tool_action` / `submit_interaction` / `verify_otp` are turn *resumes*,
  not new turns, so they are unaffected.)
- **`cancel` aborts the running turn** by dropping the turn future at its next
  `await` point, abandoning the in-flight LLM/tool call, and emits a terminal
  `cancelled` event (status **499**, "client closed request") echoing the
  cancelled turn's `requestId`. It replaces the `eventual_response` that turn
  would otherwise have sent — exactly one terminal event per turn.
- **No active turn ⇒ no-op.** A `cancel` with nothing running emits nothing and
  leaves the connection live.
- **Partial output is discarded.** The user's message is persisted at the start of
  the turn (before the agent loop), so it stays; the assistant reply is persisted
  only at the end, which the abort skips — so a cancelled turn leaves the user
  message with **no** assistant reply. The streamed `stream_token`s the client
  already saw are ephemeral UI, never persisted. (The engine's Groove checkpoint
  store may retain partial mid-turn state, independent of the message log.)
- **Disconnect mid-turn also aborts the turn** (no client remains to receive its
  output) — a strict improvement. This is distinct from **graceful shutdown**
  (SIGTERM), which deliberately lets an in-flight turn *drain* to completion within
  the pod termination window rather than aborting it.

Implementation is connection-local: the reader loop tracks the spawned turn's task
handle and `abort()`s it on `cancel`/disconnect. There is no engine change — the
agent loop is a chain of `await`s, so dropping its future cancels it.

## Mapping to smooth-operator's `AgentEvent` stream

The service subscribes to smooth-operator's `AgentEvent` stream and translates:

| smooth-operator `AgentEvent` | protocol event |
| ---------------------------- | -------------- |
| `Started` | `immediate_response` (status 202) |
| `TokenDelta { content }` | `stream_token` |
| `PhaseStart` / node boundary | `stream_chunk` (with `node`) |
| `ToolCallStart` / `ToolCallComplete` | `stream_chunk` (tool activity in `state`) |
| `HumanInputRequired { Confirm }` | `write_confirmation_required` |
| `HumanInputRequired { Input }` (auth) | `otp_verification_required` |
| `Completed { cost, tokens }` | `eventual_response` (status 200) |
| `Error` | `error` |

## Connection state

Per-connection and per-session state (mirrors the smooai key patterns; backend-specific):

- `connection → session` mapping
- `session → connections` set (ownership / fan-out)
- `user → connections`, `agent → connections` sets
- the session blob (`conversationId`, `agentId`, smooth-operator thread id, participants)

On AWS this lives in DynamoDB (TTL'd) or optional Redis; on k8s in Postgres or Redis. See [[Storage Adapters]].

## `spec/` layout (planned)

```
spec/
├── envelope.schema.json
├── actions/
│   ├── create-conversation-session.schema.json
│   ├── send-message.schema.json
│   └── ...
├── events/
│   ├── stream-chunk.schema.json
│   ├── eventual-response.schema.json
│   └── ...
├── domain/                 # conversation, participant, message, session, checkpoint
└── codegen/                # per-language generator config
```

Each client repo regenerates from `spec/` and runs the shared conformance fixtures, so drift is caught in CI.

---

**In this vault:** [[Home]] · [[The Protocol]] · [[Using the Polyglot Clients]] · [[Citations]] · [[Conversations and Sessions]]
