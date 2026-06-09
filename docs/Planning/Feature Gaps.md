# Testing & feature gap analysis (TDD plan)

A review of how mature knowledge platforms test, what it has that `smooth-operator` does not, and a **test-driven** plan to close the gaps. **Policy: every gap below is closed test-first — write the failing test, then the implementation.**

## 1. How mature knowledge platforms test

Mature knowledge platforms ship 1,000+ test files across a layered taxonomy and ~13 CI workflows.

### Backend test layers (`backend/tests/`)
| Layer | Files | What it is |
| ----- | ----- | ---------- |
| `unit` | 467 | pure unit tests, no external deps |
| `integration` | 264 | cross-component flows, spun up via docker-compose (Postgres + Vespa + Redis + model server) |
| `external_dependency_unit` | 165 | unit-ish tests that hit a real external dependency (DB, connector API), gated; run on PR + nightly |
| `daily` | 62 | longer/connector suites run on a schedule against real external services |
| `regression` | 12 | **answer-quality / search-quality regression** — the eval layer |
| `utils`/`common`/`api` | ~9 | shared fixtures + harness |

### CI matrix (the testing taxonomy, from `.github/workflows/`)
`pr-python-tests` · `pr-jest-tests` (frontend unit) · `pr-playwright-tests` (frontend e2e) · `pr-integration-tests` (compose) · `pr-database-tests` · `pr-python-connector-tests` (per-connector) · `pr-python-model-tests` (embedding/rerank model server) · `pr-golang-tests` · `pr-external-dependency-unit-tests` (+ nightly) · `nightly-llm-provider-chat` (**LLM regression across providers**) · `pr-craft-compose-tests` / `pr-craft-k8s-tests` / `pr-helm-chart-testing` (deployment) · `pr-quality-checks` (lint/format).

### Takeaways worth emulating
- **A dedicated `regression`/eval layer** that scores answer + search quality (not just substring asserts).
- **Per-connector test suites** + a `mock_connector` so ingestion is testable without live creds.
- **External-dependency tests split from pure units** and gated (PR-light, nightly-full) — exactly our "gated on `SMOOTH_AGENT_E2E`" pattern, but formalized.
- **Deployment tests in CI** (compose + k8s + helm), not just `helm lint`.
- **A nightly LLM-provider regression** that catches provider/model drift.

## 2. What we have today

Strong on correctness-of-mechanics, thin on breadth:
- **Per-crate/per-language units**: Rust 44+, TS 16, Go 26, .NET 21, Python 26.
- **Adapter conformance via testcontainers**: Postgres (pgvector/pg16), DynamoDB (dynamodb-local) — real backends.
- **Protocol conformance fixtures** (ajv-validated) shared across all 5 languages.
- **Live cross-language LLM E2E** (gated on `SMOOTH_AGENT_E2E`+key) — streaming, tool-calling, RAG grounding, per-session memory.
- **LLM-as-judge evals** (in progress) — the start of a `regression`/quality layer.

What we **lack**: ingestion/connectors, a document-processing pipeline, access-control/permissions, multi-tenancy, frontend e2e, deployment-integration tests in CI, and a formal quality-regression suite.

## 3. Gaps (baseline-has / we-don't) → TDD plan

Ordered by leverage. Each item: **write the test first (red), then implement (green)**.

### G1. Knowledge ingestion + connectors (biggest gap)
Mature platforms ship 50+ connectors (confluence, jira, github, gmail, google_drive, notion, salesforce, sharepoint, slack, zendesk, web, …) + a `mock_connector` for testing. We have only manual/seeded knowledge.
- **TDD**: define a `Connector` trait (`async fn pull(&self, since) -> Stream<Document>`). Write `tests/connector_contract.rs` against a **`MockConnector`** first (asserts the ingest→chunk→embed→store pipeline lands documents in the `StorageAdapter` knowledge slice + they're retrievable). Then implement the trait + 2–3 real connectors (web, file, github) each with an `external_dependency`-gated test mirroring that split.

### G2. Document processing / chunking pipeline
Mature knowledge platforms have a tested chunking + metadata-extraction pipeline. Our knowledge store assumes pre-chunked text.
- **TDD**: `tests/chunking.rs` first — feed a long doc + assert chunk count, overlap, boundary rules, metadata propagation, and that oversized items spill correctly. Then implement the chunker the connectors feed.

### G3. Access control / permissions (document-level) — ✅ enforced on the live chat path
Mature knowledge platforms sync per-connector permissions and filters retrieval by user entitlement. We filter by `organizationId` only.
- **TDD**: `tests/access_control.rs` first — seed docs with ACLs for users A/B; assert a query as user B never returns A-only docs (the **cross-tenant/cross-user leak** test, the highest-severity class). Then add an ACL column + retrieval filter to every adapter; run the test against Postgres + DynamoDB.
- ✅ **Done + the live-path hole closed.** The ACL layer existed but was **dead on the live chat path** (the #1 adversarial-review finding): the streaming runner queried `storage.knowledge()` raw, so a private GitHub repo was retrievable by *any* chat user. Closed by: (a) a `StorageAdapter::knowledge_for_access(&AccessContext)` seam the chat runner reads through for **both** the auto-injected context and the `knowledge_search` tool (server **and** lambda); (b) durable ACL persistence — a Postgres `knowledge_vectors.acl` column filtered **in SQL**, and a DynamoDB `acl` attribute post-filtered — so the ACL survives the ingest→serve process boundary (the in-memory side table can't); (c) `/ws` auth (bearer token → `Principal` → `AccessContext`, **fail closed** to org-public when absent) with **groups** now parsed from the JWT so a user can match a `github:owner/repo` doc ACL. Headline leak test: `smooth-operator-server/tests/acl_chat_leak.rs`; persistence: `adapters/postgres/tests/acl_persistence.rs`. Also fixed a sibling **cross-org admin leak** (`/admin/indexing/runs` + `/admin/document-sets` were global registries) — now org-keyed. See [[Access Control]] + [[Admin API]].

### G4. Answer- & search-quality regression suite (formalize the eval layer)
Mature knowledge platforms have a `regression/` layer + nightly LLM-provider-chat. We're adding LLM-judge evals — formalize it.
- **TDD**: grow `rust/evals` into the regression layer — a fixed scenario set with rubric thresholds (grounding, **anti-hallucination/honest-don't-know**, tool-use appropriateness, multi-turn reasoning), plus a **retrieval-quality** eval (seed a corpus, assert recall@k / MRR on labeled queries — deterministic, no LLM). Add a `nightly` CI job that runs the judged evals across models. Track score history to catch regressions.

### G5. Frontend e2e (Playwright) for the chat widget
Mature platforms ship extensive web + Playwright suites. Our new `SmooAI/chat-widget` has none yet.
- **TDD**: a Playwright spec first — load the widget against a locally-booted `smooth-operator-server`, send a message, assert streamed assistant tokens render + a grounded answer appears. Wire it into the widget repo's CI.

### G6. Deployment-integration tests in CI
Mature knowledge platforms run compose + k8s + helm tests in CI; we only `helm lint`/`helm template`.
- **TDD**: a `kind`-based CI job — `helm install` the chart into an ephemeral cluster with a pgvector Postgres, port-forward, run the protocol smoke (`ping`→`pong`, `create_conversation_session`) against the live pod. (Red until the image builds + chart serves — which the `SMOOTH_AGENT_BIND=0.0.0.0` fix already unblocked.)

### G7. Multi-tenancy
Mature knowledge platforms support multi-tenant schemas. Our org scoping is row-level only.
- **TDD**: `tests/multitenancy.rs` first — two orgs, assert full isolation across conversations/knowledge/checkpoints on both adapters. (Likely already passes for OLTP via `organizationId`; the test makes it a guarantee and covers the knowledge/S3-Vectors index-per-org path.)

### G8. Model-server parity (embedding/rerank) — ✅ rerank stage shipped
Mature knowledge platforms have a dedicated, tested model server (embeddings + rerank + intent). We have a pluggable `Embedder` + RRF; the rerank stage is now implemented as a pluggable seam mirroring the `Embedder` pattern.
- **TDD (done)**: the `Reranker` trait (`smooth_operator::rerank`) ships `NoopReranker` (identity default) + `LexicalReranker` (deterministic, network-free) + the production **`GatewayReranker`** (adapter crate, alongside `GatewayEmbedder`): a Cohere/Voyage-style `/v1/rerank` cross-encoder over the SmooAI gateway, key from `SMOOAI_GATEWAY_*`. It reorders candidates by returned relevance, truncates to `top_k`, and falls back to input order on any API error (never panics, never drops the turn). A `RerankBackend` seam lets unit tests inject a stub so reorder/truncate/error-fallback are exercised offline (mirrors `GithubSearchBackend`). The server's `build_reranker` selector (mirrors `build_embedder`) picks gateway-when-keyed / lexical / noop from `SMOOTH_AGENT_RERANK`, defaulting **off** so existing behavior is unchanged. Wired into the retrieval path via `KnowledgeSearchTool::with_reranker(...)` (over-fetch → rerank → truncate) in both the reference server and the lambda. A live test is gated on `SMOOTH_AGENT_E2E=1` + a real `/v1/rerank` route (`#[ignore]`).

### G9. Connector mock + external-dependency split (test infra)
Formalize the platform.s `mock_connector` + `external_dependency_unit` vs `unit` split so connectors are testable credential-free in CI and fully nightly.
- **TDD**: ship the `MockConnector` (G1) and a CI convention: `unit` (no creds, every PR) vs `external` (gated, nightly), matching our `SMOOTH_AGENT_E2E` gate.

## 4. TDD working agreement (applies to all of the above and beyond)

1. **Red first.** No feature lands without a test that failed before it. PRs show the test in the same commit/PR as the code.
2. **Match the layer to the dependency.** Pure logic → `unit`; real backend → testcontainers conformance; real LLM/connector → gated `external`/`e2e` (skips credential-free, runs nightly); quality → `evals`/`regression` with rubric thresholds.
3. **One conformance suite, every backend.** New `StorageAdapter` capability is added to the shared conformance test and must pass on in-memory + Postgres + DynamoDB.
4. **Cross-language parity.** Protocol changes update `spec/` + the shared fixtures first; every client regenerates and must validate them.
5. **Gated, never skipped silently.** External/LLM tests `skip` (not pass) without creds and log why; nightly CI supplies creds.

## 5. Suggested next TDD increments (priority order)
1. **G3 access-control leak test** (highest severity) → ACL filter on all adapters.
2. **G1 `MockConnector` + ingestion-pipeline contract test** → connector trait + web/file/github connectors.
3. **G4 retrieval-quality eval** (deterministic recall@k) alongside the LLM-judge evals.
4. **G5 widget Playwright e2e**, **G6 kind deploy smoke**, then **G2/G8/G7/G9**.

Tracked against the [[Roadmap]]; these become Phase 4 (tools/ingestion), Phase 6 (deploy CI), and a new **Phase 10 — connectors & quality regression**.

---

**In this vault:** [[Home]] · [[Roadmap]] · [[Access Control]] · [[Reranking]] · [[Evals]]
