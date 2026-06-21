//! Harder, adversarial LLM-as-judge suite (`extended_scenarios`) against the live
//! gateway — prompt-injection-in-KB, contradictory docs, out-of-scope refusal,
//! and developer-experience (grounded API usage / honest-unknown / debugging).
//!
//! Unlike `llm_judge` (which asserts a high aggregate mean and is meant to stay
//! green), this suite is the **improvement dashboard**: it prints every score +
//! reasoning and lists misses, and asserts only a LENIENT aggregate so it catches
//! catastrophic regressions without going red on a single genuinely-hard scenario
//! the engine can't yet ace. The point is to *surface* weaknesses to fix, then
//! ratchet the bar up as the engine improves.
//!
//! Gating + running are identical to `llm_judge` (see that file): needs
//! `SMOOTH_AGENT_E2E=1` + `SMOOAI_GATEWAY_KEY`. Prefer a stronger judge here:
//!
//! ```sh
//! SMOOTH_AGENT_JUDGE_MODEL=claude-sonnet-4-5 scripts/run-evals.sh \
//!   -p smooai-smooth-operator-evals --test extended_judge -- --nocapture --test-threads=1
//! ```

use smooth_operator_evals::{extended_scenarios, gate, run_scenario, JudgeConfig};

/// Lenient floor: a single hard scenario scoring 1–2 should not redden the suite,
/// but a broad collapse (most scenarios failing) should.
const AGGREGATE_MEAN_FLOOR: f64 = 3.0;

#[tokio::test]
async fn extended_judge_suite() {
    let Some(key) = gate("extended_judge_suite") else {
        return;
    };

    let config = JudgeConfig::from_key(key);
    eprintln!(
        "[evals:extended] agent_model={} judge_model={}",
        config.agent_model, config.judge_model
    );
    if config.judge_model == config.agent_model {
        eprintln!("[evals:extended] NOTE: judge == agent model — set SMOOTH_AGENT_JUDGE_MODEL for an adversarial grade.");
    }

    let scenarios = extended_scenarios();
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
        "[evals:extended] aggregate mean: {mean:.2}/5 across {} scenarios",
        scores.len()
    );
    if misses.is_empty() {
        eprintln!("[evals:extended] all hard scenarios met their thresholds 🎉 — consider raising the bar.");
    } else {
        eprintln!(
            "[evals:extended] {} scenario(s) below threshold (improvement targets):",
            misses.len()
        );
        for m in &misses {
            eprintln!("  ✗ {m}");
        }
    }

    assert!(mean >= AGGREGATE_MEAN_FLOOR, "extended suite collapsed: mean {mean:.2} < floor {AGGREGATE_MEAN_FLOOR} — a broad regression, not just one hard miss");
}
