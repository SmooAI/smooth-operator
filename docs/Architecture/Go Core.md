# Go Core

> **Status:** active — **Phase 0 shipped**. The Rust engine is the reference; C#,
> Python, and TypeScript are complete siblings. This is the Go sibling — the
> **last language**, completing Phase-0 parity across the polyglot cores. See
> [Polyglot Cores](Polyglot%20Cores.md).

## Decision

Add a **native Go engine** (`go/core`, package
`github.com/SmooAI/smooth-operator/go/core`) within the existing Go module. The
existing `go/protocol` package stays a thin protocol client; the core
reimplements the **engine itself** so an agent runs in-process in Go, with native
tools and no separate service.

Mirrors the Phase-0 shape of the other cores: an agentic tool-calling loop over
an OpenAI-compatible chat client, with in-memory knowledge grounding, held to the
**same shared eval suite** against the live gateway.

## What Phase 0 includes

- **`agent.go`** — `SmoothAgent`: inject retrieved knowledge → call the model →
  run any requested tools → loop until a tool-free answer or `MaxIterations`.
  Depends on a `ChatClient` interface so the live `GatewayClient` satisfies it and
  tests inject a fake.
- **`openai.go`** — `GatewayClient`, an OpenAI-compatible `ChatClient` over
  `net/http` (`/chat/completions` with Bearer auth). *(Phase 0 uses `net/http`
  directly, as the sibling cores' OpenAI SDKs do internally; adopting the
  `@smooai/fetch` Go client is a tracked follow-up.)*
- **`knowledge.go`** — `InMemoryKnowledge`, a lexical-overlap retriever.
- **`Tool` / `FuncTool`** — a tool interface + a func wrapper, surfaced to the
  model as OpenAI tool specs.
- **`agent_test.go`** — non-network unit tests (loop, tool calling, knowledge
  injection) via a fake client; always green (`go test`).
- **`evals_test.go`** — gated LLM-as-judge eval against the live gateway, key from
  `@smooai/config`; asserts aggregate mean ≥ 4.0, `t.Skip` when ungated.

**Phase-0 result:** the Go engine scores **5/5 on all shared scenarios**
(including prompt-injection) under an adversarial sonnet judge — at parity with
Rust, C#, Python, and TypeScript. **This completes Phase-0 across all five cores.**

## Running the evals

```sh
SMOOAI_GATEWAY_KEY=$(th config get liteLLMVirtualKeyAiServer --environment=production \
  --org-id <infra-org> --json | jq -r .value) \
  SMOOTH_AGENT_E2E=1 SMOOTH_AGENT_JUDGE_MODEL=claude-sonnet-4-5 \
  go test ./go/core/ -run TestEvalAggregateMeanClearsThreshold -v
```

## Related

- [Polyglot Cores](Polyglot%20Cores.md) · [Python Core](Python%20Core.md) · [TypeScript Core](TypeScript%20Core.md)
