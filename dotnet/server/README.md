# SmooAI.SmoothOperator.Server

<p>
  <a href="https://smoo.ai"><img src="https://img.shields.io/badge/Smoo_AI-platform-00A6A6?style=for-the-badge&labelColor=020618" alt="Smoo AI"></a>
  <a href="https://github.com/SmooAI/smooth-operator/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-MIT-FF6B6C?style=for-the-badge&labelColor=020618" alt="license"></a>
  <a href="https://dotnet.microsoft.com"><img src="https://img.shields.io/badge/.NET-8.0-00A6A6?style=for-the-badge&labelColor=020618" alt=".NET 8.0"></a>
</p>

**Wiring a chat loop is a weekend project. A production agent _server_ is not.**

Sessions that survive a reconnect. A wire protocol your clients can actually speak. Streaming turns you can watch token by token. Tools the model can call — and hard limits on the ones it must never call. Human-in-the-loop when a tool wants to write.

`SmooAI.SmoothOperator.Server` is that server, native to ASP.NET Core. It wraps the agent engine ([`SmooAI.SmoothOperator.Core`](https://www.nuget.org/packages/SmooAI.SmoothOperator.Core)) — driving a `SmoothAgent` per turn against any `Microsoft.Extensions.AI` `IChatClient` — and speaks the [smooth-operator](https://github.com/SmooAI/smooth-operator) wire protocol ([`spec/`](https://github.com/SmooAI/smooth-operator/tree/main/spec)). It's the .NET sibling of the [Rust](../../rust/smooth-operator-server), [Go](../../go/server), [TypeScript](../../typescript/server), and [Python](../../python/server) servers, all speaking the one protocol.

---

## Spin up a real agent server

The WebSocket host ships in the sibling `SmooAI.SmoothOperator.Server.AspNetCore` project — the protocol endpoint is three lines of DI:

```csharp
var builder = WebApplication.CreateBuilder(args);
builder.Services.AddSingleton<IChatClient>(/* your model */);
builder.Services.AddSmoothOperatorServer();          // store + runner + dispatcher
var app = builder.Build();
app.MapSmoothOperatorWebSocket("/ws");               // the protocol endpoint
app.Run();
```

That's a full agent backend — sessions, streaming turns, tool-calling, citations — on one WebSocket. Integration tests boot this exact host in-process and drive a **real WebSocket**, the C# parity of the Rust server's `tests/protocol_smoke.rs`. Every event is schema-validated against the same `spec/` fixtures the Rust reference server is held to (via the client's `ProtocolValidator`).

---

## Extensible — and safe by construction

An agent is only useful when it can *do* things, and only trustworthy when you can say what it may never do. This server gives you both seams.

**Give it your tools.** Any `Microsoft.Extensions.AI` `AITool` merges with the built-ins for every turn:

```csharp
var openTicket = AIFunctionFactory.Create(
    (string subject) => $"ticket opened: {subject}",
    name: "open_ticket",
    description: "Open a support ticket for the current customer.");

var runner = new TurnRunner(chatClient, store, tools: new AITool[] { openTicket });
```

**Or let it gain tools with no redeploy.** The server hosts [SEP extensions](https://github.com/SmooAI/smooth-operator/blob/main/docs/TOOLS.md) via `ExtensionServerHost` — out-of-process tool providers discovered at runtime, their `ui/confirm` prompts bridged into the protocol's confirmation frames for HITL. Gated: an extension contributes tools **only** if you name it in `SMOOTH_EXTENSIONS_ALLOW`. Nothing loads by default.

**Now declare the lines it can't cross.** Register an `IAgentConfigResolver`, and every tool — built-in, yours, or from an extension — flows through the same gates:

```csharp
builder.Services.AddSingleton<IAgentConfigResolver>(
    new StaticAgentConfigResolver().Set(agentId, new AgentConfig(
        InstructionsPrompt: "You are Ziggy, a pirate concierge.",
        Workflow: AgentConfig.ParseWorkflow(conversationWorkflowJsonb))));
```

- **Per-agent allow-list** — the agent's `tool_config.enabledTools` restricts its turn to exactly those snake_case toolIds. Off the list, off the table (empty/absent ⇒ the full set, unchanged).
- **The authLevel gate** — a tool that opts in with `supportsAuthRequirement` is *blocked at call time* on a public agent when tagged `admin`, or when tagged `end_user` and the session isn't verified — via the session's OTP bit or the `ISessionAuthenticator` seam (default: store-backed, **fails closed** until OTP verification). Internal agents auto-satisfy.
- **End-user OTP flow** — a refused `end_user` tool can offer a one-time-code identity flow via the `IOtpService` seam (`SendOtpAsync` / `VerifyOtpAsync`); the server never generates, holds, or validates a code.

Each entry's `config` object is handed to the tool at invocation (via `AIFunctionArguments.Context["smooth.tool_config"]`). Config parsing is tolerant (malformed jsonb degrades to the default persona) and the workflow judge is failure-tolerant (any error stays on the current step). No resolver registered ⇒ behavior unchanged.

You decide what the agent can touch; the runner enforces it.

---

## What's shipped

- **Phase 0 — the protocol runner.** `ISessionStore` / `InMemorySessionStore`; `TurnRunner` (load history → retrieve grounding → stream the engine → emit `stream_token`s → persist → collect citations); `FrameDispatcher` (routes `ping` / `create_conversation_session` / `get_session` / `send_message`, transport-agnostic); `ProtocolEvents` builders byte-compatible with the Rust shapes. The turn sequence — `immediate_response` (202) → `stream_token`(s) → `eventual_response` (200, with `messageId` + `responseParts` + `citations`) — is produced and schema-validated.
- **Phase 1 — the WebSocket host** (`SmooAI.SmoothOperator.Server.AspNetCore`): `AddSmoothOperatorServer()` + `MapSmoothOperatorWebSocket("/ws")`, driven by real-WebSocket integration tests.
- **Phase 2 — durable storage**: `ISessionStore` is async, and `SmooAI.SmoothOperator.Server.Postgres` provides a `PostgresSessionStore` so sessions + history survive a restart. A shared `ISessionStore` contract test runs against **both** the in-memory and Postgres adapters (real Postgres via Testcontainers).

```csharp
// Swap durable storage in:
builder.Services.AddSingleton<ISessionStore>(
    await PostgresSessionStore.CreateAsync(connectionString));
builder.Services.AddSmoothOperatorServer();   // uses the registered ISessionStore
```

- **Knowledge grounding + citations**: `TurnRunner` retrieves grounding from an `IKnowledgeBase` per turn (ACL-filtered via `IAclKnowledge`), optionally reorders with an `IReranker` (`GatewayReranker` cross-encoder or a network-free lexical fallback, **off by default**), and returns the sources as `citations` on the `eventual_response`. `SmooAI.SmoothOperator.Server.Postgres` ships the durable `PostgresKnowledgeBase` / `PostgresAclKnowledgeStore` (pgvector, ACL filtered in SQL) + `PostgresCheckpointStore`.
- **Ingestion + connectors**: `IConnector` (+ `MockConnector`), a whitespace-aware `Chunker`, an `IngestPipeline` (connector → chunk → embed → store), and a real `GitHubConnector` (lists the repo tree, fetches text/code files, each doc stamped with its `github:owner/repo` ACL group). *(Notion/Slack connectors are landing in sibling PRs — GitHub is the connector on `main`.)*
- **HITL write-confirmation**: a turn calling a `SMOOTH_AGENT_CONFIRM_TOOLS`-matched tool parks and emits `write_confirmation_required`; the client resumes it with a `confirm_tool_action` frame. `ConfirmationRegistry` tracks the in-flight confirmation per session; SEP extensions' `ui/confirm` prompts bridge into the same frames.
- **`/admin/*` API + deployable host**: `MapSmoothOperatorAdmin()` mounts auth-gated `/admin/me`, `/admin/connectors`, and `POST /admin/reindex` (re-ingest without a restart). `SmooAI.SmoothOperator.Server.Host` is a runnable ASP.NET app (model + storage + auth + startup ingestion from env config) shipped as a container via a `Dockerfile`.
- **Per-agent config + conversation workflows, the authLevel gate, SEP extension hosting, and the OTP flow** described above.

**Still open:** the .NET Notion/Slack connectors (in-flight), wiring the checkpoint adapter into a resumable live-turn loop (the adapter is contract-tested but `TurnRunner` is single-turn today), and a live-gateway integration test. See the [Server roadmap](../../docs/Architecture/Polyglot%20Cores.md#server-roadmap-c).

---

## Five languages, one protocol

The *same* server — same wire protocol, same conformance corpus — exists in five languages. Run it where your stack already lives.

| Language | Server package | Registry |
| --- | --- | --- |
| **C# / .NET** | `SmooAI.SmoothOperator.Server` | in-repo (this project) |
| **Rust** | `smooai-smooth-operator-server` | [crates.io](https://crates.io/crates/smooai-smooth-operator-server) |
| **TypeScript** | `@smooai/smooth-operator-server` | [in-repo](../../typescript/server) |
| **Python** | `smooai-smooth-operator-server` | [in-repo](../../python/server) |
| **Go** | `github.com/SmooAI/smooth-operator/go/server` | [in-repo](../../go/server) |

Every native client — [TypeScript](https://www.npmjs.com/package/@smooai/smooth-operator), Go, .NET, Python, Rust — connects to any of them unmodified.

---

## Build & test

```bash
dotnet test dotnet/server/tests/SmooAI.SmoothOperator.Server.Tests.csproj
# or the whole solution (engine + server + client):
dotnet test dotnet/SmooAI.SmoothOperator.slnx
```

---

Part of the **[smooth-operator](https://github.com/SmooAI/smooth-operator)** service — Smoo AI's polyglot AI agent service. Don't want to run it yourself? **[lom.smoo.ai](https://lom.smoo.ai)** hosts it for you.

## License

MIT © 2026 Smoo AI. See [LICENSE](https://github.com/SmooAI/smooth-operator/blob/main/LICENSE).
