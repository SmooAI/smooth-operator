# TypeScript Core

> **Status:** active — **Phase 0 shipped**. The Rust engine is the reference; the
> C# and Python cores are complete siblings. This is the TypeScript sibling. See
> [Polyglot Cores](Polyglot%20Cores.md).

## Decision

Add a **native TypeScript engine** (`typescript/core/`, package
`@smooai/smooth-operator-core`), a new pnpm-workspace member alongside the
published `@smooai/smooth-operator` protocol client. The client stays a thin
protocol client (talks to a server over the WebSocket protocol); the core
reimplements the **engine itself** so an agent runs in-process in Node, with
native tools and no separate service.

Mirrors the C#/Python Phase-0 shape: an agentic tool-calling loop over an
OpenAI-compatible chat client, with in-memory knowledge grounding, held to the
**same shared eval suite** against the live gateway.

## Parity (vs the other cores)

| Concept       | Rust (reference)        | C# `dotnet/core`        | Python `python/core`      | TS `typescript/core`            |
| ------------- | ----------------------- | ----------------------- | ------------------------- | ------------------------------- |
| Model client  | `LlmClient`             | `IChatClient`           | `openai.AsyncOpenAI`      | `openai` (`ChatClientLike`)     |
| Agent/Options | `Agent` / `AgentConfig` | `SmoothAgent`/`Options` | `SmoothAgent`/`AgentOptions` | `SmoothAgent`/`AgentOptions` |
| Tools         | `Tool`                  | `AIFunction`            | `Tool`/`FunctionTool`     | `Tool`                          |
| Knowledge     | in-memory lexical       | `InMemoryKnowledgeBase` | `InMemoryKnowledge`       | `InMemoryKnowledge`             |
| Eval          | `rust/evals`            | `EvalTests` (xUnit)     | `test_evals.py` (pytest)  | `evals.test.ts` (vitest)        |

## What Phase 0 includes

- **`src/agent.ts`** — `SmoothAgent`: inject retrieved knowledge → call the model
  → run any requested tools → loop until a tool-free answer or `maxIterations`.
  Typed against a minimal `ChatClientLike` so the real `openai` SDK satisfies it
  and tests inject a fake.
- **`src/knowledge.ts`** — `InMemoryKnowledge`, a lexical-overlap retriever.
- **`Tool`** — a tool interface surfaced to the model as OpenAI tool specs.
- **`test/agent.test.ts`** — non-network unit tests (loop, tool calling, knowledge
  injection) via a fake client; always green (vitest).
- **`test/evals.test.ts`** — gated LLM-as-judge eval against the live gateway,
  key from `@smooai/config`; asserts aggregate mean ≥ 4.0, `describe.skipIf` when
  ungated.

**Phase-0 result:** the TS engine scores **5/5 on all shared scenarios**
(including prompt-injection) under an adversarial sonnet judge — at parity with
Rust, C#, and Python.

## Deliberately deferred

Compaction, budget/cost, checkpointing, reranking, memory, sub-agents, vector
knowledge — additive, post-Phase-0, exactly as the other cores did.

## Running the evals

```sh
SMOOAI_GATEWAY_KEY=$(th config get liteLLMVirtualKeyAiServer --environment=production \
  --org-id <infra-org> --json | jq -r .value) \
  SMOOTH_AGENT_E2E=1 SMOOTH_AGENT_JUDGE_MODEL=claude-sonnet-4-5 \
  pnpm --filter @smooai/smooth-operator-core test
```

## Related

- [Polyglot Cores](Polyglot%20Cores.md) · [Python Core](Python%20Core.md)
- `dotnet/core`, `rust/smooth-operator` — sibling engines
