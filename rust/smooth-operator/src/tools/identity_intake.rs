//! The identity-intake tools — the agent's verb for collecting the visitor's
//! name / email / phone, normalized across channels.
//!
//! Two tools, one seam (see `docs/Architecture/Identity Intake.md`):
//!
//! - [`RequestIdentityIntakeTool`] (`request_identity_intake`) — the agent
//!   raises the need. On a **form-capable** session (the client declared the
//!   `identity_form` capability) the tool **parks the turn**: it sends the
//!   parsed [`IntakeRequest`] through its channel (the host's bridge emits the
//!   `identity_intake_required` protocol event and registers a responder) and
//!   awaits the [`IntakeOutcome`]. On a **text-only** session it returns
//!   immediately with a conversational directive: collect the fields one at a
//!   time, then call `submit_identity_intake`.
//! - [`SubmitIdentityIntakeTool`] (`submit_identity_intake`) — the model-callable
//!   half of the conversational fallback. Runs the same server-side validation
//!   ([`validate_intake`]) as the form path's WS handler; invalid values return
//!   a per-field tool error the model relays and re-asks; valid values invoke
//!   the host's attach callback and return the **identical** structured payload
//!   the form path resumes with.
//!
//! The channel plumbing mirrors smooth-operator-core's `ConfirmationHook`
//! (request out, outcome in, timeout-bounded park).

use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use async_trait::async_trait;
use serde_json::{json, Value};
use smooth_operator_core::tool::ToolSchema;
use smooth_operator_core::Tool;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::Mutex;

use crate::identity_intake::{
    validate_intake, IntakeField, IntakeFieldKey, IntakeOutcome, IntakeRequest, IntakeValues,
};

/// Wire name of the raise tool (and nothing else — the resume *action* shares
/// the submit tool's name by design).
pub const REQUEST_IDENTITY_INTAKE_TOOL: &str = "request_identity_intake";
/// Wire name of the conversational submit tool (same verb as the protocol's
/// resume action — same validation, same payload).
pub const SUBMIT_IDENTITY_INTAKE_TOOL: &str = "submit_identity_intake";

/// Host callback invoked with validated values on a successful conversational
/// submit (the form path attaches in the WS handler instead). Typically stamps
/// the session's `userName` / `contactEmail` / `contactPhone` metadata.
pub type IdentityAttach = Arc<dyn Fn(&IntakeValues) + Send + Sync>;

/// The four endpoints of an intake park-and-resume channel pair. The tool owns
/// `request_tx` + `outcome_rx`; the host's bridge owns `request_rx` +
/// `outcome_tx` (mirrors core's `HumanChannelPair`).
pub struct IntakeChannelPair {
    pub request_tx: UnboundedSender<IntakeRequest>,
    pub request_rx: UnboundedReceiver<IntakeRequest>,
    pub outcome_tx: UnboundedSender<IntakeOutcome>,
    pub outcome_rx: Arc<Mutex<UnboundedReceiver<IntakeOutcome>>>,
}

/// Create the intake channel pair.
#[must_use]
pub fn intake_channel() -> IntakeChannelPair {
    let (request_tx, request_rx) = tokio::sync::mpsc::unbounded_channel();
    let (outcome_tx, outcome_rx) = tokio::sync::mpsc::unbounded_channel();
    IntakeChannelPair {
        request_tx,
        request_rx,
        outcome_tx,
        outcome_rx: Arc::new(Mutex::new(outcome_rx)),
    }
}

/// Parse the tool's `fields` argument. Accepts both the structured form
/// (`[{ "key": "email", "required": true, "label": "Work email" }]`) and the
/// shorthand models like to emit (`["email", "name"]` — shorthand fields are
/// `required: true`). Unknown keys are an error (closed set).
fn parse_fields(raw: &Value) -> anyhow::Result<Vec<IntakeField>> {
    let items = raw
        .as_array()
        .ok_or_else(|| anyhow!("'fields' must be an array"))?;
    if items.is_empty() {
        return Err(anyhow!("'fields' must contain at least one field"));
    }
    let parse_key = |s: &str| -> anyhow::Result<IntakeFieldKey> {
        match s {
            "name" => Ok(IntakeFieldKey::Name),
            "email" => Ok(IntakeFieldKey::Email),
            "phone" => Ok(IntakeFieldKey::Phone),
            other => Err(anyhow!(
                "unknown intake field '{other}' (expected name, email, or phone)"
            )),
        }
    };
    let mut fields = Vec::with_capacity(items.len());
    for item in items {
        match item {
            Value::String(s) => fields.push(IntakeField {
                key: parse_key(s)?,
                required: true,
                label: None,
            }),
            Value::Object(obj) => {
                let key = obj
                    .get("key")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("each field object needs a string 'key'"))?;
                fields.push(IntakeField {
                    key: parse_key(key)?,
                    required: obj.get("required").and_then(Value::as_bool).unwrap_or(true),
                    label: obj.get("label").and_then(Value::as_str).map(str::to_string),
                });
            }
            other => return Err(anyhow!("invalid field entry: {other}")),
        }
    }
    Ok(fields)
}

/// How long a parked intake waits for a `submit_identity_intake` action before
/// the tool gives up and lets the turn continue without the details. Generous —
/// a human is typing into a form.
pub const INTAKE_TIMEOUT: Duration = Duration::from_secs(300);

/// The `request_identity_intake` tool. See the module docs for the two paths.
pub struct RequestIdentityIntakeTool {
    /// Whether this session's client declared the `identity_form` capability.
    form_supported: bool,
    request_tx: UnboundedSender<IntakeRequest>,
    outcome_rx: Arc<Mutex<UnboundedReceiver<IntakeOutcome>>>,
    timeout: Duration,
}

impl RequestIdentityIntakeTool {
    /// Build the tool. `form_supported` selects the park path; the channel ends
    /// come from [`intake_channel`] (the host keeps the other two ends for its
    /// bridge). On a text-only session the channel is never used — pass a fresh
    /// pair's ends anyway (cheap) so construction stays uniform.
    #[must_use]
    pub fn new(
        form_supported: bool,
        request_tx: UnboundedSender<IntakeRequest>,
        outcome_rx: Arc<Mutex<UnboundedReceiver<IntakeOutcome>>>,
    ) -> Self {
        Self {
            form_supported,
            request_tx,
            outcome_rx,
            timeout: INTAKE_TIMEOUT,
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
impl Tool for RequestIdentityIntakeTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: REQUEST_IDENTITY_INTAKE_TOOL.to_string(),
            description: "Ask the visitor for their contact details (name, email, and/or phone) \
                          in a channel-appropriate way. On channels that can render a form the \
                          visitor fills a structured form; on text channels you will be told to \
                          collect the fields conversationally. Always use this tool instead of \
                          free-forming a request for contact details."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "fields": {
                        "type": "array",
                        "description": "Which fields to collect, in order. Each entry is either a string (\"name\" | \"email\" | \"phone\") or an object { key, required?, label? }.",
                        "items": {
                            "anyOf": [
                                { "type": "string", "enum": ["name", "email", "phone"] },
                                {
                                    "type": "object",
                                    "properties": {
                                        "key": { "type": "string", "enum": ["name", "email", "phone"] },
                                        "required": { "type": "boolean" },
                                        "label": { "type": "string" }
                                    },
                                    "required": ["key"]
                                }
                            ]
                        }
                    },
                    "reason": {
                        "type": "string",
                        "description": "Why you need these details, phrased for the visitor (e.g. \"to send you the quote\")."
                    }
                },
                "required": ["fields", "reason"]
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> anyhow::Result<String> {
        let fields = parse_fields(arguments.get("fields").unwrap_or(&Value::Null))?;
        let reason = arguments
            .get("reason")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("to help you better")
            .to_string();
        let request = IntakeRequest { fields, reason };

        if !self.form_supported {
            // Text-only channel: degrade to a conversational directive. The model
            // collects the fields turn-by-turn and submits through the
            // `submit_identity_intake` tool (same validation, same payload).
            return Ok(json!({
                "mode": "conversational",
                "fields": request.fields,
                "reason": request.reason,
                "instructions": format!(
                    "This visitor's channel cannot display a form. Collect the requested details \
                     conversationally: ask for ONE field at a time, in the order given, naturally \
                     weaving in the reason ({}). When you have the values, call the \
                     `{SUBMIT_IDENTITY_INTAKE_TOOL}` tool with them — it validates each field and \
                     will tell you if something looks wrong so you can re-ask. If the visitor \
                     declines to share, call `{SUBMIT_IDENTITY_INTAKE_TOOL}` with declined=true \
                     and continue helping them without the details.",
                    request.reason
                ),
            })
            .to_string());
        }

        // Form-capable channel: park the turn. The host bridge (listening on the
        // request receiver) emits `identity_intake_required` + registers the
        // outcome sender; the WS handler validates the visitor's
        // `submit_identity_intake` and feeds the outcome back here.
        if self.request_tx.send(request).is_err() {
            return Err(anyhow!("identity intake channel closed"));
        }
        let mut rx = self.outcome_rx.lock().await;
        match tokio::time::timeout(self.timeout, rx.recv()).await {
            Ok(Some(IntakeOutcome::Submitted(values))) => Ok(json!({
                "status": "submitted",
                "values": values,
            })
            .to_string()),
            Ok(Some(IntakeOutcome::Declined)) => Ok(json!({
                "status": "declined",
                "message": "The visitor declined to share their details. Continue helping them \
                            without this information and do not ask again this conversation.",
            })
            .to_string()),
            // Channel closed or timed out: let the turn continue rather than fail
            // it — the visitor simply didn't fill the form.
            Ok(None) | Err(_) => Ok(json!({
                "status": "no_response",
                "message": "The visitor did not fill the form. Continue without the details; you \
                            may offer to collect them again later if it becomes relevant.",
            })
            .to_string()),
        }
    }

    /// Parks the turn awaiting a human — never safe to run alongside others.
    fn is_concurrent_safe(&self) -> bool {
        false
    }
}

/// The `submit_identity_intake` tool — the conversational fallback's submit
/// half. Registered only on sessions **without** the `identity_form` capability
/// (form sessions submit via the protocol action instead).
pub struct SubmitIdentityIntakeTool {
    /// Host attach callback (session metadata / CRM). `None` ⇒ validate-only.
    on_submit: Option<IdentityAttach>,
}

impl SubmitIdentityIntakeTool {
    #[must_use]
    pub fn new() -> Self {
        Self { on_submit: None }
    }

    /// Invoke `attach` with the validated values on every successful submit.
    #[must_use]
    pub fn with_attach(mut self, attach: IdentityAttach) -> Self {
        self.on_submit = Some(attach);
        self
    }
}

impl Default for SubmitIdentityIntakeTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for SubmitIdentityIntakeTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: SUBMIT_IDENTITY_INTAKE_TOOL.to_string(),
            description: "Submit the visitor's contact details collected conversationally after a \
                          request_identity_intake directive. Each field is validated server-side; \
                          on a validation error, apologize, re-ask for the corrected field, and \
                          submit again. If the visitor declined to share, set declined=true."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "The visitor's name, as they gave it." },
                    "email": { "type": "string", "description": "The visitor's email address." },
                    "phone": { "type": "string", "description": "The visitor's phone number." },
                    "declined": { "type": "boolean", "description": "True when the visitor declined to share their details." }
                }
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> anyhow::Result<String> {
        if arguments
            .get("declined")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Ok(json!({
                "status": "declined",
                "message": "Noted. Continue helping the visitor without their contact details \
                            and do not ask again this conversation.",
            })
            .to_string());
        }

        let get = |k: &str| arguments.get(k).and_then(Value::as_str).map(str::to_string);
        let values = IntakeValues {
            name: get("name"),
            email: get("email"),
            phone: get("phone"),
        };
        if values.is_empty() {
            return Err(anyhow!(
                "provide at least one of name/email/phone, or declined=true"
            ));
        }

        // Format validation + normalization. Required-ness is driven by the
        // conversational directive (the model asks for what was requested);
        // the format gate here is the server-side quality boundary.
        match validate_intake(&[], &values) {
            Ok(validated) => {
                if let Some(attach) = &self.on_submit {
                    attach(&validated);
                }
                Ok(json!({
                    "status": "submitted",
                    "values": validated,
                })
                .to_string())
            }
            Err(errors) => {
                let detail = errors
                    .iter()
                    .map(|e| format!("{}: {}", e.field.as_str(), e.message))
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

    fn args(v: Value) -> Value {
        v
    }

    #[tokio::test]
    async fn text_channel_returns_conversational_directive_without_parking() {
        let pair = intake_channel();
        let tool = RequestIdentityIntakeTool::new(false, pair.request_tx, pair.outcome_rx);
        let out = tool
            .execute(args(json!({
                "fields": ["email", {"key": "phone", "required": false}],
                "reason": "to send you the quote"
            })))
            .await
            .expect("directive");
        let v: Value = serde_json::from_str(&out).expect("json");
        assert_eq!(v["mode"], "conversational");
        assert_eq!(v["fields"][0]["key"], "email");
        assert_eq!(v["fields"][0]["required"], true, "shorthand ⇒ required");
        assert_eq!(v["fields"][1]["required"], false);
        assert!(v["instructions"]
            .as_str()
            .unwrap()
            .contains(SUBMIT_IDENTITY_INTAKE_TOOL));
    }

    #[tokio::test]
    async fn form_channel_parks_and_resumes_with_submitted_values() {
        let pair = intake_channel();
        let tool = RequestIdentityIntakeTool::new(true, pair.request_tx, pair.outcome_rx);
        let mut request_rx = pair.request_rx;
        let outcome_tx = pair.outcome_tx;

        // Host bridge: receive the raise, feed back validated values.
        let bridge = tokio::spawn(async move {
            let req = request_rx.recv().await.expect("intake raised");
            assert_eq!(req.reason, "to follow up");
            assert_eq!(req.fields.len(), 1);
            outcome_tx
                .send(IntakeOutcome::Submitted(IntakeValues {
                    email: Some("a@b.co".into()),
                    ..Default::default()
                }))
                .expect("send outcome");
        });

        let out = tool
            .execute(args(
                json!({ "fields": ["email"], "reason": "to follow up" }),
            ))
            .await
            .expect("resumed");
        bridge.await.expect("bridge");
        let v: Value = serde_json::from_str(&out).expect("json");
        assert_eq!(v["status"], "submitted");
        assert_eq!(v["values"]["email"], "a@b.co");
    }

    #[tokio::test]
    async fn form_channel_decline_resumes_with_declined_payload() {
        let pair = intake_channel();
        let tool = RequestIdentityIntakeTool::new(true, pair.request_tx, pair.outcome_rx);
        let mut request_rx = pair.request_rx;
        let outcome_tx = pair.outcome_tx;
        tokio::spawn(async move {
            let _ = request_rx.recv().await;
            let _ = outcome_tx.send(IntakeOutcome::Declined);
        });
        let out = tool
            .execute(args(json!({ "fields": ["email"], "reason": "r" })))
            .await
            .expect("resumed");
        let v: Value = serde_json::from_str(&out).expect("json");
        assert_eq!(v["status"], "declined");
    }

    #[tokio::test]
    async fn form_channel_timeout_degrades_to_no_response() {
        let pair = intake_channel();
        let tool = RequestIdentityIntakeTool::new(true, pair.request_tx, pair.outcome_rx)
            .with_timeout(Duration::from_millis(20));
        // Keep the receiver alive but never answer.
        let _request_rx = pair.request_rx;
        let _outcome_tx = pair.outcome_tx;
        let out = tool
            .execute(args(json!({ "fields": ["name"], "reason": "r" })))
            .await
            .expect("degrades, not errors");
        let v: Value = serde_json::from_str(&out).expect("json");
        assert_eq!(v["status"], "no_response");
    }

    #[tokio::test]
    async fn unknown_field_key_is_an_error() {
        let pair = intake_channel();
        let tool = RequestIdentityIntakeTool::new(false, pair.request_tx, pair.outcome_rx);
        let err = tool
            .execute(args(json!({ "fields": ["ssn"], "reason": "r" })))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown intake field"));
    }

    #[tokio::test]
    async fn submit_tool_validates_and_attaches() {
        let attached: Arc<std::sync::Mutex<Option<IntakeValues>>> =
            Arc::new(std::sync::Mutex::new(None));
        let sink = Arc::clone(&attached);
        let tool = SubmitIdentityIntakeTool::new().with_attach(Arc::new(move |v| {
            *sink.lock().unwrap() = Some(v.clone());
        }));

        // Bad email → tool error the model relays.
        let err = tool
            .execute(json!({ "email": "not-an-email" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("email"));
        assert!(attached.lock().unwrap().is_none(), "no attach on failure");

        // Good values → normalized payload + attach callback.
        let out = tool
            .execute(json!({ "email": "A@b.CO", "phone": "555-123-4567" }))
            .await
            .expect("valid");
        let v: Value = serde_json::from_str(&out).expect("json");
        assert_eq!(v["status"], "submitted");
        assert_eq!(v["values"]["email"], "A@b.co");
        assert_eq!(v["values"]["phone"], "+15551234567");
        let got = attached.lock().unwrap().clone().expect("attached");
        assert_eq!(got.phone.as_deref(), Some("+15551234567"));
    }

    #[tokio::test]
    async fn submit_tool_declined_and_empty_paths() {
        let tool = SubmitIdentityIntakeTool::new();
        let out = tool.execute(json!({ "declined": true })).await.expect("ok");
        assert!(out.contains("declined"));

        let err = tool.execute(json!({})).await.unwrap_err();
        assert!(err.to_string().contains("at least one"));
    }
}
