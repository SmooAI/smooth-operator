# SmooAI.SmoothOperator.Server

The **smooth-operator service in C#** — the native .NET analog of the Rust
`smooth-operator-server`. It wraps the agent engine
([`SmooAI.SmoothOperator.Core`](../core)) and adds the *system* around it: conversation
sessions, the schema-driven protocol, streaming turns, grounding + citations. It's the
"run the whole smooth-operator system in .NET" layer (vs. embedding just the engine, or
running the Rust server + the .NET client).

Conformance is enforced: every event the server produces is validated against the **same
`spec/` schemas + conformance fixtures** the Rust reference server is held to (via the
protocol client's `ProtocolValidator`).

## Status — Phase 0 (the protocol runner)

Shipped:

- `ISessionStore` / `InMemorySessionStore` — sessions + conversation message logs.
- `TurnRunner` — drives one `send_message` turn: load prior history, retrieve grounding
  knowledge, run the engine streaming, emit `stream_token`s, persist the reply, collect
  citations. (The C# analog of the Rust `run_streaming_turn`.)
- `FrameDispatcher` — routes an incoming frame by its `action` (`ping` /
  `create_conversation_session` / `get_session` / `send_message`) and emits the response
  event(s) to a sink. Transport-agnostic.
- `ProtocolEvents` — builders for the event frames, byte-compatible with the Rust shapes
  (including the triple-nested `eventual_response.data.data`).

The event sequence for a turn — `immediate_response` (202) → `stream_token`(s) →
`eventual_response` (200, with `messageId` + `response.responseParts` + `citations`) — is
produced and schema-validated. 5 conformance tests.

**Next:** the ASP.NET Core WebSocket `/ws` host, then durable Postgres storage, ingestion +
connectors, ACL + auth, and a deployable host. See the
[Server roadmap](../../docs/Architecture/Polyglot%20Cores.md#server-roadmap-c) in the
Polyglot Cores doc.

## Shape of it

```csharp
var store = new InMemorySessionStore();
var runner = new TurnRunner(chatClient, store, knowledgeBase);   // chatClient = any IChatClient
var dispatcher = new FrameDispatcher(store, runner);

// A transport (later: the WS host) feeds raw frames in and writes the events out.
await dispatcher.DispatchAsync(rawFrameJson, evt => socket.Send(evt.ToJsonString()));
```

## Build & test

```bash
dotnet test dotnet/server/tests/SmooAI.SmoothOperator.Server.Tests.csproj
# or the whole solution (engine + server + client):
dotnet test dotnet/SmooAI.SmoothOperator.slnx
```
