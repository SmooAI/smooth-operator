# Python Core

> **Status:** active — **Phase 0 shipped**. The Rust engine
> `smooai-smooth-operator-core` is the reference; the C# core (`dotnet/core`) is
> the first complete sibling. This is the Python sibling. See
> [Polyglot Cores](Polyglot%20Cores.md) for the overall initiative.

## Decision

Add a **native Python engine** (`python/core/`, package `smooth-operator-core`),
not another protocol client. The existing `python/` package (`smooth-operator`)
stays a thin protocol client that talks to a server over the WebSocket protocol;
the Python *core* reimplements the **engine itself** so an agent runs in-process
in Python — native tools, native debugging, no separate service.

We mirror the **C# Core's Phase-0 approach** rather than inventing a new shape:
an agentic tool-calling loop over an OpenAI-compatible chat client, with
in-memory knowledge grounding, held to the **same shared eval suite** against the
live gateway. Parity is enforced by behavior (the eval scenarios), not by
identical type shapes.

## Why mirror C# Phase 0

The C# core is a completed, blessed reference for "how a sibling engine starts":
a `SmoothAgent` over `IChatClient`, `AgentOptions`, in-memory knowledge, and a
gated LLM-as-judge eval. Python's analogue is exact:

| Concept            | C# (`dotnet/core`)            | Python (`python/core`)                         |
| ------------------ | ----------------------------- | ---------------------------------------------- |
| Model client       | `IChatClient` (MEAI + OpenAI) | `openai.AsyncOpenAI` (OpenAI-compatible)       |
| Agent              | `SmoothAgent`                 | `SmoothAgent`                                  |
| Options            | `AgentOptions`                | `AgentOptions`                                 |
| Tools              | `AIFunction`                  | `Tool` / `FunctionTool`                        |
| Knowledge          | `InMemoryKnowledgeBase`       | `InMemoryKnowledge` (lexical-overlap retrieval)|
| Eval               | `EvalTests` (xUnit, gated)    | `test_evals.py` (pytest, gated)                |

## What Phase 0 includes

- **Agentic loop** (`agent.py`): inject retrieved knowledge into the system
  prompt, call the model, run any requested tools, feed results back, loop until
  the model answers without a tool call or `max_iterations` is hit.
- **Tools**: a `Tool` protocol + `FunctionTool` to wrap an async function
  (the `AIFunctionFactory` analogue), surfaced to the model as OpenAI tool specs.
- **Knowledge** (`knowledge.py`): `InMemoryKnowledge`, a lexical-overlap
  retriever (Phase-0 parity with the Rust engine's in-memory lexical store).
- **Eval** (`tests/test_evals.py`): runs the native `SmoothAgent` on the shared
  scenarios against the live gateway, judges each reply, asserts aggregate
  mean ≥ 4.0. Gated on `SMOOTH_AGENT_E2E=1` + `SMOOAI_GATEWAY_KEY` (fetched from
  `@smooai/config` via the `th config` runner), so it skips cleanly in CI.

**Phase-0 result:** the Python engine scores **5/5 on all shared scenarios**
(including prompt-injection) under an adversarial sonnet judge — at parity with
the Rust and C# engines.

## Deliberately deferred (layer on, exactly as C# did past Phase 0)

Context compaction, cost/budget tracking, checkpointing, reranking, memory,
sub-agents, and a vector knowledge store. None change the loop's shape — they are
additive, and the eval suite ratchets the bar up as they land.

## Running the evals

```sh
SMOOAI_GATEWAY_KEY=$(th config get liteLLMVirtualKeyAiServer --environment=production \
  --org-id <infra-org> --json | jq -r .value) \
  SMOOTH_AGENT_E2E=1 SMOOTH_AGENT_JUDGE_MODEL=claude-sonnet-4-5 \
  uv run pytest python/core/tests/test_evals.py -s
```

## Related

- [Polyglot Cores](Polyglot%20Cores.md) — the overall initiative
- `dotnet/core` — the C# sibling this mirrors
- `rust/smooth-operator` — the reference engine
- `rust/evals`, `dotnet/core/tests/EvalTests.cs` — the sibling eval suites
