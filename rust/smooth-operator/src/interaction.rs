//! Rich Interactions — the extensible structured-interaction framework.
//!
//! One pattern, many kinds (see `docs/Architecture/Rich Interactions.md`):
//! an agent raises a **structured interaction** (identity intake, a date
//! picker, choice chips, …). On a channel whose client declared the kind's
//! render capability (`supports` at `create_conversation_session`), the turn
//! parks and the client renders a rich card (`interaction_required` →
//! `submit_interaction`). On a text-only channel the same raise degrades to the
//! kind's **conversational fallback**: a directive the model follows turn by
//! turn, submitting through the generic `submit_interaction` *tool*. Both
//! paths run the kind's **server-side validator** and resume the turn with the
//! same canonical payload.
//!
//! Adding a kind = implementing [`InteractionKind`] (one module), registering
//! it in an [`InteractionRegistry`], and adding its spec schema under
//! `spec/interactions/`. No new protocol events, no new client verbs.
//!
//! The first (and reference) kind is
//! [`IdentityIntakeKind`](crate::identity_intake::IdentityIntakeKind).

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use smooth_operator_core::tool::ToolSchema;

/// A single per-field validation failure, carried on the `interaction_invalid`
/// event and in the conversational tool's error result. `field` is a
/// kind-specific field key (identity_intake: `name` / `email` / `phone`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InteractionFieldError {
    /// The kind-specific field that failed.
    pub field: String,
    /// Human-readable validation message.
    pub message: String,
}

/// A parsed raise: what the agent asked the visitor for.
#[derive(Debug, Clone)]
pub struct InteractionRequest {
    /// The interaction kind (e.g. `identity_intake`).
    pub kind: String,
    /// The kind-specific render spec (shape per `spec/interactions/<kind>.schema.json#/$defs/Spec`).
    pub spec: Value,
    /// Why the agent raised it (card header / woven into the conversational ask).
    pub reason: String,
}

/// How a parked interaction resolved.
#[derive(Debug, Clone)]
pub enum InteractionOutcome {
    /// The visitor submitted values; already validated + canonicalized by the
    /// kind's [`InteractionKind::validate`].
    Submitted { values: Value },
    /// The visitor declined the interaction.
    Declined,
}

/// One interaction kind — the extension seam of the Rich Interactions pattern.
///
/// A kind supplies exactly the pieces that differ per interaction; ALL park /
/// resume / event / registry machinery is shared and kind-agnostic:
///
/// 1. identity (`kind` / `capability`),
/// 2. the LLM-facing raise-tool surface (`tool_schema` + `parse_request`) —
///    per-kind so the model sees a precise parameter schema,
/// 3. the **server-side validator** (`validate`) producing the canonical
///    values (the same payload on rich and fallback channels),
/// 4. the **conversational degradation** (`fallback_directive`) for channels
///    without the render capability.
pub trait InteractionKind: Send + Sync {
    /// The wire kind id (e.g. `identity_intake`). Selects the client card and
    /// the validator; the kind catalog lives in `spec/interactions/`.
    fn kind(&self) -> &'static str;

    /// The client render capability that gates the rich path (e.g.
    /// `identity_form`). A session that declared it in `supports` gets the
    /// parked card; anything else gets the conversational fallback.
    fn capability(&self) -> &'static str;

    /// The raise tool's LLM-facing schema (per-kind, so parameters stay
    /// precise). Convention: name it `request_<kind>`.
    fn tool_schema(&self) -> ToolSchema;

    /// Parse + canonicalize the raise tool's arguments into the kind's `spec`
    /// (carried on `interaction_required`) and the human-readable reason.
    ///
    /// # Errors
    /// On malformed arguments; the error text is surfaced to the model.
    fn parse_request(&self, args: &Value) -> anyhow::Result<InteractionRequest>;

    /// Validate submitted values against `spec`, returning the canonical
    /// (normalized) values or the full list of per-field errors. `spec` may be
    /// `Value::Null` on the conversational path when the raise happened in an
    /// earlier turn — the kind should then apply format-only validation.
    ///
    /// # Errors
    /// Every failed field (not just the first), so a card can annotate all of
    /// them in one round-trip.
    fn validate(&self, spec: &Value, values: &Value) -> Result<Value, Vec<InteractionFieldError>>;

    /// The conversational-degradation directive for text-only channels:
    /// instructions the model follows to collect the same information turn by
    /// turn (identity: field-by-field ask; choices: enumerated ask; date:
    /// natural-language ask). The framework tells the model to submit through
    /// the `submit_interaction` tool, which routes back into [`validate`](Self::validate).
    fn fallback_directive(&self, spec: &Value, reason: &str) -> String;
}

/// The set of interaction kinds a server hosts. The default registry contains
/// the reference kinds ([`IdentityIntakeKind`](crate::identity_intake::IdentityIntakeKind));
/// a host may extend or replace it.
#[derive(Clone)]
pub struct InteractionRegistry {
    kinds: Vec<Arc<dyn InteractionKind>>,
}

impl InteractionRegistry {
    /// An empty registry (no interactions hosted).
    #[must_use]
    pub fn empty() -> Self {
        Self { kinds: Vec::new() }
    }

    /// Register a kind (builder). Later registrations with the same `kind()`
    /// shadow earlier ones on lookup order — don't do that.
    #[must_use]
    pub fn with(mut self, kind: Arc<dyn InteractionKind>) -> Self {
        self.kinds.push(kind);
        self
    }

    /// Look up a kind by its wire id.
    #[must_use]
    pub fn get(&self, kind: &str) -> Option<Arc<dyn InteractionKind>> {
        self.kinds.iter().find(|k| k.kind() == kind).cloned()
    }

    /// Every registered kind, in registration order.
    #[must_use]
    pub fn kinds(&self) -> &[Arc<dyn InteractionKind>] {
        &self.kinds
    }
}

impl Default for InteractionRegistry {
    /// The reference catalog: `identity_intake`.
    fn default() -> Self {
        Self::empty().with(Arc::new(crate::identity_intake::IdentityIntakeKind))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_registry_hosts_identity_intake() {
        let reg = InteractionRegistry::default();
        let kind = reg
            .get("identity_intake")
            .expect("identity_intake registered");
        assert_eq!(kind.capability(), "identity_form");
        assert_eq!(kind.tool_schema().name, "request_identity_intake");
        assert!(reg.get("date_picker").is_none(), "unknown kinds are None");
    }
}
