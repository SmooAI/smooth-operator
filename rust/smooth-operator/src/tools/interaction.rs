//! The Rich Interactions tools — kind-agnostic park/resume + fallback machinery
//! (see `docs/Architecture/Rich Interactions.md` and [`crate::interaction`]).
//!
//! - [`RequestInteractionTool`] — ONE instance per registered
//!   [`InteractionKind`], carrying that kind's precise LLM-facing schema. On a
//!   session that declared the kind's render capability it **parks the turn**:
//!   it mints an interaction id, sends the parsed request through its channel as
//!   an [`InteractionRaise`] (the host's bridge emits `interaction_required` and
//!   registers a responder) and awaits the [`InteractionResolution`] carrying
//!   THAT id. Otherwise it returns immediately with the kind's
//!   conversational-fallback directive.
//! - [`SubmitInteractionTool`] — the generic model-callable submit for the
//!   conversational fallback (`submit_interaction { kind, values | declined }`).
//!   Routes to the kind's server-side validator; invalid values return a
//!   per-field tool error the model relays and re-asks; valid values invoke the
//!   host's attach callback and return the **identical** canonical payload the
//!   rich path resumes with.
//!
//! The channel plumbing mirrors smooth-operator-core's `ConfirmationHook`
//! (request out, outcome in, timeout-bounded park).

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::anyhow;
use async_trait::async_trait;
use serde_json::{json, Value};
use smooth_operator_core::tool::ToolSchema;
use smooth_operator_core::Tool;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::Mutex;

use crate::interaction::{
    InteractionKind, InteractionOutcome, InteractionRaise, InteractionRegistry,
    InteractionResolution,
};

/// Wire name of the generic conversational submit tool (same verb as the
/// protocol's resume action — same validation, same payload).
pub const SUBMIT_INTERACTION_TOOL: &str = "submit_interaction";

/// Host callback invoked with `(kind, canonical values)` on every successful
/// submit on the conversational path (the rich path attaches in the WS handler,
/// which owns validation there). The identity_intake kind's host effect stamps
/// the session's `userName` / `contactEmail` / `contactPhone` metadata.
pub type InteractionAttach = Arc<dyn Fn(&str, &Value) + Send + Sync>;

/// Specs raised earlier THIS turn on the conversational path, keyed by kind, so
/// the generic submit tool can validate with full required-ness. A raise from a
/// PRIOR turn isn't in here (per-turn state) — the kind then validates
/// format-only against a `Null` spec.
pub type RaisedSpecs = Arc<StdMutex<HashMap<String, Value>>>;

/// The four endpoints of an interaction park-and-resume channel pair. The raise
/// tools own `request_tx` + `outcome_rx`; the host's bridge owns `request_rx` +
/// `outcome_tx` (mirrors core's `HumanChannelPair`).
pub struct InteractionChannelPair {
    pub request_tx: UnboundedSender<InteractionRaise>,
    pub request_rx: UnboundedReceiver<InteractionRaise>,
    pub outcome_tx: UnboundedSender<InteractionResolution>,
    pub outcome_rx: Arc<Mutex<UnboundedReceiver<InteractionResolution>>>,
}

/// Create the interaction channel pair.
#[must_use]
pub fn interaction_channel() -> InteractionChannelPair {
    let (request_tx, request_rx) = tokio::sync::mpsc::unbounded_channel();
    let (outcome_tx, outcome_rx) = tokio::sync::mpsc::unbounded_channel();
    InteractionChannelPair {
        request_tx,
        request_rx,
        outcome_tx,
        outcome_rx: Arc::new(Mutex::new(outcome_rx)),
    }
}

/// How long a parked interaction waits for a `submit_interaction` action before
/// the tool gives up and lets the turn continue without the details. Generous —
/// a human is filling a card.
pub const INTERACTION_TIMEOUT: Duration = Duration::from_secs(300);

/// The per-kind raise tool. See the module docs for the two paths.
pub struct RequestInteractionTool {
    kind: Arc<dyn InteractionKind>,
    /// Whether this session's client declared the kind's render capability.
    rich: bool,
    request_tx: UnboundedSender<InteractionRaise>,
    outcome_rx: Arc<Mutex<UnboundedReceiver<InteractionResolution>>>,
    /// Fallback-path spec stash (see [`RaisedSpecs`]). Written on every
    /// conversational raise so the submit tool validates with required-ness.
    raised: RaisedSpecs,
    timeout: Duration,
}

impl RequestInteractionTool {
    /// Build the raise tool for one kind. `rich` selects the park path; the
    /// channel ends come from [`interaction_channel`] (the host keeps the other
    /// two ends for its bridge).
    #[must_use]
    pub fn new(
        kind: Arc<dyn InteractionKind>,
        rich: bool,
        request_tx: UnboundedSender<InteractionRaise>,
        outcome_rx: Arc<Mutex<UnboundedReceiver<InteractionResolution>>>,
        raised: RaisedSpecs,
    ) -> Self {
        Self {
            kind,
            rich,
            request_tx,
            outcome_rx,
            raised,
            timeout: INTERACTION_TIMEOUT,
        }
    }

    /// Override the park timeout (tests).
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

#[async_trait]
impl Tool for RequestInteractionTool {
    fn schema(&self) -> ToolSchema {
        self.kind.tool_schema()
    }

    async fn execute(&self, arguments: Value) -> anyhow::Result<String> {
        let request = self.kind.parse_request(&arguments)?;

        if !self.rich {
            // Text-only channel: degrade to the kind's conversational directive.
            // Stash the spec so a same-turn `submit_interaction` validates with
            // full required-ness.
            if let Ok(mut raised) = self.raised.lock() {
                raised.insert(request.kind.clone(), request.spec.clone());
            }
            return Ok(json!({
                "mode": "conversational",
                "kind": request.kind,
                "spec": request.spec,
                "reason": request.reason,
                "instructions": self.kind.fallback_directive(&request.spec, &request.reason),
            })
            .to_string());
        }

        // Rich channel: park the turn. The host bridge (listening on the request
        // receiver) emits `interaction_required` + registers the outcome sender;
        // the WS handler validates the visitor's `submit_interaction` and feeds
        // the outcome back here.
        let id = uuid::Uuid::new_v4().to_string();
        if self
            .request_tx
            .send(InteractionRaise {
                id: id.clone(),
                request,
            })
            .is_err()
        {
            return Err(anyhow!("interaction channel closed"));
        }

        // The outcome channel is shared by every raise in the turn, so wait for
        // OUR id: a resolution carrying any other id answers a park that already
        // gave up (its timeout fired and the turn moved on), and consuming it
        // here would answer this question with that one's values — across
        // questions and even across kinds (th-d121f5). Drop it and keep waiting
        // on the ORIGINAL deadline, so a stale card can't extend this park.
        let deadline = tokio::time::Instant::now() + self.timeout;
        let mut rx = self.outcome_rx.lock().await;
        loop {
            let received =
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Ok(Some(resolution)) => resolution,
                    // Channel closed or timed out: let the turn continue rather than
                    // fail it — the visitor simply didn't answer the card.
                    Ok(None) | Err(_) => return Ok(json!({
                        "status": "no_response",
                        "message": "The visitor did not respond to the card. Continue without it; \
                                    you may offer again later if it becomes relevant.",
                    })
                    .to_string()),
                };
            if received.interaction_id != id {
                continue;
            }
            return Ok(match received.outcome {
                InteractionOutcome::Submitted { values } => json!({
                    "status": "submitted",
                    "values": values,
                })
                .to_string(),
                InteractionOutcome::Declined => json!({
                    "status": "declined",
                    "message": "The visitor declined. Continue helping them without this and do \
                                not ask again this conversation.",
                })
                .to_string(),
            });
        }
    }

    /// Parks the turn awaiting a human — never safe to run alongside others.
    fn is_concurrent_safe(&self) -> bool {
        false
    }
}

/// The generic `submit_interaction` tool — the conversational fallback's submit
/// half, one instance per turn regardless of how many kinds are hosted.
/// Registered only when at least one kind is on the fallback path (rich
/// sessions submit via the protocol action instead).
pub struct SubmitInteractionTool {
    kinds: InteractionRegistry,
    raised: RaisedSpecs,
    /// Host attach callback. `None` ⇒ validate-only.
    on_submit: Option<InteractionAttach>,
}

impl SubmitInteractionTool {
    #[must_use]
    pub fn new(kinds: InteractionRegistry, raised: RaisedSpecs) -> Self {
        Self {
            kinds,
            raised,
            on_submit: None,
        }
    }

    /// Invoke `attach(kind, values)` on every successful submit.
    #[must_use]
    pub fn with_attach(mut self, attach: InteractionAttach) -> Self {
        self.on_submit = Some(attach);
        self
    }
}

#[async_trait]
impl Tool for SubmitInteractionTool {
    fn schema(&self) -> ToolSchema {
        let kind_ids: Vec<&str> = self.kinds.kinds().iter().map(|k| k.kind()).collect();
        ToolSchema {
            name: SUBMIT_INTERACTION_TOOL.to_string(),
            description: "Submit the visitor's answers collected conversationally after a \
                          request_* interaction directive. Values are validated server-side; on \
                          a validation error, apologize, re-ask for the corrected field, and \
                          submit again. If the visitor declined, set declined=true."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "enum": kind_ids, "description": "The interaction kind being submitted (from the directive)." },
                    "values": { "type": "object", "description": "The collected values, shaped per the interaction kind." },
                    "declined": { "type": "boolean", "description": "True when the visitor declined the interaction." }
                },
                "required": ["kind"]
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> anyhow::Result<String> {
        let kind_id = arguments
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("'kind' is required"))?;
        let Some(kind) = self.kinds.get(kind_id) else {
            return Err(anyhow!("unknown interaction kind '{kind_id}'"));
        };

        if arguments
            .get("declined")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Ok(json!({
                "status": "declined",
                "message": "Noted. Continue helping the visitor without this and do not ask \
                            again this conversation.",
            })
            .to_string());
        }

        let values = arguments.get("values").cloned().unwrap_or(Value::Null);
        // The spec raised earlier this turn (full required-ness) — or Null (a
        // prior-turn raise): the kind then validates format-only.
        let spec = self
            .raised
            .lock()
            .ok()
            .and_then(|m| m.get(kind_id).cloned())
            .unwrap_or(Value::Null);

        match kind.validate(&spec, &values) {
            Ok(canonical) => {
                if let Some(attach) = &self.on_submit {
                    attach(kind_id, &canonical);
                }
                Ok(json!({
                    "status": "submitted",
                    "values": canonical,
                })
                .to_string())
            }
            Err(errors) => {
                let detail = errors
                    .iter()
                    .map(|e| format!("{}: {}", e.field, e.message))
                    .collect::<Vec<_>>()
                    .join("; ");
                Err(anyhow!(
                    "validation failed — {detail}. Re-ask the visitor for the corrected value(s) and submit again."
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity_intake::IdentityIntakeKind;

    fn identity() -> Arc<dyn InteractionKind> {
        Arc::new(IdentityIntakeKind)
    }

    fn raised() -> RaisedSpecs {
        Arc::new(StdMutex::new(HashMap::new()))
    }

    #[tokio::test]
    async fn text_channel_returns_the_kinds_directive_and_stashes_the_spec() {
        let pair = interaction_channel();
        let stash = raised();
        let tool = RequestInteractionTool::new(
            identity(),
            false,
            pair.request_tx,
            pair.outcome_rx,
            Arc::clone(&stash),
        );
        let out = tool
            .execute(json!({
                "fields": ["email", {"key": "phone", "required": false}],
                "reason": "to send you the quote"
            }))
            .await
            .expect("directive");
        let v: Value = serde_json::from_str(&out).expect("json");
        assert_eq!(v["mode"], "conversational");
        assert_eq!(v["kind"], "identity_intake");
        assert_eq!(v["spec"]["fields"][0]["key"], "email");
        assert_eq!(
            v["spec"]["fields"][0]["required"], true,
            "shorthand ⇒ required"
        );
        assert!(v["instructions"]
            .as_str()
            .unwrap()
            .contains(SUBMIT_INTERACTION_TOOL));
        // The spec was stashed for same-turn required-ness validation.
        assert!(stash.lock().unwrap().contains_key("identity_intake"));
    }

    #[tokio::test]
    async fn rich_channel_parks_and_resumes_with_submitted_values() {
        let pair = interaction_channel();
        let tool = RequestInteractionTool::new(
            identity(),
            true,
            pair.request_tx,
            pair.outcome_rx,
            raised(),
        );
        let mut request_rx = pair.request_rx;
        let outcome_tx = pair.outcome_tx;

        // Host bridge: receive the raise, feed back validated values.
        let bridge = tokio::spawn(async move {
            let raise = request_rx.recv().await.expect("interaction raised");
            assert_eq!(raise.request.kind, "identity_intake");
            assert_eq!(raise.request.reason, "to follow up");
            outcome_tx
                .send(InteractionResolution {
                    interaction_id: raise.id,
                    outcome: InteractionOutcome::Submitted {
                        values: json!({ "email": "a@b.co" }),
                    },
                })
                .expect("send outcome");
        });

        let out = tool
            .execute(json!({ "fields": ["email"], "reason": "to follow up" }))
            .await
            .expect("resumed");
        bridge.await.expect("bridge");
        let v: Value = serde_json::from_str(&out).expect("json");
        assert_eq!(v["status"], "submitted");
        assert_eq!(v["values"]["email"], "a@b.co");
    }

    #[tokio::test]
    async fn rich_channel_decline_and_timeout_degrade_gracefully() {
        // Decline.
        let pair = interaction_channel();
        let tool = RequestInteractionTool::new(
            identity(),
            true,
            pair.request_tx,
            pair.outcome_rx,
            raised(),
        );
        let mut request_rx = pair.request_rx;
        let outcome_tx = pair.outcome_tx;
        tokio::spawn(async move {
            let raise = request_rx.recv().await.expect("raised");
            let _ = outcome_tx.send(InteractionResolution {
                interaction_id: raise.id,
                outcome: InteractionOutcome::Declined,
            });
        });
        let out = tool
            .execute(json!({ "fields": ["email"], "reason": "r" }))
            .await
            .expect("resumed");
        assert_eq!(
            serde_json::from_str::<Value>(&out).unwrap()["status"],
            "declined"
        );

        // Timeout → no_response, not an error.
        let pair = interaction_channel();
        let tool = RequestInteractionTool::new(
            identity(),
            true,
            pair.request_tx,
            pair.outcome_rx,
            raised(),
        )
        .with_timeout(Duration::from_millis(20));
        let _request_rx = pair.request_rx;
        let _outcome_tx = pair.outcome_tx;
        let out = tool
            .execute(json!({ "fields": ["name"], "reason": "r" }))
            .await
            .expect("degrades, not errors");
        assert_eq!(
            serde_json::from_str::<Value>(&out).unwrap()["status"],
            "no_response"
        );
    }

    /// th-d121f5 — every raise in a turn shares ONE outcome channel, so a park
    /// that only did `rx.recv()` took whatever arrived first. Raise #1 times out
    /// and the turn moves on; the visitor then clicks its still-rendered card,
    /// queueing an outcome nobody is waiting for. The NEXT raise — a different
    /// question, here a different kind entirely — must not read that as its
    /// answer.
    #[tokio::test]
    async fn a_dead_parks_outcome_is_never_consumed_by_the_next_raise() {
        let InteractionChannelPair {
            request_tx,
            mut request_rx,
            outcome_tx,
            outcome_rx,
        } = interaction_channel();

        let choices = RequestInteractionTool::new(
            Arc::new(crate::choices::ChoicesKind),
            true,
            request_tx.clone(),
            Arc::clone(&outcome_rx),
            raised(),
        )
        .with_timeout(Duration::from_millis(20));
        let intake =
            RequestInteractionTool::new(identity(), true, request_tx, outcome_rx, raised());

        // Raise #1 parks and nobody answers in time.
        let out = choices
            .execute(json!({
                "questions": [{ "key": "plan", "header": "Which plan?", "question": "Which plan fits?", "options": [{"label": "Starter"}, {"label": "Enterprise"}] }],
                "reason": "to quote you"
            }))
            .await
            .expect("degrades on timeout");
        assert_eq!(
            serde_json::from_str::<Value>(&out).unwrap()["status"],
            "no_response"
        );
        let stale = request_rx.recv().await.expect("raise #1");
        assert_eq!(stale.request.kind, "choices");

        // The visitor clicks the stale card — queued ahead of raise #2's answer.
        outcome_tx
            .send(InteractionResolution {
                interaction_id: stale.id,
                outcome: InteractionOutcome::Submitted {
                    values: json!({ "plan": "Enterprise" }),
                },
            })
            .expect("stale outcome queued");

        // Raise #2 parks; its own answer follows.
        let bridge = {
            let outcome_tx = outcome_tx.clone();
            tokio::spawn(async move {
                let raise = request_rx.recv().await.expect("raise #2");
                assert_eq!(raise.request.kind, "identity_intake");
                outcome_tx
                    .send(InteractionResolution {
                        interaction_id: raise.id,
                        outcome: InteractionOutcome::Submitted {
                            values: json!({ "email": "a@b.co" }),
                        },
                    })
                    .expect("send outcome");
            })
        };
        let out = intake
            .execute(json!({ "fields": ["email"], "reason": "to follow up" }))
            .await
            .expect("resumed");
        bridge.await.expect("bridge");

        let v: Value = serde_json::from_str(&out).expect("json");
        assert_eq!(v["status"], "submitted");
        assert_eq!(
            v["values"]["email"], "a@b.co",
            "the intake park must resume with ITS answer, not the dead choices park's: {v}"
        );
        assert!(
            v["values"]["plan"].is_null(),
            "cross-kind bleed: the choices answer resolved an identity_intake raise: {v}"
        );
    }

    /// Stale outcomes are dropped one after another without ever resolving the
    /// park — the park degrades on its own timeout as if they had never arrived.
    ///
    /// Runs on tokio's virtual clock so the 5 drops and the expiry are ordered by
    /// the runtime rather than by how loaded the machine is.
    ///
    /// The park also keeps its ORIGINAL deadline across those drops (`timeout_at`,
    /// not a fresh `timeout` per message), so stale clicks can't hold a turn open.
    /// That is deliberately NOT asserted here: an elapsed-time bound cannot
    /// distinguish the two — the restart bug expires at ~110ms against a 60ms
    /// park, and no threshold separates those without measuring the scheduler.
    #[tokio::test(start_paused = true)]
    async fn stale_outcomes_are_dropped_without_resolving_the_park() {
        let pair = interaction_channel();
        let tool = RequestInteractionTool::new(
            identity(),
            true,
            pair.request_tx,
            Arc::clone(&pair.outcome_rx),
            raised(),
        )
        .with_timeout(Duration::from_millis(60));
        let mut request_rx = pair.request_rx;
        let outcome_tx = pair.outcome_tx;

        tokio::spawn(async move {
            let _ = request_rx.recv().await;
            for _ in 0..5 {
                let _ = outcome_tx.send(InteractionResolution {
                    interaction_id: "somebody-elses-park".to_string(),
                    outcome: InteractionOutcome::Declined,
                });
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });

        let out = tool
            .execute(json!({ "fields": ["email"], "reason": "r" }))
            .await
            .expect("degrades");
        assert_eq!(
            serde_json::from_str::<Value>(&out).unwrap()["status"],
            "no_response",
            "five outcomes for another park must not resolve this one"
        );
    }

    #[tokio::test]
    async fn unknown_field_key_is_an_error() {
        let pair = interaction_channel();
        let tool = RequestInteractionTool::new(
            identity(),
            false,
            pair.request_tx,
            pair.outcome_rx,
            raised(),
        );
        let err = tool
            .execute(json!({ "fields": ["ssn"], "reason": "r" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown intake field"));
    }

    #[tokio::test]
    async fn submit_tool_routes_to_the_kind_validator_and_attaches() {
        let attached: Arc<StdMutex<Option<(String, Value)>>> = Arc::new(StdMutex::new(None));
        let sink = Arc::clone(&attached);
        let tool = SubmitInteractionTool::new(InteractionRegistry::default(), raised())
            .with_attach(Arc::new(move |kind, values| {
                *sink.lock().unwrap() = Some((kind.to_string(), values.clone()));
            }));

        // Unknown kind → error.
        let err = tool
            .execute(json!({ "kind": "date_picker", "values": {} }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown interaction kind"));

        // Bad email → tool error the model relays.
        let err = tool
            .execute(json!({ "kind": "identity_intake", "values": { "email": "not-an-email" } }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("email"));
        assert!(attached.lock().unwrap().is_none(), "no attach on failure");

        // Good values → normalized payload + attach callback.
        let out = tool
            .execute(json!({ "kind": "identity_intake", "values": { "email": "A@b.CO", "phone": "555-123-4567" } }))
            .await
            .expect("valid");
        let v: Value = serde_json::from_str(&out).expect("json");
        assert_eq!(v["status"], "submitted");
        assert_eq!(v["values"]["email"], "A@b.co");
        assert_eq!(v["values"]["phone"], "+15551234567");
        let (kind, got) = attached.lock().unwrap().clone().expect("attached");
        assert_eq!(kind, "identity_intake");
        assert_eq!(got["phone"], "+15551234567");
    }

    #[tokio::test]
    async fn submit_tool_enforces_required_ness_from_the_same_turn_raise() {
        // Raise on the fallback path (stashes the spec: email REQUIRED)…
        let pair = interaction_channel();
        let stash = raised();
        let raise = RequestInteractionTool::new(
            identity(),
            false,
            pair.request_tx,
            pair.outcome_rx,
            Arc::clone(&stash),
        );
        raise
            .execute(json!({ "fields": [{"key": "email", "required": true}], "reason": "r" }))
            .await
            .expect("directive");

        // …then a submit missing the required email fails required-ness.
        let tool = SubmitInteractionTool::new(InteractionRegistry::default(), stash);
        let err = tool
            .execute(json!({ "kind": "identity_intake", "values": { "name": "Ada" } }))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("required"),
            "same-turn raise enforces required fields: {err}"
        );
    }

    #[tokio::test]
    async fn submit_tool_declined_path() {
        let tool = SubmitInteractionTool::new(InteractionRegistry::default(), raised());
        let out = tool
            .execute(json!({ "kind": "identity_intake", "declined": true }))
            .await
            .expect("ok");
        assert!(out.contains("declined"));
    }
}
