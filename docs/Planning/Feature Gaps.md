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

### G1. Knowledge ingestion + connectors — ✅ seam, mock, and 3 connectors shipped; the SaaS long tail remains
Mature platforms ship 50+ connectors (confluence, jira, github, gmail, google_drive, notion, salesforce, sharepoint, slack, zendesk, web, …) + a `mock_connector` for testing. We have only manual/seeded knowledge.
- **TDD**: define a `Connector` trait (`async fn pull(&self, since) -> Stream<Document>`). Write `tests/connector_contract.rs` against a **`MockConnector`** first (asserts the ingest→chunk→embed→store pipeline lands documents in the `StorageAdapter` knowledge slice + they're retrievable). Then implement the trait + 2–3 real connectors (web, file, github) each with an `external_dependency`-gated test mirroring that split.
- ✅ **Done — `rust/ingestion` (`smooai-smooth-operator-ingestion`).** The seam is `Connector { fn name(); async fn pull(&self, since: Option<Timestamp>) -> Result<Vec<RawDocument>> }` (`src/connector.rs`), driven by `ingest(connector, chunker, embedder, knowledge, options)` (`src/pipeline.rs`): pull → chunk → embed → `KnowledgeBase::ingest`, idempotent on `(document id, content hash)` via an `IngestLedger` so a re-run stores nothing new. Shipped connectors: **file**, **web**, **github** (`src/connectors/`), plus the credential-free `MockConnector`. Background / incremental re-indexing (per-connector cursor + per-run status) is `src/indexing.rs`. The contract test is **`tests/ingestion_contract.rs`** (not `connector_contract.rs` as sketched above); the GitHub connector runs fully offline against a `wiremock` server (`tests/github_connector.rs`). See [[Ingestion]] + [[Connectors]].
- ✅ **ACLs survive ingestion, connector-agnostically.** A connector's `RawDocument::acl` propagates through the chunker and is written as a structured `DocAcl` (under `DocAcl::ACL_METADATA_KEY`) — the ingest half of the G3 chain. Enforcement is **adapter-side, not opt-in**: Postgres parses that key at ingest into the `knowledge_vectors.acl` column and filters in SQL, DynamoDB into an `acl` attribute post-filtered at read, and `InMemoryStorageAdapter` wraps its knowledge slice in an `AclKnowledgeStore` whose `knowledge()` returns the ACL-recording ingest handle. So the pipeline writing that metadata is the single load-bearing link every backend depends on — which is what the test below fences. `tests/ingestion_contract.rs::ingested_acls_gate_retrieval_for_every_connector` fences it at the **pipeline** seam (a doc ingested for `group-eng` is unreadable by `group-fin` and by anonymous, while a no-ACL doc stays org-public), so the guarantee no longer rides on the GitHub-specific test alone and cannot be deleted with any one connector. Every negative assertion is paired with the entitled-principal positive control, so an empty run cannot satisfy it vacuously.
- ⚠️ **Deviation from the sketch above: `pull` returns `Vec<RawDocument>`, not `Stream<Document>`.** A `Vec` materializes the whole source per pull. That is fine for the three shipped connectors and wrong for a large Confluence space or Jira project — which is precisely what the next connectors are. Re-shaping `pull` into a stream (or a paged `pull_page(cursor)`) is a **breaking change to the trait**, so it should land *before* the SaaS connectors, not after them.
- **Remains**: the SaaS long tail — confluence, jira, notion, slack, zendesk, google_drive, salesforce, sharepoint. Confluence and Jira are the hardest shape (deep pagination, incremental `since`, per-document permissions) and should be designed against first; their per-document permissions must map onto `RawDocument::acl` the way `GithubConnectorConfig::acl_groups` already does, or ingesting them reopens G3.

### G2. Document processing / chunking pipeline
Mature knowledge platforms have a tested chunking + metadata-extraction pipeline. Our knowledge store assumes pre-chunked text.
- **TDD**: `tests/chunking.rs` first — feed a long doc + assert chunk count, overlap, boundary rules, metadata propagation, and that oversized items spill correctly. Then implement the chunker the connectors feed.

### G3. Access control / permissions (document-level) — ✅ enforced on the live chat path
Mature knowledge platforms sync per-connector permissions and filters retrieval by user entitlement. We filter by `organizationId` only.
- **TDD**: `tests/access_control.rs` first — seed docs with ACLs for users A/B; assert a query as user B never returns A-only docs (the **cross-tenant/cross-user leak** test, the highest-severity class). Then add an ACL column + retrieval filter to every adapter; run the test against Postgres + DynamoDB.
- ✅ **Done + the live-path hole closed.** The ACL layer existed but was **dead on the live chat path** (the #1 adversarial-review finding): the streaming runner queried `storage.knowledge()` raw, so a private GitHub repo was retrievable by *any* chat user. Closed by: (a) a `StorageAdapter::knowledge_for_access(&AccessContext)` seam the chat runner reads through for **both** the auto-injected context and the `knowledge_search` tool (server **and** lambda); (b) durable ACL persistence — a Postgres `knowledge_vectors.acl` column filtered **in SQL**, and a DynamoDB `acl` attribute post-filtered — so the ACL survives the ingest→serve process boundary (the in-memory side table can't); (c) `/ws` auth (bearer token → `Principal` → `AccessContext`, **fail closed** to org-public when absent) with **groups** now parsed from the JWT so a user can match a `github:owner/repo` doc ACL. Headline leak test: `smooth-operator-server/tests/acl_chat_leak.rs`; persistence: `adapters/postgres/tests/acl_persistence.rs`. Also fixed a sibling **cross-org admin leak** (`/admin/indexing/runs` + `/admin/document-sets` were global registries) — now org-keyed. See [[Access Control]] + [[Admin API]].

### G4. Answer- & search-quality regression suite (formalize the eval layer) — ✅ both halves shipped
Mature knowledge platforms have a `regression/` layer + nightly LLM-provider-chat. We're adding LLM-judge evals — formalize it.
- **TDD**: grow `rust/evals` into the regression layer — a fixed scenario set with rubric thresholds (grounding, **anti-hallucination/honest-don't-know**, tool-use appropriateness, multi-turn reasoning), plus a **retrieval-quality** eval (seed a corpus, assert recall@k / MRR on labeled queries — deterministic, no LLM). Add a `nightly` CI job that runs the judged evals across models. Track score history to catch regressions.
- ✅ **Half 1 — deterministic search quality, gating every PR.** `rust/evals/tests/retrieval_quality.rs` seeds a **frozen 20-document corpus** (`src/corpus.rs`) through the *real* ingest→chunk→embed→store pipeline and runs a **frozen 20-query labeled set** through the *real* `KnowledgeSearchTool`, scoring **recall@3 / recall@5 / MRR** against hand-written constants (0.90 / 0.95 / 0.90 against a measured 0.975 / 1.000 / 0.975). **Deliberately ungated** — no `SMOOTH_AGENT_E2E`, no `#[cfg(feature)]`, no `#[ignore]` — because a gated suite that prints `ok. 0 passed` is a suite that did not run (§4.5). Its **sensitivity is itself under test**: four permanent degradation tests break one real stage each and assert the metrics fall through the gate — half the corpus dropped (recall@3 0.500), 48-char chunking (MRR 0.842), first-paragraph-only extraction (0.775 / 0.717), and a reranker with its comparator reversed (0.325 / 0.250). The corpus is built for that sensitivity: the first draft of 13 unrelated documents scored a **perfect recall@3 with every degradation still passing**, so it was rebuilt around near-duplicate distractors with half the queries targeting facts in a document's *second or third* paragraph. See [[Evals]].
- ✅ **Half 2 — judged regression layer + nightly.** Every `Scenario` now declares a typed `Competency` (a required field, not a name→competency lookup table that would drift silently), and `tests/regression.rs` runs all 15 scenarios from both suites, rolls them into a per-competency `Scorecard`, and asserts each competency's own floor — anti-hallucination and safety at 4.0, grounding/tool-use/tone at 3.5, multi-turn at 2.5 (a floor that *describes* the known cross-turn-memory gap rather than hiding it). `.github/workflows/nightly-evals.yml` sweeps a matrix of agent models via the new `SMOOTH_AGENT_EVAL_MODEL`, appends each night's scorecard to a cached `eval-history.jsonl` rendered as a trend table, and **cannot go green on a skip**: `SMOOTH_AGENT_EVALS_REQUIRED=1` turns a missing credential into a hard failure, and the gate is the `cargo test` exit code — nothing parses a log line, and `CARGO_TERM_COLOR: never` is set regardless. **Prerequisite:** the `SMOOAI_GATEWAY_KEY` repository secret must be added; until then the nightly job fails loudly at its preflight step.

### G5. Frontend e2e (Playwright) for the chat widget — ✅ running in CI
Mature platforms ship extensive web + Playwright suites.
- **TDD**: a Playwright spec first — load the widget against a locally-booted `smooth-operator-server`, send a message, assert streamed assistant tokens render + a grounded answer appears. Wire it into the widget repo's CI.
- ✅ **Done — and the interesting part was that the specs already existed but had never run.** `SmooAI/chat-widget` had 6 spec files; its `ci.yml` ran typecheck/unit/build and **never invoked `test:e2e`**, so the suite was standing coverage that had never executed. Run for the first time, **2 of 8 were red**: `repro-stream-mock.spec.ts` mocked `WebSocket` but not `fetch`, so on mount the widget POSTed the **real production** `/internal/resume-by-fingerprint`, got a 403, and the console error tripped its own page-error assertion — a "mock" spec that depended on prod being reachable. Fixed with a `page.route` stub.
- **Streaming had no real guard.** The existing spec asserted only the *final* assistant text, which a widget that ignores every `stream_token` frame and paints the `eventual_response` blob also satisfies — confirmed by deleting streaming from the mock and watching it stay green. `e2e/streaming.spec.ts` closes it: the mock **withholds `eventual_response`** and all assertions sample inside that window, so only stream tokens can have produced on-screen text; it asserts incremental, monotonic growth to the full reply.
- **CI split so a missing secret cannot look like a pass**: `ci.yml` → `E2E (credential-free)` runs the hermetic specs on every PR (9 tests, ~6s); `e2e-live.yml` (nightly) builds a lean in-memory `smooth-operator-server` (`--no-default-features`) with `SMOOTH_AGENT_SEED_KB=1` and asserts the grounded **17-day return window** answer, **failing loudly** when `SMOOAI_GATEWAY_KEY` is absent rather than skipping green. `SMOOTH_AGENT_SERVER_BIN` replaced a hardcoded developer-local `shared-target` path that had never existed on a runner. (SmooAI/chat-widget#46)
- **Lesson worth generalising**: a suite that has never run is not coverage. `grep -c` on a spec file counts *attempts*, not passes — the only proof is a CI job whose green depends on those assertions executing.

### G6. Deployment-integration tests in CI — ✅ shipped
- ✅ **Done.** `.github/workflows/pr-kind-deploy-smoke.yml` runs the planned `kind` job on every PR: `helm install` into an ephemeral cluster, then the protocol smoke against the live pod. It is a required-looking check on current PRs (observed green on #526, 2026-08-22).
- This entry read "we only `helm lint`/`helm template`" for some time after the job shipped. A gap doc that is stale in the CLOSED direction is worse than one that is merely incomplete: it argues for work that already exists. When you close a gap here, edit this file in the same PR.

### G7. Multi-tenancy — ✅ isolation is now a test, and closing two live leaks
Mature knowledge platforms support multi-tenant schemas. Our org scoping is row-level only.
- **TDD**: `tests/multitenancy.rs` first — two orgs, assert full isolation across conversations/knowledge/checkpoints on both adapters. (Likely already passes for OLTP via `organizationId`; the test makes it a guarantee and covers the knowledge/S3-Vectors index-per-org path.)
- ✅ **Done — and the "likely already passes" framing above was wrong twice.** One
  **shared** suite (`rust/adapters/multitenancy_suite.rs`, `#[path]`-included by
  each adapter's `tests/multitenancy.rs`) runs the same body against **in-memory,
  Postgres and DynamoDB**, with a positive control on every isolation assertion so
  a backend that returns nothing can't pass vacuously. A second suite
  (`smooth-operator-server/tests/multitenancy.rs`) drives the real
  `handler::handle_frame` from an attacker authenticated to another org.

  **Now guaranteed by a test, on all three backends:**
  - conversation listings are org-partitioned, by-org and by-org-and-user — asserted with the **same user email owning one conversation in each org**, so the isolation cannot be incidentally coming from a differing owner;
  - the **idempotency claim is per-org** (`(organization_id, idempotency_key)`): two orgs using the same key get two distinct conversations, where an org-blind claim would have handed org B **org A's conversation row**;
  - message pages, participants and sessions ride their own org's conversation, and a session update in one org does not touch the other's row;
  - knowledge retrieval bound to org B never returns org A's document — *including* documents ingested through the org-blind `knowledge()` handle, which must land in the tenant their `org_id` metadata names;
  - checkpoints saved under one agent id are invisible under another.

  **What it found (both fixed in the same PR, each proven by reverting the fix and re-running):**
  1. 🚨 **Cross-tenant session access on every by-id path.** Org was resolved per
     connection only to *stamp* new sessions; `may_read_conversation` checked the
     **owner email** and never the org, and its deliberate ownerless-is-open rule
     is exactly the widget's default state. An attacker authenticated to org B who
     learned an org-A session id could read the session, replay its history through
     a turn, retitle the conversation, and resume it — minting a session bound to
     the victim's org, which flows into the turn's `ToolProviderContext`. The
     **Lambda transport had no check at all**. Fixed at the `scoped_session` /
     `may_read_conversation` chokepoints (and the Lambda's `get_session` /
     `send_message`), denying indistinguishably from not-found.
  2. 🚨 **Knowledge was not tenant-isolated where the backend isn't
     org-partitioned.** `AclKnowledgeStore` filtered by user/group only, assuming
     the wrapped store had already done the org filter — true for Postgres/DynamoDB,
     false for the in-memory adapter and any adapter using the
     `knowledge_for_access` trait default. Compounding it, `POST
     /admin/connectors/{id}/index` ingested through the org-blind `knowledge()`
     handle for every tenant (same shape as G3: the seam existed, one caller went
     around it), so on Postgres every connector document was written with
     `organization_id = NULL` — invisible to every org-scoped read. The ACL store now
     records each document's org and enforces the tenant boundary before the ACL;
     DynamoDB honours `AccessContext::organization_id` for its query partition
     (Postgres already did); both backends prefer the document's own `org_id` at
     ingest; and the admin run goes through the org-bound seam.

  **Still NOT guaranteed — merely true today (residuals, deliberately not dressed up):**
  - `StorageAdapter`'s by-id reads (`get_conversation`, `get_message`, `get_session`, `list_participants_by_conversation`) take **no org** and are not org-checked at the adapter. Enforcement lives at the caller; a new caller can still forget.
  - A connection with **no** verified org (anonymous / tokenless — the widget's normal state) is not org-checked at all. Fail-closing it would deny the widget its own session; a deployment needing hard isolation must require auth (`strict_auth`).
  - A conversation with **no participants yet** (the create→first-frame race) has no derivable org, so the conversation-id path falls through to the ownership check. The session-id path is unaffected — a `Session` always carries its org.
  - `CheckpointStore` has **no org dimension** at all: isolation rests entirely on agent-id uniqueness (the server mints a fresh UUID per `Agent`, and never reads checkpoints back). A host that reuses a stable agent id across tenants would commingle conversation state.
  - **S3 Vectors is UNVERIFIED.** The index-per-org path is behind the `s3-vectors` feature and needs real AWS; the suite exercises the brute-force DynamoDB backend only.

### G8. Model-server parity (embedding/rerank) — ✅ rerank stage shipped
Mature knowledge platforms have a dedicated, tested model server (embeddings + rerank + intent). We have a pluggable `Embedder` + RRF; the rerank stage is now implemented as a pluggable seam mirroring the `Embedder` pattern.
- **TDD (done)**: the `Reranker` trait (`smooth_operator::rerank`) ships `NoopReranker` (identity default) + `LexicalReranker` (deterministic, network-free) + the production **`GatewayReranker`** (adapter crate, alongside `GatewayEmbedder`): a Cohere/Voyage-style `/v1/rerank` cross-encoder over the SmooAI gateway, key from `SMOOAI_GATEWAY_*`. It reorders candidates by returned relevance, truncates to `top_k`, and falls back to input order on any API error (never panics, never drops the turn). A `RerankBackend` seam lets unit tests inject a stub so reorder/truncate/error-fallback are exercised offline (mirrors `GithubSearchBackend`). The server's `build_reranker` selector (mirrors `build_embedder`) picks gateway-when-keyed / lexical / noop from `SMOOTH_AGENT_RERANK`, defaulting **off** so existing behavior is unchanged. Wired into the retrieval path via `KnowledgeSearchTool::with_reranker(...)` (over-fetch → rerank → truncate) in both the reference server and the lambda. A live test is gated on `SMOOTH_AGENT_E2E=1` + a real `/v1/rerank` route (`#[ignore]`).

### G9. Connector mock + external-dependency split (test infra) — ✅ mock shipped; the tier split is convention, with no nightly running it
Formalize the platform.s `mock_connector` + `external_dependency_unit` vs `unit` split so connectors are testable credential-free in CI and fully nightly.
- **TDD**: ship the `MockConnector` (G1) and a CI convention: `unit` (no creds, every PR) vs `external` (gated, nightly), matching our `SMOOTH_AGENT_E2E` gate.
- ✅ **Done (the mock + the credential-free tier).** `MockConnector` (`src/connector.rs`) is the fixture behind the ingestion contract test. Every ingestion test in CI is credential-free and runs on **every PR**: GitHub goes through `wiremock`, embeddings through `DeterministicEmbedder`, files through `tempfile`. The one test that touches the network (`connectors::web` live fetch) is `#[ignore]` **and** gated on `SMOOTH_AGENT_E2E=1`, and skips loudly rather than passing silently.
- **Remains**: the **nightly half of the split**. `.github/workflows/rust.yml` has no `schedule:` trigger, so the gated `external` tier is currently a convention that nothing ever executes — a live-API break (auth change, response-schema change, rate limit) in the web or GitHub connector is invisible until a user hits it. Wiring a scheduled job that supplies creds and runs the `#[ignore]`d tests is what actually closes G9.

## 4. TDD working agreement (applies to all of the above and beyond)

1. **Red first.** No feature lands without a test that failed before it. PRs show the test in the same commit/PR as the code.
2. **Match the layer to the dependency.** Pure logic → `unit`; real backend → testcontainers conformance; real LLM/connector → gated `external`/`e2e` (skips credential-free, runs nightly); quality → `evals`/`regression` with rubric thresholds.
3. **One conformance suite, every backend.** New `StorageAdapter` capability is added to the shared conformance test and must pass on in-memory + Postgres + DynamoDB.
4. **Cross-language parity.** Protocol changes update `spec/` + the shared fixtures first; every client regenerates and must validate them.
5. **Gated, never skipped silently.** External/LLM tests `skip` (not pass) without creds and log why; nightly CI supplies creds.

## 5. Suggested next TDD increments (priority order)
1. ~~**G3 access-control leak test** (highest severity) → ACL filter on all adapters.~~ ✅ shipped, including the live-path hole — see §G3.
2. ~~**G1 `MockConnector` + ingestion-pipeline contract test** → connector trait + web/file/github connectors.~~ ✅ shipped — next in this line is the `pull` streaming/pagination decision, then Confluence + Jira.
3. ~~**G4 retrieval-quality eval** (deterministic recall@k) alongside the LLM-judge evals.~~ ✅ shipped — the deterministic recall@k/MRR gate runs ungated on every PR, and the judged suites are now a per-competency `regression` layer with a nightly job; see §G4. Remainder: the `SMOOAI_GATEWAY_KEY` repo secret the nightly needs, and scoring the dense path (pearl `th-15a147`).
4. **G5 widget Playwright e2e**, then **G7** (multi-tenancy), plus the specific remainders of **G2** (see §G2) and **G9** — a `schedule:` job that actually runs the gated `external` tier; the mock and the credential-free tier are done. (G1, G3, G6 and G8 are done.)

Tracked against the [[Roadmap]]; these become Phase 4 (tools/ingestion), Phase 6 (deploy CI), and a new **Phase 10 — connectors & quality regression**.

---

**In this vault:** [[Home]] · [[Roadmap]] · [[Access Control]] · [[Reranking]] · [[Evals]]
