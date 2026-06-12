# Polyglot Cores

> **Status:** active (C# is the first sibling core, in progress).
> The Rust engine `smooai-smooth-operator-core` is the **reference implementation**.

## The idea

smooth-operator has, until now, been **one Rust engine + polyglot protocol clients**:
every other language talked to a Rust server over the WebSocket protocol (a network
hop). A **polyglot core** goes further — it reimplements the *engine itself* natively
in each target language, so the agent runs **in-process** in that language's runtime,
with native tools, native debugging, and no separate service to operate.

Each language core learns from that ecosystem's best agent framework rather than
transliterating Rust:

| Language | Learns from | LLM/model abstraction |
| --- | --- | --- |
| **Rust** (reference) | — | `LlmProvider` trait |
| **C#** (first sibling) | **Microsoft Agent Framework (MAF)** + `Microsoft.Extensions.AI` | `IChatClient` |
| *(future)* Python | LangGraph / Agno | (TBD) |
| *(future)* TypeScript | — | (TBD) |

This is deliberately **not** the FFI-bindings approach (one Rust core exposed to every
language via uniffi/napi/PyO3). Async + streaming across an FFI boundary is painful,
and FFI bindings don't *feel* native. Polyglot cores cost more to maintain, but each
one is a first-class library in its ecosystem. What keeps them honest is a **shared
behavioral contract**, not shared code.

## The shared contract — parity is enforced, not assumed

A sibling core is "correct" when it passes the same three layers the Rust reference
passes. It does **not** have to mirror Rust's type shapes — only its behavior.

1. **Protocol conformance** — every instance in [`spec/conformance/fixtures.json`](../../spec/conformance/fixtures.json)
   round-trips its declared JSON Schema. (Relevant when the core is wrapped in the
   server/protocol layer; the engine itself is protocol-agnostic.)
2. **Behavioral parity unit tests** — the engine's defining behaviors, ported as the
   language's idiomatic tests. The canonical Phase-0 assertion, from the Rust core:
   *a text response with no tool calls ends the loop after exactly one LLM call, and
   the user's message reached the model.* Each phase below adds its parity tests.
3. **Eval scenarios** — the five [`rust/evals`](../../rust/evals) scenarios
   (`grounded_answer`, `honest_no_knowledge`, `tool_use_supported_answer`,
   `multi_turn_coherence`, `tone_helpfulness`), judged by an LLM against the same
   rubric, mean score ≥ 4.0. A sibling core runs the *same* scenarios against a live
   model and must clear the same bar.

If a behavior isn't covered by one of these three, it isn't part of the contract yet —
add the test to the reference first, then port it.

## Engine concept inventory (Rust → C#)

What every core implements, and how the C# core expresses each on MEAI/MAF idioms.
The C# core **reuses MEAI primitives** wherever MAF already has the right abstraction,
and only invents types for the agentic machinery MEAI doesn't provide (the loop,
checkpoints, cast, HITL, cost, memory/knowledge injection).

| Rust engine | C# core | Notes |
| --- | --- | --- |
| `LlmProvider` trait | `Microsoft.Extensions.AI.IChatClient` | any MEAI provider plugs in (Azure OpenAI, OpenAI, Ollama, the smooth gateway) |
| `Message` / `Role` | `ChatMessage` / `ChatRole` | reuse MEAI |
| `Tool` trait / `ToolSchema` | `AIFunction` / `AITool` (`AIFunctionFactory.Create`) | a normal C# method becomes a tool |
| `ToolCall` / `ToolResult` | `FunctionCallContent` / `FunctionResultContent` | reuse MEAI content types |
| `StreamEvent` | `ChatResponseUpdate` / `AgentRunResponseUpdate` | MAF streaming shape |
| `Agent` / `agent.run()` / `run_with_channel()` | `SmoothAgent` / `RunAsync` / `RunStreamingAsync` | MAF naming; **we own the loop** |
| `AgentEvent` enum | `AgentRunResponseUpdate` + typed run events | |
| `MockLlmClient` | `MockChatClient : IChatClient` | scripted test double |
| `Memory` / `KnowledgeBase` | `IAgentMemory` / `IKnowledgeBase` | injected as context pre-turn |
| `CheckpointStore` | `ICheckpointStore` | thread state save/resume |
| `Cast` / `OperatorRole` / `DispatchSubagentTool` | `Cast` / `OperatorRole` / dispatch tool | maps to MAF handoff/group-chat patterns |
| HITL (`HumanRequest`/`HumanResponse`) | `IHumanGate` pause/resume | tool write-confirmation + input |
| `CostTracker` / `CostBudget` | `CostTracker` / `CostBudget` | usage accounting + budget enforcement |

## Phased roadmap (C#)

Mirrors how the Rust core was bootstrapped (harness first, then layer up). Each phase
ships green parity tests before the next starts.

- **Phase 0 — harness + agentic loop** *(shipped)*: `IChatClient`-driven loop,
  `AIFunction` tools, `MockChatClient`, `RunAsync`/`RunStreamingAsync`, max-iteration
  guard, usage accumulation. Parity test: text-turn ends after one call; tool-turn
  executes the function and feeds the result back.
- **Phase 1 — conversation + compaction** *(shipped)*: `SmoothAgentThread` carries
  history across turns; `MaxContextTokens` budget with a `SlidingWindow` compaction
  strategy (preserves system + latest user). Parity tests: multi-turn continuity;
  compaction trims old messages under budget.
- **Phase 2 — memory + knowledge** *(shipped)*: pluggable `IKnowledgeBase` /
  `IAgentMemory` (with deterministic in-memory lexical impls); the agent retrieves
  the top-K hits for the user's message and injects them as grounding context before
  answering (RAG). Parity tests: ranked retrieval, knowledge + memory injection,
  no-hit injects nothing.
- **Phase 3 — checkpointing + resume**: `ICheckpointStore` (in-memory → file → SQLite),
  resume-or-new.
- **Phase 4 — HITL**: `IHumanGate` confirmation hook, pause on write tools, resume.
- **Phase 5 — cast / subagents**: `OperatorRole` clearance, dispatch-subagent tool,
  isolated sidekick transcripts.
- **Phase 6 — cost + budgets**: `CostTracker`, `CostBudget` enforcement, model pricing.
- **Phase 7 — evals**: run the five shared eval scenarios against a live model, ≥ 4.0.
- **Phase 8 — packaging**: publish `SmooAI.SmoothOperator.Core` to NuGet (mirror the
  npm/crates publish workflows).

## Adding the Nth language core

1. Pick the ecosystem's idiomatic agent/LLM abstraction (the language's MEAI-equivalent).
2. Port Phase 0 first — the loop + a mock model + the one parity assertion.
3. Work the phases in order; each must pass the shared parity tests before moving on.
4. Wire the eval scenarios; clear ≥ 4.0.
5. Package for the ecosystem's registry.

## Related

- [[Architecture Overview]] — the system around the engine.
- [[.NET MEAI]] — the .NET *client* surface (the interop skin); this doc is about the *engine*.
- [[Using the Polyglot Clients]] — the protocol-client story (the other axis of "polyglot").
