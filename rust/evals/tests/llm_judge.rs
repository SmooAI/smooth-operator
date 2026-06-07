//! LLM-as-judge eval suite against the live `llm.smoo.ai` gateway.
//!
//! Drives the **real** smooth-operator agent (via [`KnowledgeChatRuntime`]) on a
//! set of realistic AI scenarios, then has a **judge** model score each reply
//! against a rubric. This is quality scoring, not substring matching.
//!
//! ## Gating (safe to commit, safe in CI)
//!
//! This test is a no-op unless BOTH are set:
//!   - `SMOOTH_AGENT_E2E=1`        — explicit opt-in flag
//!   - `SMOOAI_GATEWAY_KEY=<key>`  — gateway API key (never hardcoded/printed)
//!
//! With neither set, `cargo test` stays green (the test prints a skip and
//! returns). CI without credentials stays green.
//!
//! ## Running locally (does NOT print the key)
//!
//! ```sh
//! export SMOOAI_GATEWAY_KEY=$(python3 -c \
//!   "import json;print(json.load(open('$HOME/.local/share/opencode/auth.json'))['smooai']['key'])")
//! export SMOOTH_AGENT_E2E=1
//! cargo test -p smooai-smooth-operator-agent-evals --test llm_judge \
//!   -- --nocapture --test-threads=1
//! ```
//!
//! Optionally point the judge at a different (stronger) model for a more
//! adversarial grade — see the same-model-judging note in `lib.rs`:
//!
//! ```sh
//! export SMOOTH_AGENT_JUDGE_MODEL=claude-sonnet-4-5
//! ```
//!
//! ## Threshold strategy
//!
//! Same-model judging (haiku grading haiku) carries real run-to-run variance: a
//! borderline reply can score 4 on one run and 3 on the next. To keep one
//! judge-variance blip from reddening the whole suite while still catching real
//! regressions, the suite asserts on the **aggregate mean** (≥ 4.0) and logs any
//! individual scenario that fell below its own threshold rather than hard-failing
//! per scenario. Every per-scenario score + reasoning is printed under
//! `--nocapture` so misses are always visible.

use smooth_operator_agent_evals::{default_scenarios, gate, run_scenario, JudgeConfig};

/// Aggregate mean score the suite must clear.
const AGGREGATE_MEAN_THRESHOLD: f64 = 4.0;

#[tokio::test]
async fn llm_judge_suite() {
    let Some(key) = gate("llm_judge_suite") else {
        return;
    };

    let config = JudgeConfig::from_key(key);
    eprintln!(
        "[evals] agent_model={} judge_model={} (set SMOOTH_AGENT_JUDGE_MODEL to override the judge)",
        config.agent_model, config.judge_model
    );
    if config.judge_model == config.agent_model {
        eprintln!(
            "[evals] NOTE: judge and agent are the SAME model — a known leniency/blind-spot \
             limitation. Set SMOOTH_AGENT_JUDGE_MODEL for an adversarial grade."
        );
    }

    let scenarios = default_scenarios();
    let mut scores: Vec<u8> = Vec::with_capacity(scenarios.len());
    let mut misses: Vec<String> = Vec::new();

    for scenario in &scenarios {
        let result = run_scenario(scenario, &config)
            .await
            .unwrap_or_else(|e| panic!("scenario {} failed to run/judge: {e:#}", scenario.name));

        eprintln!("\n──────────────────────────────────────────────────────────");
        eprintln!("[scenario] {}", result.scenario);
        eprintln!(
            "  knowledge_search fired: {}",
            result.knowledge_search_fired
        );
        eprintln!("  agent reply: {:?}", result.agent_reply);
        eprintln!(
            "  JUDGE score: {}/5  (threshold {})  judge_pass={}",
            result.verdict.score, result.threshold, result.verdict.pass
        );
        eprintln!("  JUDGE reasoning: {}", result.verdict.reasoning);
        eprintln!(
            "  => {}",
            if result.met_threshold() {
                "MET threshold"
            } else {
                "BELOW threshold"
            }
        );

        if !result.met_threshold() {
            misses.push(format!(
                "{} scored {}/5 (< {}): {}",
                result.scenario, result.verdict.score, result.threshold, result.verdict.reasoning
            ));
        }
        scores.push(result.verdict.score);
    }

    let total: u32 = scores.iter().map(|&s| u32::from(s)).sum();
    let mean = f64::from(total) / scores.len() as f64;

    eprintln!("\n══════════════════════════════════════════════════════════");
    eprintln!(
        "[evals] aggregate mean score: {mean:.2}/5 across {} scenarios (threshold {AGGREGATE_MEAN_THRESHOLD})",
        scores.len()
    );
    if misses.is_empty() {
        eprintln!("[evals] all scenarios met their individual thresholds.");
    } else {
        eprintln!(
            "[evals] {} scenario(s) below individual threshold (non-fatal):",
            misses.len()
        );
        for m in &misses {
            eprintln!("  - {m}");
        }
    }

    assert!(
        mean >= AGGREGATE_MEAN_THRESHOLD,
        "aggregate mean {mean:.2} fell below {AGGREGATE_MEAN_THRESHOLD}; misses: {misses:?}"
    );
}
