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

**Phase 1 (the WebSocket host)** is also shipped, in the sibling
`SmooAI.SmoothOperator.Server.AspNetCore` project:

```csharp
var builder = WebApplication.CreateBuilder(args);
builder.Services.AddSingleton<IChatClient>(/* your model */);
builder.Services.AddSmoothOperatorServer();          // store + runner + dispatcher
var app = builder.Build();
app.MapSmoothOperatorWebSocket("/ws");               // the protocol endpoint
app.Run();
```

Integration tests boot this host in-process and drive a **real WebSocket** — the C# parity of
the Rust server's `tests/protocol_smoke.rs`.

**Phase 2 (durable storage)** is shipped for the session store: `ISessionStore` is async, and
`SmooAI.SmoothOperator.Server.Postgres` provides a `PostgresSessionStore` so sessions + history
survive a restart. A shared `ISessionStore` contract test runs against **both** the in-memory and
Postgres adapters (the Rust adapter-parity pattern), on a real Postgres via Testcontainers.

```csharp
// Swap durable storage in:
builder.Services.AddSingleton<ISessionStore>(
    await PostgresSessionStore.CreateAsync(connectionString));
builder.Services.AddSmoothOperatorServer();   // uses the registered ISessionStore
```

**Per-agent config + conversation workflows** (SMOODEV-590): register an `IAgentConfigResolver`
and each agent's own `instructions.prompt` drives its system prompt (overriding the org/default
persona); its `conversation_workflow` (goal + intent/criteria steps) runs as a stepped,
judge-advanced flow — the current step is rendered into the prompt and a cheap post-turn judge
advances the (per-conversation-persisted) pointer when the step's criteria are met; its `greeting`
is woven into the first reply only; and its `tool_config.enabledTools` restricts the server's tool set
to the enabled snake_case toolIds (empty/absent ⇒ the full set, unchanged).
`create_conversation_session` carries only an agent UUID, so config is resolved server-side per
turn from the session's agent (mirrors the TS / Python lanes' `AgentConfigResolver`). Config
parsing is tolerant (malformed jsonb degrades to the default persona) and the judge is
failure-tolerant (any error stays on the current step). No resolver registered ⇒ behavior unchanged.

```csharp
builder.Services.AddSingleton<IAgentConfigResolver>(
    new StaticAgentConfigResolver().Set(agentId, new AgentConfig(
        InstructionsPrompt: "You are Ziggy, a pirate concierge.",
        Workflow: AgentConfig.ParseWorkflow(conversationWorkflowJsonb))));
// A multi-tenant host swaps in a resolver backed by the `agents` table. Registering a resolver
// defaults the workflow judge to the LLM judge over your IChatClient; register your own
// IWorkflowJudge (e.g. a distinct cheap model) to override.
```

**Next:** knowledge + checkpoint adapters on Postgres+pgvector, ingestion + connectors, ACL +
auth, then a deployable container. See the
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
