# .NET — ecosystem interop & idioms

`.NET` is a first-class target for smooth-agent. To make it feel native to a .NET-AI developer, we align with the conventions of **`Microsoft.Extensions.AI` (MEAI)** and the **Microsoft Agent Framework (MAF)** — borrowing their abstractions and idioms, **not** their Azure-coupled, in-process execution model. smooth-agent stays protocol-first and provider-agnostic (via smooth-operator's `LlmProvider`); the .NET surface is a thin, idiomatic skin over that.

> Reference: [MAF overview (C#)](https://learn.microsoft.com/en-us/agent-framework/overview/?pivots=programming-language-csharp), [`AIAgent.RunStreamingAsync`](https://learn.microsoft.com/en-us/dotnet/api/microsoft.agents.ai.aiagent.runstreamingasync?view=agent-framework-dotnet-latest).

## Borrow list (ranked)

1. **Build on `Microsoft.Extensions.AI`.** MAF sits on MEAI's `IChatClient`, `ChatMessage`, `AIFunction`, `ChatResponseUpdate`/`AgentRunResponseUpdate` — the de-facto .NET AI standard. Ship a **`SmoothAgentChatClient : IChatClient` facade** over the remote `SmoothAgentClient` so smooth-agent slots into any MAF/Semantic-Kernel/MEAI app, and so a .NET dev's existing `AIFunction` tools work against us. This interop is the single highest-value alignment.
2. **`AgentThread`-style session handle.** Surface a `SmoothAgentThread` that wraps `sessionId`/`threadId` so multi-turn is `thread.RunStreamingAsync(msg)`, not manual id plumbing. Mirror the handle concept in the other language clients.
3. **`AIFunctionFactory.Create()` tool authoring.** For the .NET tool side (Phase 4), reuse MEAI's `AIFunction`/`AIFunctionFactory` (reflection → JSON schema) rather than inventing a C# `ToolDefinition` DSL.
4. **DI-first.** `services.AddSmoothAgent(...)` `IServiceCollection` extensions registering the client/runtime/transport.
5. **Middleware pipeline.** ASP.NET-style delegating handlers for the agent run (pre/post run, pre/post tool) — maps onto smooth-operator's existing `ToolHook` (`pre_call`/`post_call`) and `ConfirmationHook`.
6. **OpenTelemetry `gen_ai.*` semantic conventions** (cross-cutting, not just .NET). MAF emits them; the smooai monorepo already uses them. Adopt in smooth-operator (currently an OTel parity gap) + smooth-agent so traces interop with the ecosystem.
7. **MCP interop.** Make MCP tools first-class in smooth-agent's tool layer; smooth-operator already depends on `rmcp` (Rust MCP SDK).
8. **Naming/philosophy.** Adopt MAF's `RunAsync`/`RunStreamingAsync` naming where it fits; honor "if you can write a function, do that instead of an agent." Their multi-agent patterns (sequential/concurrent/handoff/group-chat) map onto smooth-operator's `cast`/`DispatchSubagentTool` + the `human-agent` participant + escalation.

## Do NOT copy
- **Azure/Foundry coupling** (`Microsoft.Agents.AI.Foundry`, `AzureCliCredential`) — stay provider-agnostic.
- **In-process `AIAgent` execution** — our agent runs behind the WS protocol; reconcile by offering the `IChatClient` facade *over the remote client*.

## Status
The base `SmooAI.SmoothAgent` protocol client (generated types + streaming `IAsyncEnumerable<ServerEvent>` + HITL) is done. The MEAI facade, DI extensions, `SmoothAgentThread`, and OTel conventions are tracked in [ROADMAP.md](ROADMAP.md) (Phase 5 service hosts / Phase 3 OTel).
