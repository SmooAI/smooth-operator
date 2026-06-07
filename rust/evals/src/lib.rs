//! LLM-as-judge evaluation harness for `smooth-operator-agent`.
//!
//! Where the core crate's tests assert on substrings ("the reply contains
//! `17`"), this harness asks a **second LLM** — the *judge* — to score the
//! *quality* of the agent's behavior against a written rubric. That catches the
//! things substring checks can't: did the agent hallucinate a CEO name? did it
//! ground its answer in the retrieved fact rather than its own priors? did it
//! reason correctly over two turns?
//!
//! ## Shape
//!
//! 1. A [`Scenario`] declares a name, optional seeded KB docs, the user turn(s),
//!    the *ground-truth facts* the judge should hold the answer against, a
//!    free-text [`Scenario::rubric`], and a [`Scenario::pass_threshold`]
//!    (1–5; default 4).
//! 2. [`run_scenario`] builds a real [`KnowledgeChatRuntime`] over the in-memory
//!    adapter, seeds the KB, runs every user turn against the **live gateway**,
//!    captures the final reply + whether tools fired (from [`TurnOutcome`]), then
//!    calls the **judge model** (a separate gateway chat completion) with a
//!    rubric prompt and parses its strict-JSON verdict.
//! 3. The result is a [`JudgedResult`]: score, pass flag, reasoning, and whether
//!    it met the scenario's threshold.
//!
//! ## Same-model-judging limitation
//!
//! By default the agent and the judge are the **same model** (`claude-haiku-4-5`).
//! A model judging output from its own family is a known weak spot — it tends to
//! be lenient toward its own phrasing and shares blind spots. The
//! [`JudgeConfig`] honors `SMOOTH_AGENT_JUDGE_MODEL` so you can point the judge
//! at a stronger, different model (e.g. a Sonnet/GPT class) for a more adversarial
//! grade. This is the single most impactful knob for eval trustworthiness.
//!
//! ## Secret handling
//!
//! The gateway key is read from `SMOOAI_GATEWAY_KEY` and never printed. The
//! harness is gated: it only runs when `SMOOTH_AGENT_E2E=1` *and* the key is
//! present (see [`gate`]). Otherwise it skips.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use smooth_operator::llm::{ApiFormat, RetryPolicy};
use smooth_operator::{Document, DocumentType, LlmClient, LlmConfig, Message};
use smooth_operator_agent_adapter_memory::InMemoryStorageAdapter;
use smooth_operator_agent_core::runtime::KnowledgeChatRuntime;
use smooth_operator_agent_core::StorageAdapter;

/// The live OpenAI-compatible gateway.
pub const GATEWAY_URL: &str = "https://llm.smoo.ai/v1";
/// The cheap model used for both the agent and (by default) the judge.
pub const CHEAP_MODEL: &str = "claude-haiku-4-5";

/// One seeded knowledge-base document for a scenario.
#[derive(Debug, Clone)]
pub struct KbDoc {
    /// The document body (the fact the agent may retrieve).
    pub text: String,
    /// A source path/id, surfaced in retrieval citations.
    pub source: String,
}

impl KbDoc {
    /// Build a `KbDoc` from a body and source path.
    pub fn new(text: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            source: source.into(),
        }
    }
}

/// A single eval scenario: what to seed, what to ask, and how the judge scores.
#[derive(Debug, Clone)]
pub struct Scenario {
    /// Stable identifier, also used as the conversation id.
    pub name: &'static str,
    /// Knowledge-base documents to seed before the turn(s) run. May be empty.
    pub kb_docs: Vec<KbDoc>,
    /// The user turn(s), in order. Multi-turn scenarios put earlier context in
    /// the first entries; the agent's reply to the *last* turn is what the judge
    /// grades.
    pub user_turns: Vec<&'static str>,
    /// The ground-truth facts the judge should hold the agent's answer against
    /// (free text, given verbatim to the judge).
    pub ground_truth: &'static str,
    /// Instructions to the judge: exactly what to check. This is the rubric.
    pub rubric: &'static str,
    /// Minimum score (1–5) for the scenario to count as a pass.
    pub pass_threshold: u8,
}

/// The judge's parsed verdict for one scenario.
#[derive(Debug, Clone, Deserialize)]
pub struct JudgeVerdict {
    /// 1 (poor) – 5 (excellent).
    pub score: u8,
    /// The judge's own pass/fail call (advisory; the harness applies the
    /// scenario threshold independently in [`JudgedResult::met_threshold`]).
    pub pass: bool,
    /// One or two sentences explaining the score.
    pub reasoning: String,
}

/// The full outcome of judging a scenario.
#[derive(Debug, Clone)]
pub struct JudgedResult {
    /// The scenario name.
    pub scenario: &'static str,
    /// The agent's final reply that was judged.
    pub agent_reply: String,
    /// Whether `knowledge_search` fired at any point in the run.
    pub knowledge_search_fired: bool,
    /// The judge's verdict.
    pub verdict: JudgeVerdict,
    /// The threshold this scenario was held to.
    pub threshold: u8,
}

impl JudgedResult {
    /// `true` if the judge's score met or exceeded the scenario threshold.
    #[must_use]
    pub fn met_threshold(&self) -> bool {
        self.verdict.score >= self.threshold
    }
}

/// Configuration for the agent and judge gateway clients.
#[derive(Debug, Clone)]
pub struct JudgeConfig {
    /// Gateway base url (OpenAI-compatible).
    pub api_url: String,
    /// Gateway API key. Never logged.
    pub api_key: String,
    /// Model the *agent* runs with.
    pub agent_model: String,
    /// Model the *judge* runs with. Defaults to `agent_model` unless
    /// `SMOOTH_AGENT_JUDGE_MODEL` is set.
    pub judge_model: String,
}

impl JudgeConfig {
    /// Build a config from a key, defaulting both models to [`CHEAP_MODEL`] and
    /// honoring the `SMOOTH_AGENT_JUDGE_MODEL` override for the judge only.
    #[must_use]
    pub fn from_key(api_key: String) -> Self {
        let judge_model = std::env::var("SMOOTH_AGENT_JUDGE_MODEL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| CHEAP_MODEL.to_string());
        Self {
            api_url: GATEWAY_URL.to_string(),
            api_key,
            agent_model: CHEAP_MODEL.to_string(),
            judge_model,
        }
    }

    /// An `LlmConfig` for the **agent** runtime, pointed at the live gateway.
    /// `max_tokens` is modest because this is a paid endpoint.
    #[must_use]
    pub fn agent_llm_config(&self) -> LlmConfig {
        LlmConfig {
            api_url: self.api_url.clone(),
            api_key: self.api_key.clone(),
            model: self.agent_model.clone(),
            max_tokens: 512,
            temperature: 0.0,
            retry_policy: RetryPolicy::default(),
            api_format: ApiFormat::OpenAiCompat,
        }
    }

    /// An `LlmConfig` for the **judge** client. Slightly smaller token budget —
    /// the judge only emits a small JSON object.
    #[must_use]
    pub fn judge_llm_config(&self) -> LlmConfig {
        LlmConfig {
            api_url: self.api_url.clone(),
            api_key: self.api_key.clone(),
            model: self.judge_model.clone(),
            max_tokens: 300,
            temperature: 0.0,
            retry_policy: RetryPolicy::default(),
            api_format: ApiFormat::OpenAiCompat,
        }
    }
}

/// Gate the harness on the opt-in flag + key presence. Returns the key on
/// success, or `None` (with a printed skip notice) when the harness should be
/// skipped. NEVER prints the key value.
#[must_use]
pub fn gate(label: &str) -> Option<String> {
    if std::env::var("SMOOTH_AGENT_E2E").as_deref() != Ok("1") {
        eprintln!("[skip] {label}: SMOOTH_AGENT_E2E != \"1\" — skipping live-gateway evals");
        return None;
    }
    match std::env::var("SMOOAI_GATEWAY_KEY") {
        Ok(key) if !key.trim().is_empty() => Some(key),
        _ => {
            eprintln!(
                "[skip] {label}: SMOOAI_GATEWAY_KEY unset/empty — skipping live-gateway evals"
            );
            None
        }
    }
}

/// Build an in-memory adapter and seed it with the scenario's KB docs.
fn seeded_storage(scenario: &Scenario) -> Result<Arc<InMemoryStorageAdapter>> {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let kb = storage.knowledge();
    for doc in &scenario.kb_docs {
        kb.ingest(Document::new(
            doc.text.clone(),
            doc.source.clone(),
            DocumentType::Documentation,
        ))
        .with_context(|| format!("ingest KB doc {}", doc.source))?;
    }
    Ok(storage)
}

/// The judge's system prompt. It pins the output to a strict JSON object so the
/// verdict is machine-parseable.
const JUDGE_SYSTEM_PROMPT: &str =
    "You are a strict, fair evaluator of an AI customer-support agent. \
You grade the agent's REPLY against the rubric and the ground-truth facts. \
Be skeptical: a confident answer that invents facts not in the ground truth must score low, \
and appropriately admitting 'I don't know' when the ground truth lacks the answer must score high. \
Respond with ONLY a single JSON object, no prose, no markdown fences, exactly: \
{\"score\": <integer 1-5>, \"pass\": <true|false>, \"reasoning\": \"<one or two sentences>\"}.";

/// Build the user-side judge prompt for a scenario + the agent's reply.
fn judge_prompt(scenario: &Scenario, agent_reply: &str, knowledge_search_fired: bool) -> String {
    let conversation = scenario
        .user_turns
        .iter()
        .enumerate()
        .map(|(i, t)| format!("  Turn {}: {t}", i + 1))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "RUBRIC (what to check):\n{rubric}\n\n\
         GROUND-TRUTH FACTS (the only facts that are true here):\n{ground_truth}\n\n\
         USER CONVERSATION:\n{conversation}\n\n\
         Did the agent run a knowledge_search tool this turn? {tool}\n\n\
         AGENT'S FINAL REPLY (grade this):\n\"\"\"\n{reply}\n\"\"\"\n\n\
         Score 1-5 per the rubric. Return ONLY the JSON object.",
        rubric = scenario.rubric,
        ground_truth = scenario.ground_truth,
        conversation = conversation,
        tool = if knowledge_search_fired { "yes" } else { "no" },
        reply = agent_reply,
    )
}

/// Extract a JSON object from a possibly-noisy model response (handles ```json
/// fences and leading/trailing prose) and parse it into a [`JudgeVerdict`].
pub fn parse_verdict(raw: &str) -> Result<JudgeVerdict> {
    // Find the first '{' and the matching last '}' — the model sometimes wraps
    // the object in fences or a sentence. This is deliberately forgiving.
    let start = raw
        .find('{')
        .ok_or_else(|| anyhow!("no '{{' in judge response: {raw:?}"))?;
    let end = raw
        .rfind('}')
        .ok_or_else(|| anyhow!("no '}}' in judge response: {raw:?}"))?;
    if end < start {
        return Err(anyhow!("malformed brace span in judge response: {raw:?}"));
    }
    let candidate = &raw[start..=end];
    let verdict: JudgeVerdict = serde_json::from_str(candidate)
        .with_context(|| format!("parse judge JSON: {candidate:?}"))?;
    if !(1..=5).contains(&verdict.score) {
        return Err(anyhow!("judge score {} out of 1-5 range", verdict.score));
    }
    Ok(verdict)
}

/// Call the judge model once and parse its verdict. Retries the parse path once
/// on a malformed response (a fresh call with a terser nudge).
async fn judge_reply(
    judge: &LlmClient,
    scenario: &Scenario,
    agent_reply: &str,
    knowledge_search_fired: bool,
) -> Result<JudgeVerdict> {
    let system = Message::system(JUDGE_SYSTEM_PROMPT);
    let user = Message::user(judge_prompt(scenario, agent_reply, knowledge_search_fired));

    // First attempt.
    let messages: Vec<&Message> = vec![&system, &user];
    let resp = judge
        .chat(&messages, &[])
        .await
        .context("judge chat (attempt 1)")?;
    if let Ok(v) = parse_verdict(&resp.content) {
        return Ok(v);
    }
    eprintln!(
        "[judge] attempt 1 produced unparseable verdict for {}, retrying",
        scenario.name
    );

    // Retry once with an extra reminder appended.
    let nudge = Message::user(
        "Your previous answer was not valid JSON. Reply with ONLY the JSON object: \
         {\"score\": <1-5>, \"pass\": <bool>, \"reasoning\": \"...\"}",
    );
    let messages: Vec<&Message> = vec![&system, &user, &nudge];
    let resp = judge
        .chat(&messages, &[])
        .await
        .context("judge chat (attempt 2)")?;
    parse_verdict(&resp.content).context("judge verdict unparseable after retry")
}

/// Run one scenario end-to-end: seed KB, drive the agent over the live gateway,
/// then have the judge grade the final reply.
///
/// `config` carries both the agent and judge model selection (the judge model
/// may differ — see [`JudgeConfig`]).
///
/// # Errors
/// Returns an error if the agent loop fails, the judge call fails, or the judge
/// verdict can't be parsed after one retry.
pub async fn run_scenario(scenario: &Scenario, config: &JudgeConfig) -> Result<JudgedResult> {
    // --- 1. agent: real runtime over the in-memory adapter, live gateway ---
    let storage = seeded_storage(scenario)?;
    let runtime =
        KnowledgeChatRuntime::new(storage, config.agent_llm_config()).with_max_iterations(6);

    let mut agent_reply = String::new();
    let mut knowledge_search_fired = false;
    for turn in &scenario.user_turns {
        let outcome = runtime
            .run_turn(scenario.name, turn)
            .await
            .with_context(|| format!("agent run_turn for scenario {}", scenario.name))?;
        knowledge_search_fired |= outcome.invoked_tool("knowledge_search");
        agent_reply = outcome.reply;
    }

    // --- 2. judge: a separate gateway chat completion with the rubric ---
    let judge = LlmClient::new(config.judge_llm_config());
    let verdict = judge_reply(&judge, scenario, &agent_reply, knowledge_search_fired).await?;

    Ok(JudgedResult {
        scenario: scenario.name,
        agent_reply,
        knowledge_search_fired,
        verdict,
        threshold: scenario.pass_threshold,
    })
}

/// The default scenario suite. Each scenario targets a distinct AI competency.
#[must_use]
pub fn default_scenarios() -> Vec<Scenario> {
    // A deliberately unusual number (17 days) so a generic ungrounded answer
    // can't accidentally match the ground truth.
    let returns_doc = || {
        KbDoc::new(
            "SmooAI's return window is exactly 17 days from the delivery date. Returns must be \
         initiated within that window for a full refund.",
            "policies/returns.md",
        )
    };

    vec![
        // 1. GROUNDED ANSWER — fact is in the KB; the reply must state it
        //    correctly and not invent surrounding details.
        Scenario {
            name: "grounded_answer",
            kb_docs: vec![returns_doc()],
            user_turns: vec!["What is SmooAI's return policy? How many days do I have?"],
            ground_truth: "The return window is exactly 17 days from delivery, for a full refund. \
                           There are no other stated return details.",
            rubric: "Score 5 if the reply correctly states the 17-day return window AND stays \
                     grounded (does NOT invent extra policy details like restocking fees, \
                     exceptions, or a different number). Score 1 if it states a wrong number or \
                     fabricates details. Penalize any invented specifics not in the ground truth.",
            pass_threshold: 4,
        },
        // 2. HONEST NO-KNOWLEDGE — the anti-hallucination eval. The KB only has
        //    the return policy; the CEO's name is NOT in it. The agent must
        //    decline rather than invent a name.
        Scenario {
            name: "honest_no_knowledge",
            kb_docs: vec![returns_doc()],
            user_turns: vec!["What is the name of SmooAI's CEO?"],
            ground_truth: "The knowledge base contains ONLY the return policy. It has NO \
                           information about any CEO or any person's name. The correct behavior \
                           is to admit the information is not available.",
            rubric: "Score 5 ONLY if the agent appropriately says it does not know / cannot find \
                     the CEO's name in its knowledge base, WITHOUT inventing a name. Score 1 if \
                     the agent states ANY specific person's name as the CEO (that is a \
                     hallucination, since no name exists in the ground truth). A hedged guess that \
                     still names someone is a fail.",
            pass_threshold: 4,
        },
        // 3. TOOL-USE APPROPRIATENESS — a policy question that should be
        //    answered from retrieved knowledge; judge whether the answer is
        //    well-supported by the retrieved fact.
        Scenario {
            name: "tool_use_supported_answer",
            kb_docs: vec![
                returns_doc(),
                KbDoc::new(
                    "SmooAI standard shipping takes 5 to 7 business days within the continental US. \
                     Expedited shipping takes 2 business days.",
                    "policies/shipping.md",
                ),
            ],
            user_turns: vec!["How long does standard shipping take? Please check your knowledge base."],
            ground_truth: "Standard shipping takes 5 to 7 business days within the continental US. \
                           Expedited shipping takes 2 business days.",
            rubric: "Score 5 if the answer is well-supported by the retrieved shipping fact \
                     (states 5-7 business days for standard shipping) and does not contradict the \
                     ground truth. Score low if it invents a different timeframe or ignores the \
                     knowledge base.",
            pass_threshold: 4,
        },
        // 4. MULTI-TURN COHERENCE — turn 1 establishes a delivery date; turn 2
        //    asks a question that depends on it. Correct reasoning = 5th + 17
        //    days = the 22nd.
        Scenario {
            name: "multi_turn_coherence",
            kb_docs: vec![returns_doc()],
            user_turns: vec![
                "I ordered a SmooAI widget on the 1st of the month, and it was delivered on the 5th.",
                "Given that, what's the last day I can return it? Use the return policy.",
            ],
            ground_truth: "The return window is 17 days from DELIVERY (the 5th). 5 + 17 = the 22nd \
                           of the month. The correct last return day is the 22nd. (Reasoning from \
                           the order date, the 1st, would be wrong.)",
            rubric: "Score 5 if the agent correctly reasons over BOTH turns: it uses the delivery \
                     date (the 5th), adds the 17-day window, and arrives at the 22nd. Score 3 if \
                     it states the 17-day window but doesn't compute the date or anchors on the \
                     wrong date. Score 1 if it gives a wrong final date or loses the multi-turn \
                     context entirely.",
            pass_threshold: 4,
        },
        // 5. TONE / HELPFULNESS (optional) — clarity and helpfulness of a
        //    grounded reply, independent of raw correctness.
        Scenario {
            name: "tone_helpfulness",
            kb_docs: vec![returns_doc()],
            user_turns: vec!["Hi! I think my order might be defective — what are my options?"],
            ground_truth: "The only relevant policy is the 17-day return window for a full refund. \
                           A helpful reply acknowledges the concern, explains the return option, \
                           and is clear and courteous without inventing a warranty or repair \
                           process that isn't in the ground truth.",
            rubric: "Score 5 if the reply is clear, courteous, and helpful: it acknowledges the \
                     defect concern and points to the available return option (17-day window) \
                     without fabricating a warranty/repair policy that doesn't exist in the \
                     ground truth. Score low if it is curt, unhelpful, or invents policies.",
            pass_threshold: 4,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_verdict_plain_json() {
        let v = parse_verdict(r#"{"score": 5, "pass": true, "reasoning": "good"}"#).unwrap();
        assert_eq!(v.score, 5);
        assert!(v.pass);
        assert_eq!(v.reasoning, "good");
    }

    #[test]
    fn parse_verdict_with_fences_and_prose() {
        let raw = "Here is my verdict:\n```json\n{\"score\": 3, \"pass\": false, \
                   \"reasoning\": \"partial\"}\n```\nThanks.";
        let v = parse_verdict(raw).unwrap();
        assert_eq!(v.score, 3);
        assert!(!v.pass);
    }

    #[test]
    fn parse_verdict_rejects_out_of_range() {
        assert!(parse_verdict(r#"{"score": 9, "pass": true, "reasoning": "x"}"#).is_err());
    }

    #[test]
    fn parse_verdict_rejects_no_object() {
        assert!(parse_verdict("no json here").is_err());
    }

    #[test]
    fn default_scenarios_are_well_formed() {
        let scenarios = default_scenarios();
        assert_eq!(scenarios.len(), 5);
        for s in &scenarios {
            assert!(!s.user_turns.is_empty(), "{} has no turns", s.name);
            assert!(!s.rubric.is_empty(), "{} has no rubric", s.name);
            assert!(
                (1..=5).contains(&s.pass_threshold),
                "{} bad threshold",
                s.name
            );
        }
        // The multi-turn scenario must actually have >1 turn.
        let mt = scenarios
            .iter()
            .find(|s| s.name == "multi_turn_coherence")
            .unwrap();
        assert!(mt.user_turns.len() >= 2);
    }

    #[test]
    fn judge_model_defaults_to_agent_model_without_env() {
        // Only meaningful when the override isn't set in the ambient env.
        if std::env::var("SMOOTH_AGENT_JUDGE_MODEL").is_err() {
            let cfg = JudgeConfig::from_key("placeholder".to_string());
            assert_eq!(cfg.judge_model, cfg.agent_model);
            assert_eq!(cfg.judge_model, CHEAP_MODEL);
        }
    }
}
