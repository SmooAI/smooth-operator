# SmooAI.SmoothOperator.Core

The **native C# implementation of the smooth-operator agent engine** — an in-process,
NuGet-installable sibling of the Rust reference engine `smooai-smooth-operator-core`.
It is **not** a client to a remote server: it *is* the agent, running in your .NET
process.

It's built on **`Microsoft.Extensions.AI`** and learns from **Microsoft Agent Framework**
idioms, so it slots into the .NET AI ecosystem natively:

- Any MEAI provider is the model (`IChatClient` — Azure OpenAI, OpenAI, Ollama, the
  smooth gateway, …).
- A normal C# method is a tool (`AIFunctionFactory.Create`).
- `RunAsync` / `RunStreamingAsync` (MAF naming).

Behavioral parity with the Rust reference is enforced by the **shared conformance
fixtures + eval scenarios**, not by identical type shapes — see
[Polyglot Cores](https://github.com/SmooAI/smooth-operator/blob/main/docs/Architecture/Polyglot%20Cores.md).

## Status

- **Phase 0 — the agentic loop** (shipped): `IChatClient`-driven loop, `AIFunction` tools,
  usage accumulation, max-iteration guard, streaming. `MockChatClient` test double.
- **Phase 1 — conversation + compaction** (shipped): `SmoothAgentThread` for multi-turn
  history, `MaxContextTokens` budget + `SlidingWindow` compaction.
- **Phase 2 — memory + knowledge** (shipped): pluggable `IKnowledgeBase` / `IAgentMemory`,
  retrieved and injected as pre-turn grounding context (RAG).
- **Phase 3 — checkpointing + resume** (shipped): `ICheckpointStore` + `CheckpointStrategy`;
  snapshot a run and `ResumeThreadAsync` to rebuild a thread after a crash.

18 parity tests green. See the phased roadmap in the Polyglot Cores doc.

```csharp
// Multi-turn: pass a thread to each run and it remembers.
var thread = agent.GetNewThread();
await agent.RunAsync("My name is Brent.", thread);
var r = await agent.RunAsync("What's my name?", thread);   // "Your name is Brent."
```

```csharp
// RAG: give it a knowledge base and it grounds answers in retrieved context.
var kb = new InMemoryKnowledgeBase();
await kb.IngestAsync(new KnowledgeDocument("returns", "The return window is 17 days.", "policy.md"));

var agent = new SmoothAgent(model, new AgentOptions { Knowledge = kb });
var r = await agent.RunAsync("How long is the return window?");   // grounded in policy.md
```

## Quickstart

```csharp
using Microsoft.Extensions.AI;
using SmooAI.SmoothOperator.Core;

// Any IChatClient — here, an OpenAI-compatible endpoint (the smooth gateway, Azure, …).
IChatClient model = /* your MEAI client */;

var options = new AgentOptions { Instructions = "You are a helpful support agent." };
options.Tools.Add(AIFunctionFactory.Create(
    (string city) => $"The weather in {city} is sunny.",
    "get_weather", "Gets the weather for a city"));

var agent = new SmoothAgent(model, options);

AgentRunResponse result = await agent.RunAsync("What's the weather in Chicago?");
Console.WriteLine(result.Text);          // final answer
Console.WriteLine(result.Iterations);    // LLM calls it took
Console.WriteLine(result.Usage.TotalTokenCount);
```

Stream it instead:

```csharp
await foreach (var update in agent.RunStreamingAsync("What's the weather in Chicago?"))
    Console.Write(update.Text);
```

## Build & test

```bash
dotnet test dotnet/core/tests/SmooAI.SmoothOperator.Core.Tests.csproj
# or the whole solution (client + core):
dotnet test dotnet/SmooAI.SmoothOperator.slnx
```

## Relationship to `SmooAI.SmoothOperator`

`SmooAI.SmoothOperator` (in `dotnet/src`) is the **protocol client** — it talks to a
remote Rust `smooth-operator-server` over WebSocket, and exposes an `IChatClient` facade.
`SmooAI.SmoothOperator.Core` (here) is the **engine** — it runs the agent locally. They're
complementary: use the client to reach a hosted agent, use the core to *be* the agent.
