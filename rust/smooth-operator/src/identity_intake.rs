//! Identity intake — channel-normalized lead/identity capture: the first
//! (reference) **Rich Interaction kind** (see `docs/Architecture/Rich Interactions.md`
//! and [`crate::interaction`]).
//!
//! - On a channel that declared the `identity_form` capability, the agent's
//!   `request_identity_intake` tool parks the turn and the server emits
//!   `interaction_required { kind: "identity_intake" }`; the client's form
//!   resumes with a `submit_interaction` action.
//! - On a **text-only** channel, the same raise degrades to a conversational
//!   directive and the model submits collected values through the generic
//!   `submit_interaction` *tool*.
//!
//! Both paths validate through [`validate_intake`] — one implementation, one
//! behavior — and resume the turn with the same structured payload.
//! [`IdentityIntakeKind`] packages it all as an
//! [`InteractionKind`](crate::interaction::InteractionKind).

use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use smooth_operator_core::tool::ToolSchema;

use crate::interaction::{InteractionFieldError, InteractionKind, InteractionRequest};

/// The closed set of identity fields intake can collect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IntakeFieldKey {
    /// The visitor's display name.
    Name,
    /// The visitor's email address.
    Email,
    /// The visitor's phone number (normalized to E.164).
    Phone,
}

impl IntakeFieldKey {
    /// The wire / prompt name of this field.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Email => "email",
            Self::Phone => "phone",
        }
    }
}

/// One requested identity field, as raised by the agent's
/// `request_identity_intake` tool and carried on the
/// `identity_intake_required` event (`spec/events/identity-intake-required.schema.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntakeField {
    /// Which identity field to collect.
    pub key: IntakeFieldKey,
    /// Whether the visitor must provide this field to submit.
    #[serde(default)]
    pub required: bool,
    /// Optional display label overriding the client's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Validated, normalized identity values — the structured payload the parked
/// turn resumes with (identical on the form and conversational paths).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntakeValues {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// E.164-normalized (`+15551234567`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone: Option<String>,
}

impl IntakeValues {
    /// True when no field carries a value.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.name.is_none() && self.email.is_none() && self.phone.is_none()
    }
}

/// A single per-field validation failure, carried on the
/// `identity_intake_invalid` event and in the conversational tool's error
/// result (`spec/events/identity-intake-invalid.schema.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntakeFieldError {
    /// The field that failed.
    pub field: IntakeFieldKey,
    /// Human-readable validation message.
    pub message: String,
}

/// Validate raw submitted values against the requested `fields`, returning the
/// normalized [`IntakeValues`] or the full list of per-field errors.
///
/// Rules (see the design doc):
/// - every `required` field must be present and non-blank,
/// - `name`: non-empty after trim,
/// - `email`: `local@domain.tld` shape (single `@`, dot in the domain, no
///   whitespace); domain lowercased,
/// - `phone`: E.164 after stripping separators (`+` + 8–15 digits); bare
///   10-digit or 1-prefixed 11-digit NANP numbers are accepted and normalized
///   to `+1…`.
///
/// Fields that were *not* requested but are present are still validated and
/// kept — a visitor volunteering their phone is a gift, not an error.
///
/// # Errors
/// Returns every failed field (not just the first) so a form can annotate all
/// of them in one round-trip.
pub fn validate_intake(
    fields: &[IntakeField],
    values: &IntakeValues,
) -> Result<IntakeValues, Vec<IntakeFieldError>> {
    let mut errors = Vec::new();
    let mut out = IntakeValues::default();

    let get = |key: IntakeFieldKey| -> Option<&str> {
        match key {
            IntakeFieldKey::Name => values.name.as_deref(),
            IntakeFieldKey::Email => values.email.as_deref(),
            IntakeFieldKey::Phone => values.phone.as_deref(),
        }
    };

    // Required-ness: every required requested field must be present + non-blank.
    for field in fields {
        if field.required && get(field.key).is_none_or(|v| v.trim().is_empty()) {
            errors.push(IntakeFieldError {
                field: field.key,
                message: "this field is required".to_string(),
            });
        }
    }

    // Format validation + normalization for whatever was provided.
    if let Some(name) = values.name.as_deref() {
        let trimmed = name.trim();
        if !trimmed.is_empty() {
            out.name = Some(trimmed.to_string());
        }
    }
    if let Some(email) = values.email.as_deref() {
        let trimmed = email.trim();
        if !trimmed.is_empty() {
            match normalize_email(trimmed) {
                Some(normalized) => out.email = Some(normalized),
                None => errors.push(IntakeFieldError {
                    field: IntakeFieldKey::Email,
                    message: "must be a valid email address".to_string(),
                }),
            }
        }
    }
    if let Some(phone) = values.phone.as_deref() {
        let trimmed = phone.trim();
        if !trimmed.is_empty() {
            match normalize_phone_e164(trimmed) {
                Some(normalized) => out.phone = Some(normalized),
                None => errors.push(IntakeFieldError {
                    field: IntakeFieldKey::Phone,
                    message:
                        "must be a valid phone number (include your country code, e.g. +1 555 123 4567)"
                            .to_string(),
                }),
            }
        }
    }

    if errors.is_empty() {
        Ok(out)
    } else {
        Err(errors)
    }
}

/// Minimal email-shape validation: exactly one `@`, non-empty local part, a
/// dot-containing domain, no whitespace. Returns the trimmed address with a
/// lowercased domain, or `None` when malformed.
///
/// ponytail: shape check, not RFC 5322 — a deliverability check belongs to the
/// host's email service, not the protocol boundary.
#[must_use]
pub fn normalize_email(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.chars().any(char::is_whitespace) {
        return None;
    }
    let (local, domain) = s.split_once('@')?;
    if local.is_empty() || domain.contains('@') {
        return None;
    }
    // Domain needs an interior dot: `a.b`, not `.b`, `a.`, or `ab`.
    let domain_lc = domain.to_ascii_lowercase();
    let mut parts = domain_lc.split('.');
    if domain_lc.split('.').count() < 2 || parts.any(str::is_empty) {
        return None;
    }
    Some(format!("{local}@{domain_lc}"))
}

/// Normalize a phone number to E.164, or `None` when unparseable.
///
/// Strips common separators (space, `-`, `.`, `(`, `)`), then accepts:
/// - `+` + 8–15 digits (already E.164),
/// - a bare 10-digit number or a 1-prefixed 11-digit number → `+1…` (NANP).
///
/// ponytail: NANP default for bare national numbers; swap in the `phonenumber`
/// crate if non-NANP national formats ever need to parse.
#[must_use]
pub fn normalize_phone_e164(raw: &str) -> Option<String> {
    let s: String = raw
        .trim()
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '.' | '(' | ')'))
        .collect();
    let (plus, digits) = match s.strip_prefix('+') {
        Some(rest) => (true, rest),
        None => (false, s.as_str()),
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if plus {
        // E.164: country code can't start with 0; total 8–15 digits.
        if (8..=15).contains(&digits.len()) && !digits.starts_with('0') {
            return Some(format!("+{digits}"));
        }
        return None;
    }
    match digits.len() {
        10 => Some(format!("+1{digits}")),
        11 if digits.starts_with('1') => Some(format!("+{digits}")),
        _ => None,
    }
}

/// Parse the raise tool's `fields` argument. Accepts both the structured form
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

/// The `identity_intake` Rich Interaction kind — the reference implementation
/// of [`InteractionKind`] (see the module docs and
/// `spec/interactions/identity-intake.schema.json`).
pub struct IdentityIntakeKind;

impl InteractionKind for IdentityIntakeKind {
    fn kind(&self) -> &'static str {
        "identity_intake"
    }

    fn capability(&self) -> &'static str {
        "identity_form"
    }

    fn tool_schema(&self) -> ToolSchema {
        ToolSchema {
            name: "request_identity_intake".to_string(),
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

    fn parse_request(&self, args: &Value) -> anyhow::Result<InteractionRequest> {
        let fields = parse_fields(args.get("fields").unwrap_or(&Value::Null))?;
        let reason = args
            .get("reason")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("to help you better")
            .to_string();
        Ok(InteractionRequest {
            kind: self.kind().to_string(),
            spec: json!({ "fields": fields }),
            reason,
        })
    }

    fn validate(&self, spec: &Value, values: &Value) -> Result<Value, Vec<InteractionFieldError>> {
        // The spec's fields drive required-ness; a Null/absent spec (fallback
        // raise from an earlier turn) degrades to format-only validation.
        let fields: Vec<IntakeField> = spec
            .get("fields")
            .cloned()
            .and_then(|f| serde_json::from_value(f).ok())
            .unwrap_or_default();
        let values: IntakeValues = serde_json::from_value(values.clone()).map_err(|e| {
            vec![InteractionFieldError {
                field: "values".to_string(),
                message: format!("invalid values shape: {e}"),
            }]
        })?;
        if values.is_empty() {
            return Err(vec![InteractionFieldError {
                field: "values".to_string(),
                message: "provide at least one of name/email/phone, or declined=true".to_string(),
            }]);
        }
        match validate_intake(&fields, &values) {
            Ok(validated) => Ok(serde_json::to_value(validated).unwrap_or(Value::Null)),
            Err(errors) => Err(errors
                .into_iter()
                .map(|e| InteractionFieldError {
                    field: e.field.as_str().to_string(),
                    message: e.message,
                })
                .collect()),
        }
    }

    fn fallback_directive(&self, spec: &Value, reason: &str) -> String {
        let field_list = spec
            .get("fields")
            .and_then(Value::as_array)
            .map(|fields| {
                fields
                    .iter()
                    .filter_map(|f| f.get("key").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        format!(
            "This visitor's channel cannot display a form. Collect the requested details \
             ({field_list}) conversationally: ask for ONE field at a time, in the order given, \
             naturally weaving in the reason ({reason}). When you have the values, call the \
             `submit_interaction` tool with kind \"identity_intake\" and the values — it \
             validates each field and will tell you if something looks wrong so you can re-ask. \
             If the visitor declines to share, call `submit_interaction` with declined=true and \
             continue helping them without the details."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(key: IntakeFieldKey, required: bool) -> IntakeField {
        IntakeField {
            key,
            required,
            label: None,
        }
    }

    #[test]
    fn email_shapes() {
        assert_eq!(
            normalize_email("Alice@Example.COM").as_deref(),
            Some("Alice@example.com"),
            "domain lowercased, local case preserved"
        );
        for bad in [
            "",
            "no-at",
            "@x.com",
            "a@b",
            "a@.com",
            "a@b.",
            "a b@c.com",
            "a@b@c.com",
        ] {
            assert_eq!(normalize_email(bad), None, "{bad:?} should be rejected");
        }
    }

    #[test]
    fn phone_shapes() {
        assert_eq!(
            normalize_phone_e164("+1 (555) 123-4567").as_deref(),
            Some("+15551234567")
        );
        assert_eq!(
            normalize_phone_e164("555.123.4567").as_deref(),
            Some("+15551234567"),
            "bare 10-digit NANP"
        );
        assert_eq!(
            normalize_phone_e164("1 555 123 4567").as_deref(),
            Some("+15551234567"),
            "1-prefixed 11-digit NANP"
        );
        assert_eq!(
            normalize_phone_e164("+447911123456").as_deref(),
            Some("+447911123456"),
            "non-NANP with country code"
        );
        for bad in ["", "abc", "+0123456789", "12345", "+1234567890123456"] {
            assert_eq!(
                normalize_phone_e164(bad),
                None,
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn required_field_missing_is_an_error() {
        let fields = [
            field(IntakeFieldKey::Email, true),
            field(IntakeFieldKey::Name, false),
        ];
        let err = validate_intake(&fields, &IntakeValues::default()).unwrap_err();
        assert_eq!(err.len(), 1);
        assert_eq!(err[0].field, IntakeFieldKey::Email);

        // Blank counts as missing.
        let blank = IntakeValues {
            email: Some("   ".into()),
            ..Default::default()
        };
        assert!(validate_intake(&fields, &blank).is_err());
    }

    #[test]
    fn valid_submit_normalizes() {
        let fields = [
            field(IntakeFieldKey::Email, true),
            field(IntakeFieldKey::Phone, false),
        ];
        let values = IntakeValues {
            name: Some("  Alice Example  ".into()),
            email: Some("alice@Example.com".into()),
            phone: Some("(555) 123-4567".into()),
        };
        let out = validate_intake(&fields, &values).expect("valid");
        assert_eq!(out.name.as_deref(), Some("Alice Example"));
        assert_eq!(out.email.as_deref(), Some("alice@example.com"));
        assert_eq!(out.phone.as_deref(), Some("+15551234567"));
    }

    #[test]
    fn all_errors_reported_in_one_pass() {
        let fields = [field(IntakeFieldKey::Name, true)];
        let values = IntakeValues {
            name: None,
            email: Some("not-an-email".into()),
            phone: Some("nope".into()),
        };
        let err = validate_intake(&fields, &values).unwrap_err();
        assert_eq!(
            err.len(),
            3,
            "missing name + bad email + bad phone: {err:?}"
        );
    }

    #[test]
    fn volunteered_fields_are_kept() {
        // Only email requested, but the visitor volunteered a phone — keep it.
        let fields = [field(IntakeFieldKey::Email, true)];
        let values = IntakeValues {
            email: Some("a@b.co".into()),
            phone: Some("+15551234567".into()),
            ..Default::default()
        };
        let out = validate_intake(&fields, &values).expect("valid");
        assert_eq!(out.phone.as_deref(), Some("+15551234567"));
    }
}
