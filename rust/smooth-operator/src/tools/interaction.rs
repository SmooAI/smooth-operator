//! The Rich Interactions tools — kind-agnostic park/resume + fallback machinery
//! (see `docs/Architecture/Rich Interactions.md` and [`crate::interaction`]).
//!
//! - [`RequestInteractionTool`] — ONE instance per registered
//!   [`InteractionKind`], carrying that kind's precise LLM-facing schema. On a
//!   session that declared the kind's render capability it **parks the turn**:
//!   it sends the parsed [`InteractionRequest`] through its channel (the host's
//!   bridge emits `interaction_required` and registers a responder) and awaits
//!   the [`InteractionOutcome`]. Otherwise it returns immediately with the
//!   kind's conversational-fallback directive.
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
    InteractionKind, InteractionOutcome, InteractionRegistry, InteractionRequest,
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
    pub request_tx: UnboundedSender<InteractionRequest>,
    pub request_rx: UnboundedReceiver<InteractionRequest>,
    pub outcome_tx: UnboundedSender<InteractionOutcome>,
    pub outcome_rx: Arc<Mutex<UnboundedReceiver<InteractionOutcome>>>,
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
    request_tx: UnboundedSender<InteractionRequest>,
    outcome_rx: Arc<Mutex<UnboundedReceiver<InteractionOutcome>>>,
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
        request_tx: UnboundedSender<InteractionRequest>,
        outcome_rx: Arc<Mutex<UnboundedReceiver<InteractionOutcome>>>,
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
        if self.request_tx.send(request).is_err() {
            return Err(anyhow!("interaction channel closed"));
        }
        let mut rx = self.outcome_rx.lock().await;
        match tokio::time::timeout(self.timeout, rx.recv()).await {
            Ok(Some(InteractionOutcome::Submitted { values })) => Ok(json!({
                "status": "submitted",
                "values": values,
            })
            .to_string()),
            Ok(Some(InteractionOutcome::Declined)) => Ok(json!({
                "status": "declined",
                "message": "The visitor declined. Continue helping them without this and do not \
                            ask again this conversation.",
            })
            .to_string()),
            // Channel closed or timed out: let the turn continue rather than fail
            // it — the visitor simply didn't answer the card.
            Ok(None) | Err(_) => Ok(json!({
                "status": "no_response",
                "message": "The visitor did not respond to the card. Continue without it; you \
                            may offer again later if it becomes relevant.",
            })
            .to_string()),
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
            let req = request_rx.recv().await.expect("interaction raised");
            assert_eq!(req.kind, "identity_intake");
            assert_eq!(req.reason, "to follow up");
            outcome_tx
                .send(InteractionOutcome::Submitted {
                    values: json!({ "email": "a@b.co" }),
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
            let _ = request_rx.recv().await;
            let _ = outcome_tx.send(InteractionOutcome::Declined);
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
