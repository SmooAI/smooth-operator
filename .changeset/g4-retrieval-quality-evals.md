---
'@smooai/smooth-operator': patch
---

Add a deterministic search-quality regression suite and formalize the judged evals into a scored regression layer (feature gap G4).

**The half that gates CI.** `rust/evals` now ships a retrieval-quality eval that needs no LLM, no key, and no network: a frozen 20-document corpus is seeded through the real ingest→chunk→embed→store pipeline, a frozen 20-query labeled set runs through the real `knowledge_search` tool, and the ranked results are scored with recall@3, recall@5, and MRR against hand-written thresholds. It is deliberately ungated — no `SMOOTH_AGENT_E2E`, no feature flag, no `#[ignore]` — so it runs on every PR and catches a chunker change, an embedder swap, or a rerank bug the day it lands.

Four permanent degradation tests prove the suite can actually go red: half the corpus dropped, 48-char chunking, first-paragraph-only extraction, and a reranker with its comparator reversed each breach the thresholds the gate enforces.

**The judged half.** Every eval scenario now declares a typed `Competency` (grounding, anti-hallucination, tool use, multi-turn reasoning, safety, tone), and a new `regression` suite rolls all 15 scenarios up into a per-competency `Scorecard` with its own floor — so a drop in grounding no longer averages away against a rise in tone. `SMOOTH_AGENT_EVAL_MODEL` lets the agent model be swept, and `SMOOTH_AGENT_EVALS_REQUIRED=1` turns "skipped for want of credentials" into a hard failure.

**Nightly CI.** `.github/workflows/nightly-evals.yml` runs the judged suite across a model matrix, appends each night's scorecard to a cached score history, and renders the trend into the job summary. It fails loudly when the gateway key is missing rather than reporting a green no-op, and nothing in it parses a test log.
