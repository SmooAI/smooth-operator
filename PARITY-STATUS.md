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
| Durable-execution **backend** (Temporal: crash-safe resume, durable HITL signals, durable-wait timer) | ✅ all five [^durable] |

### Why the claim holds

Three shared corpora are generated **from the Rust reference** and replayed by all five engines — Rust included, so the reference cannot drift away from its own ports unnoticed:

- [`spec/evals/scenarios.json`](https://github.com/SmooAI/smooth-operator-core/blob/main/spec/evals/scenarios.json) (core repo) — LLM-as-judge eval scenarios (same ids, same count, ratcheted against deletion).
- [`spec/narc/corpus.json`](https://github.com/SmooAI/smooth-operator-core/blob/main/spec/narc/corpus.json) (core repo) — the secret/injection detection set, pinned pattern-by-pattern **and severity-by-severity**. A port that downgrades a `BLOCK` to an `ALERT` fails its own suite.
- [`spec/providers/routing.json`](https://github.com/SmooAI/smooth-operator-core/blob/main/spec/providers/routing.json) (core repo) — every preset slot's resolved model, base URL, key and wire format.

On the service side, all five servers replay [`spec/conformance/scenarios`](spec/conformance) — in **this** repo — against the engine's deterministic mock, so they must emit identical protocol output.

[^durable]: The durable backend is a separate, **optional, feature-gated per-language package** — Rust's `smooth-operator-temporal` crate plus new Go / TypeScript / Python / .NET packages ([core-repo PRs #170, #168, #169, #173](https://github.com/SmooAI/smooth-operator-core/pulls)) — each with a client-side `AgentExecutor` that starts the Temporal workflow, durable HITL via approve/deny signals, and a durable-wait timer, e2e-tested against a real ephemeral Temporal server. The server-side selection seam (env `SMOOTH_AGENT_DURABLE_EXECUTOR`, backend **dependency-injected** so the published server keeps no hard Temporal dep) is in all five servers ([this repo PRs #450, #451, #452, #455](https://github.com/SmooAI/smooth-operator/pulls)); no engine pulls a Temporal SDK into your dependency tree by default. **Shared ADR-030 follow-up — the same in every language, Rust reference included, *not* a Rust-vs-others gap:** the durable path yields only a terminal result (no token-delta streaming) and reports `costUsd=0` on the workflow result, and the executor seeds from agent config only (no prior-thread history / per-turn-per-org tool registry). This is the remaining "workflow→streaming adapter bridge" product question.

---

## 2. Rust-first — the engine's one honest exception

| Surface | Status |
| --- | --- |
| **Extension sandbox / integrity hardening** | **Rust-first.** Capability *declarations* are parsed and honoured in all five, but process-level confinement and manifest-integrity verification are Rust-only. See [Extension-Sandboxing-Design.md](https://github.com/SmooAI/smooth-operator-core/blob/main/docs/Extension-Sandboxing-Design.md). |

The **durable-execution backend** used to sit here as the second Rust-first exception; it no longer does. The Temporal backend ships as an optional per-language package in all five, and the server-side selection seam is in all five servers — see the durable backend row in §1 and its footnote for the one shared ADR-030 streaming/cost follow-up that remains (identical across all five, not a parity gap).

---

## 3. The service — depth is uneven, and this is the part to read before picking a language

All five servers carry the transport core: frame dispatch, per-turn engine, sessions, auth, graceful drain, and a Postgres store over the same conversation/message/participant/session/settings tables. Past that:

| | Rust | C#/.NET | Go | TypeScript | Python |
| --- | --- | --- | --- | --- | --- |
| Transport core (dispatch · sessions · auth · drain) | ✅ | ✅ | ✅ | ✅ | ✅ |
| Shared scenario conformance corpus | ✅ | ✅ | ✅ | ✅ | ✅ |
| Postgres conversation store | ✅ | ✅ | ✅ | ✅ | ✅ |
| Server `gen_ai.*` OTel telemetry (chat + tool spans · redacted tool args · env-gated OTLP) | ✅ | ✅ | ✅ | ✅ | ✅ |
| Second storage backend (DynamoDB + S3 Vectors) | ✅ | — | — | — | — |
| Persistent checkpoint / knowledge / ACL-knowledge stores [^knowledge] | ✅ | ✅ | ◐ | ◐ | ◐ |
| Deep ingestion + ACL surface | ✅ | ✅ | ◐ | ◐ | ◐ |
| Backplane `attach`/`detach` | ✅ | — | ✅ | ✅ | ✅ |
| Backplane `publish` (event fan-out) | ✅ | — | ✅ | ✅ | — |
| **Cross-pod backplane (Redis / NATS)** | ✅ | — | — | — | — |

[^knowledge]: The durable **knowledge + ACL-knowledge** Postgres stores now ship in **Go, TypeScript and Python** too ([PRs #442, #443, #444](https://github.com/SmooAI/smooth-operator/pulls)) — on the shared `knowledge_vectors` table + pgvector — alongside Rust and .NET (which already had them, e.g. Python's `postgres_knowledge.py`: `PostgresVectorKnowledge` / `PostgresAclKnowledge`). These three cells stay **◐ rather than ✅** for two honest reasons: the persistent **checkpoint** store is still pending in Go/TS/Python, and the **TS + Python** knowledge stores are shipped and contract-tested but **not yet wired into the live dispatcher** (a sync-engine vs async-pg bridge). The **second storage backend** (DynamoDB + S3-Vectors, the row above) remains Rust-only.

**The operational consequence:** only the **Rust** server scales past one replica today. Go, TypeScript and Python run an in-memory backplane — correct for a single process, silently wrong the moment you run two pods, because an event published on pod A never reaches a socket held by pod B. C# has no backplane surface at all.

---

## 4. Known-remaining

Three items, in the order they'd bite:

1. **Cross-pod backplane beyond Rust.** Go and TypeScript have the full `attach`/`detach`/`publish` surface but only an in-memory implementation; **Python has `attach`/`detach` with no `publish`**; **C# has no backplane at all**. Redis/NATS adapters exist only for Rust. Until a language has one, treat its server as single-replica.
2. **Durable path → streaming/cost adapter bridge (ADR-030) — done shipping, one shared follow-up left.** The Temporal backend now ships as an optional per-language package in **all five** and the server-side selection seam is in **all five** servers (§1 durable backend row + footnote), so this is no longer a language gap. What remains is identical across every language, Rust reference included: the durable path returns only a terminal result (no token-delta streaming) and reports `costUsd=0` on the workflow result, and the executor seeds from agent config only. Bridging the workflow result back onto the streaming/cost surface is the open ADR-030 product question.
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

# Durable backend: temporal packages now ship in all five (rust/ go/ typescript/ python/ dotnet/),
# and the server-side selection seam (SMOOTH_AGENT_DURABLE_EXECUTOR) is in all five servers.
find . -iname '*temporal*' -not -path '*/target/*' -not -path '*/node_modules/*'
rg -n 'SMOOTH_AGENT_DURABLE_EXECUTOR' rust/ go/ typescript/ python/ dotnet/

# Server gen_ai OTel telemetry present in all five servers? (chat + tool spans)
rg -n 'gen_ai\.chat|gen_ai\.tool' rust/ go/ typescript/ python/ dotnet/
```

Last verified against `main` on 2026-08-17.
