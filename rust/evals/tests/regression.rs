//! The judged **regression layer** — per-competency rubric floors + a scorecard
//! the nightly job turns into score history (feature gap G4, second half).
//!
//! `llm_judge` asserts one aggregate mean over the default suite and
//! `extended_judge` asserts a lenient floor over the hard suite. Neither answers
//! the question a regression layer exists to answer: *which competency moved?*
//! A drop in grounding and a drop in tone average out to the same number, and
//! averaging them is how a real regression hides.
//!
//! This suite runs **every** scenario from both suites, rolls the scores up by
//! [`Competency`], and asserts each competency's own floor. It also writes a
//! machine-readable scorecard to `target/eval-scorecard.json`, which
//! `.github/workflows/nightly-evals.yml` appends to a cached history file so a
//! slow drift shows up as a trend instead of a surprise.
//!
//! ## Gating — and why a skip can be made fatal
//!
//! This suite needs a live gateway, so it is gated on `SMOOTH_AGENT_E2E=1` +
//! `SMOOAI_GATEWAY_KEY` like its siblings, and skips (loudly, with a reason) on
//! a credential-free machine.
//!
//! A gated suite that reports `ok. 0 passed` is a suite that did not run. So the
//! nightly job — the one place credentials are guaranteed — sets
//! **`SMOOTH_AGENT_EVALS_REQUIRED=1`**, which turns a skip into a hard failure.
//! The signal CI reads is the process exit code, never a parsed log line: no
//! tally, no `grep -c`, nothing an ANSI escape or a wrapped line can fool.
//!
//! ## Running it
//!
//! ```sh
//! scripts/run-evals.sh -p smooai-smooth-operator-evals --test regression \
//!   -- --nocapture --test-threads=1
//! ```

use std::path::PathBuf;

use smooth_operator_evals::{
    default_scenarios, extended_scenarios, gate, run_scenario, JudgeConfig, JudgedResult, Scorecard,
};

/// Where the scorecard lands for CI to pick up.
fn scorecard_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../target")
        .join("eval-scorecard.json")
}

/// Whether a missing credential must fail rather than skip (set by nightly CI).
fn skip_is_fatal() -> bool {
    std::env::var("SMOOTH_AGENT_EVALS_REQUIRED").as_deref() == Ok("1")
}

#[tokio::test]
async fn judged_regression_suite_meets_competency_floors() {
    let Some(key) = gate("regression_suite") else {
        assert!(
            !skip_is_fatal(),
            "SMOOTH_AGENT_EVALS_REQUIRED=1 but the judged evals could not run — set \
             SMOOTH_AGENT_E2E=1 and a non-empty SMOOAI_GATEWAY_KEY. A nightly eval job that \
             skips is a nightly eval job that is not running."
        );
        eprintln!("[eval-status] SKIPPED reason=no-credentials suite=regression");
        return;
    };

    let config = JudgeConfig::from_key(key);
    eprintln!(
        "[eval-status] RUNNING suite=regression agent_model={} judge_model={}",
        config.agent_model, config.judge_model
    );
    if config.judge_model == config.agent_model {
        eprintln!(
            "[eval-status] NOTE judge==agent model — set SMOOTH_AGENT_JUDGE_MODEL for an \
             adversarial grade."
        );
    }

    let scenarios: Vec<_> = default_scenarios()
        .into_iter()
        .chain(extended_scenarios())
        .collect();

    let mut results: Vec<JudgedResult> = Vec::with_capacity(scenarios.len());
    for scenario in &scenarios {
        let result = run_scenario(scenario, &config)
            .await
            .unwrap_or_else(|e| panic!("scenario {} failed to run/judge: {e:#}", scenario.name));
        eprintln!(
            "  [{}] {} → {}/5 (threshold {}) tool_fired={}",
            result.competency.label(),
            result.scenario,
            result.verdict.score,
            result.threshold,
            result.knowledge_search_fired,
        );
        results.push(result);
    }

    let scorecard = Scorecard::from_results(&results);

    eprintln!("\n[eval-status] SCORECARD");
    for (competency, mean, count) in &scorecard.rows {
        eprintln!(
            "  {:<22} mean {mean:.2}/5 over {count} scenario(s)  floor {:.2}  {}",
            competency.label(),
            competency.floor(),
            if *mean >= competency.floor() {
                "OK"
            } else {
                "BREACH"
            },
        );
    }
    eprintln!(
        "  overall mean {:.2}/5 over {} scenarios",
        scorecard.overall_mean,
        results.len()
    );
    for miss in &scorecard.misses {
        eprintln!("  ✗ {miss}");
    }

    // Write the scorecard before asserting, so a failing night still leaves a
    // history row explaining what it failed on.
    let json = scorecard.to_json(&config.agent_model, &config.judge_model);
    let path = scorecard_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create target dir for scorecard");
    }
    std::fs::write(
        &path,
        serde_json::to_string(&json).expect("serialize scorecard"),
    )
    .expect("write scorecard");
    eprintln!("[eval-status] scorecard written to {}", path.display());

    let breaches = scorecard.breaches();
    assert!(
        breaches.is_empty(),
        "competency floor breached: {}",
        breaches
            .iter()
            .map(|(c, mean)| format!("{} {mean:.2} < {:.2}", c.label(), c.floor()))
            .collect::<Vec<_>>()
            .join(", ")
    );
}

/// Every scenario in both suites must roll up into a competency the scorecard
/// actually holds to a floor. This is credential-free — it is the part of the
/// regression layer that runs on every PR, so a newly added scenario that
/// nobody scores cannot slip in unnoticed.
#[test]
fn every_scenario_declares_a_competency_with_a_floor() {
    let scenarios: Vec<_> = default_scenarios()
        .into_iter()
        .chain(extended_scenarios())
        .collect();
    assert!(
        scenarios.len() >= 15,
        "expected the full scenario set, found {}",
        scenarios.len()
    );
    for s in &scenarios {
        let floor = s.competency.floor();
        assert!(
            (1.0..=5.0).contains(&floor),
            "scenario {} has competency {} with an out-of-range floor {floor}",
            s.name,
            s.competency.label()
        );
        assert!(
            (1..=5).contains(&s.pass_threshold),
            "scenario {} has an out-of-range pass_threshold {}",
            s.name,
            s.pass_threshold
        );
    }
}
