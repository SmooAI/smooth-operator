# LLM-as-Judge Evaluation Harness

The `evals` crate (`smooai-smooth-operator-agent-evals`, at `rust/evals/`) is a
quality-scoring harness for the reference agent. Where the core crate's
end-to-end tests assert on substrings ("the reply contains `17`"), this harness
asks a **second LLM — the judge — to score the *quality* of the agent's behavior
against a written rubric**. That catches what substring checks can't: did the
agent hallucinate a CEO name? did it ground its answer in the retrieved fact
rather than its own priors? did it reason correctly over two turns?

It runs the **real** smooth-operator agent (via `KnowledgeChatRuntime`) against
the **live** OpenAI-compatible gateway at `https://llm.smoo.ai/v1` using the
cheap `claude-haiku-4-5` model — no mocks on the agent path.

## Layout

| File | Purpose |
| --- | --- |
| `rust/evals/src/lib.rs` | The harness: `Scenario`, `JudgedResult`, `JudgeConfig`, `run_scenario`, `parse_verdict`, `default_scenarios`. |
| `rust/evals/tests/llm_judge.rs` | Gated live-gateway integration test that runs the whole suite and asserts on the aggregate. |

## How a scenario is judged

`run_scenario(scenario, config)`:

1. Builds a `KnowledgeChatRuntime` over the in-memory adapter and seeds the
   scenario's KB documents.
2. Runs every user turn against the live gateway, capturing the agent's final
   reply and whether `knowledge_search` fired (from `TurnOutcome`).
3. Calls the **judge** model (a separate, raw `LlmClient` chat completion) with a
   rubric prompt containing: the rubric, the ground-truth facts, the user
   conversation, whether a tool fired, and the agent's reply. The judge must
   return strict JSON `{ "score": 1-5, "pass": bool, "reasoning": "..." }`.
4. Parses the verdict robustly (`parse_verdict` extracts the first `{ … }` span,
   tolerating ```json fences / prose, and validates the 1–5 range). On a parse
   failure it retries the judge call **once** with a terse JSON-only nudge.
5. Returns a `JudgedResult` with the score, reasoning, and whether the score met
   the scenario's `pass_threshold`.

## The scenarios

`default_scenarios()` returns five, each exercising a distinct competency. The
KB seeds a deliberately unusual number — **17-day** return window — so a generic
ungrounded answer can't accidentally match.

| Scenario | Competency | Rubric (abridged) |
| --- | --- | --- |
| `grounded_answer` | Grounding | Correctly states the 17-day window **and** invents no extra policy details. |
| `honest_no_knowledge` | **Anti-hallucination** | Asked for the CEO's name (not in KB) — must say it doesn't know, **without inventing a name**. Any named person = fail. |
| `tool_use_supported_answer` | Tool-use appropriateness | Answer (standard shipping 5–7 business days) must be well-supported by retrieved knowledge. |
| `multi_turn_coherence` | Cross-turn reasoning | Turn 1 gives a delivery date (the 5th); turn 2 asks the last return day. Correct = the 5th + 17 days = the 22nd. |
| `tone_helpfulness` | Tone / helpfulness | Reply must be clear, courteous, helpful, and not fabricate a warranty/repair policy. |

## Threshold strategy

Same-model judging carries real run-to-run variance: a borderline reply can
score 4 on one run and 3 on the next. To keep one judge-variance blip from
reddening the whole suite while still catching real regressions, the test
asserts on the **aggregate mean (≥ 4.0)** and logs any scenario below its own
threshold rather than hard-failing per scenario. Every per-scenario score +
reasoning prints under `--nocapture`, so misses are always visible.

This is not just variance insurance — on the first live run it surfaced a **real
behavioral limitation**: `multi_turn_coherence` scored **1/5** because
`KnowledgeChatRuntime` does not yet wire cross-turn memory (each `run_turn`
builds a fresh `Agent` with a new id, so turn 2 has no recollection of turn 1's
delivery date — the same gap documented in
`smooth-operator-agent-core/tests/e2e_llm_smoo_ai.rs`). The judge caught it; a
substring check would not have. The aggregate stayed ≥ 4.0 (4.20), so the suite
passes while loudly logging the miss for follow-up.

## Same-model-judging limitation & the judge-model knob

By default the **agent and judge are the same model** (`claude-haiku-4-5`). A
model grading output from its own family is a known weak spot — it tends to be
lenient toward its own phrasing and shares blind spots. For a more adversarial
grade, point the judge at a different/stronger model:

```sh
export SMOOTH_AGENT_JUDGE_MODEL=claude-sonnet-4-5   # judge only; agent stays haiku
```

`JudgeConfig::from_key` reads this env var; when unset the judge defaults to the
agent model and the test prints a NOTE flagging the limitation.

## Secret handling & gating

- The gateway key is read from `SMOOAI_GATEWAY_KEY` and **never printed**.
- The harness is gated: `llm_judge.rs` is a no-op (prints a skip, returns) unless
  **both** `SMOOTH_AGENT_E2E=1` and a non-empty `SMOOAI_GATEWAY_KEY` are set. So
  `cargo test` with no env stays green, and CI without credentials stays green.
- The five `parse_verdict` / scenario-shape unit tests in `lib.rs` run with no
  network and no key.

## Running it

```sh
# Load the key WITHOUT printing it, opt in, and run the suite single-threaded.
export SMOOAI_GATEWAY_KEY=$(python3 -c \
  "import json;print(json.load(open('$HOME/.local/share/opencode/auth.json'))['smooai']['key'])")
export SMOOTH_AGENT_E2E=1
cargo test -p smooai-smooth-operator-agent-evals --test llm_judge \
  -- --nocapture --test-threads=1
```

Token usage is kept modest: terse prompts, agent `max_tokens=512`, judge
`max_tokens=300`, `temperature=0.0`.
