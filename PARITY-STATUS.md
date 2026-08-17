# Parity status

**What "five languages" actually means today**, surface by surface. Every row was verified against merged code — the symbol or file that implements it — not against the pull request that claimed it.

Two repos are in scope: **[smooth-operator-core](https://github.com/SmooAI/smooth-operator-core)** (the embeddable agent engine) and **smooth-operator** (this repo — the service that wraps it).

Legend: **✅ all five** · **◐ partial** · **Rust-first** = shipped in Rust, seam or declaration only elsewhere.

---

## 1. The engine — at parity across all five

Rust is the reference; TypeScript, Python, Go and C#/.NET are native ports. These capabilities are present in **all five** engines:

| | |
| --- | --- |
| Agentic tool-calling loop | ✅ all five |
| Real gateway LLM client (OpenAI-compatible, live endpoint) | ✅ all five |
| `LlmProvider` seam + deterministic record/replay mock | ✅ all five |
| Streaming (text, tool calls, tool results) | ✅ all five |
| Parallel tool calls · retry/backoff | ✅ all five |
| Structured output (`response_format` + JSON Schema) | ✅ all five |
| Multimodal image input (`image_url` content parts) | ✅ all five |
| Knowledge (in-memory + vector) · lexical rerank · memory · compaction | ✅ all five |
| Prompt-cache split + Anthropic `cache_control` request markers | ✅ all five |
| Project-context loader (`CONTEXT.md` / `AGENTS.md` / `CLAUDE.md` stack) | ✅ all five |
| Cost / budget accounting + gateway cost headers (streaming path included) | ✅ all five |
| Checkpointing · conversation thread · sub-agents · typed workflow graph | ✅ all five |
| Cast (roles + clearance) · human-in-the-loop gate | ✅ all five |
| Tool-hook lifecycle, including the **mutable** post-call result | ✅ all five |
| Permission gate + deny-policy + stored grants | ✅ all five |
| NarcHook secret-detection + prompt-injection scanner | ✅ all five |
| Deferred tools + `tool_search` | ✅ all five |
| Provider routing: presets, per-activity slots, fallback chains, per-model quirks, LiteLLM alias resolution | ✅ all five |
| SEP extension host: manifest, capabilities, tool/provider/UI/command lanes, restart backoff, bounded observe lane | ✅ all five |
| Durable-execution **seam** (`AgentExecutor` + activities + `TurnPolicy`) | ✅ all five |

### Why the claim holds

Three shared corpora are generated **from the Rust reference** and replayed by all five engines — Rust included, so the reference cannot drift away from its own ports unnoticed:

- [`spec/evals/scenarios.json`](https://github.com/SmooAI/smooth-operator-core/blob/main/spec/evals/scenarios.json) (core repo) — LLM-as-judge eval scenarios (same ids, same count, ratcheted against deletion).
- [`spec/narc/corpus.json`](https://github.com/SmooAI/smooth-operator-core/blob/main/spec/narc/corpus.json) (core repo) — the secret/injection detection set, pinned pattern-by-pattern **and severity-by-severity**. A port that downgrades a `BLOCK` to an `ALERT` fails its own suite.
- [`spec/providers/routing.json`](https://github.com/SmooAI/smooth-operator-core/blob/main/spec/providers/routing.json) (core repo) — every preset slot's resolved model, base URL, key and wire format.

On the service side, all five servers replay [`spec/conformance/scenarios`](spec/conformance) — in **this** repo — against the engine's deterministic mock, so they must emit identical protocol output.

---

## 2. Rust-first — the engine's two honest exceptions

| Surface | Status |
| --- | --- |
| **Extension sandbox / integrity hardening** | **Rust-first.** Capability *declarations* are parsed and honoured in all five, but process-level confinement and manifest-integrity verification are Rust-only. See [Extension-Sandboxing-Design.md](https://github.com/SmooAI/smooth-operator-core/blob/main/docs/Extension-Sandboxing-Design.md). |
| **Durable-execution backend** | **Rust-first.** The `AgentExecutor` seam is in all five with an in-process executor that delegates verbatim to `run`, so the seam changes nothing until something plugs in. Only Rust ships a real backend — the separate, feature-gated `smooth-operator-temporal` crate (turn as a Temporal workflow, model/tool calls as activities: crash-safe resume, durable HITL signals, durable timers). The other four carry a `TODO(ADR-030)` naming the opt-in package their backend belongs in; no engine pulls a Temporal SDK into your dependency tree. |

---

## 3. The service — depth is uneven, and this is the part to read before picking a language

All five servers carry the transport core: frame dispatch, per-turn engine, sessions, auth, graceful drain, and a Postgres store over the same conversation/message/participant/session/settings tables. Past that:

| | Rust | C#/.NET | Go | TypeScript | Python |
| --- | --- | --- | --- | --- | --- |
| Transport core (dispatch · sessions · auth · drain) | ✅ | ✅ | ✅ | ✅ | ✅ |
| Shared scenario conformance corpus | ✅ | ✅ | ✅ | ✅ | ✅ |
| Postgres conversation store | ✅ | ✅ | ✅ | ✅ | ✅ |
| Second storage backend (DynamoDB + S3 Vectors) | ✅ | — | — | — | — |
| Persistent checkpoint / knowledge / ACL-knowledge stores | ✅ | ✅ | — | — | — |
| Deep ingestion + ACL surface | ✅ | ✅ | ◐ | ◐ | ◐ |
| Backplane `attach`/`detach` | ✅ | — | ✅ | ✅ | ✅ |
| Backplane `publish` (event fan-out) | ✅ | — | ✅ | ✅ | — |
| **Cross-pod backplane (Redis / NATS)** | ✅ | — | — | — | — |

**The operational consequence:** only the **Rust** server scales past one replica today. Go, TypeScript and Python run an in-memory backplane — correct for a single process, silently wrong the moment you run two pods, because an event published on pod A never reaches a socket held by pod B. C# has no backplane surface at all.

---

## 4. Known-remaining

Three items, in the order they'd bite:

1. **Cross-pod backplane beyond Rust.** Go and TypeScript have the full `attach`/`detach`/`publish` surface but only an in-memory implementation; **Python has `attach`/`detach` with no `publish`**; **C# has no backplane at all**. Redis/NATS adapters exist only for Rust. Until a language has one, treat its server as single-replica.
2. **Durable backend beyond Rust.** Per-language Temporal packages (the seam is already in place, so this is additive and needs no engine change).
3. **`seq` is under-specified in the shared schema.** [`spec/extension/methods/event.schema.json`](spec/extension/methods/event.schema.json) marks `seq` optional (`required: ["event", "context"]`), but **all five** engines always emit it on an `event` frame, and all five correctly omit it on the out-of-band `events_lost` marker. The implementations agree with each other and the schema is looser than all of them — so a sixth implementation could legally omit `seq`, pass conformance, and break the gap-detection that `events_lost` depends on. Tighten the schema to match the implementations rather than the reverse.

---

## 5. How to re-verify this document

Nothing here should be taken on trust — including from this file. Each claim maps to a grep:

```bash
# Engine capability present in all five? (example: the NarcHook)
rg -l 'NarcHook'  rust/smooth-operator-core/src typescript/core/src \
                  python/core/src go/core dotnet/core/src   # in smooth-operator-core

# Backplane surface per language (in this repo)
rg -n 'attach|detach|publish' go/server/backplane.go \
    typescript/server/src/backplane.ts \
    python/server/src/smooth_operator_server/backplane.py \
    rust/smooth-operator/src/backplane.rs
rg -ril backplane dotnet/                    # expect: no matches

# Durable backend: expect matches only under rust/
find . -iname '*temporal*' -not -path '*/target/*' -not -path '*/node_modules/*'
```

Last verified against `main` on 2026-08-17.
