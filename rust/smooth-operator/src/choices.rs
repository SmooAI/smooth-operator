//! Choices — a structured multiple-choice ask (modeled on Claude Code's
//! `AskUserQuestion`): the second **Rich Interaction kind** (see
//! `docs/Architecture/Rich Interactions.md` and [`crate::interaction`]).
//!
//! The agent asks 1–4 short questions, each with 2–4 labeled options; the turn
//! parks until the visitor picks. Every question also carries an implicit
//! free-text **"Other"** escape hatch, so the visitor can answer outside the
//! enumerated options (exactly as `AskUserQuestion` always offers "Other").
//!
//! - On a channel that declared the `choice_chips` capability, the agent's
//!   `request_choices` tool parks the turn and the server emits
//!   `interaction_required { kind: "choices" }`; the client's chip/menu card
//!   resumes with a `submit_interaction` action.
//! - On a **text-only** channel the same raise degrades to a conversational
//!   directive that enumerates the questions + options ("choices = enumerated
//!   ask") and the model submits the picks through the generic
//!   `submit_interaction` *tool*.
//!
//! Both paths validate through [`validate_choices`] — one implementation, one
//! behavior — and resume the turn with the same structured payload.
//! [`ChoicesKind`] packages it all as an [`InteractionKind`](crate::interaction::InteractionKind).

use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use smooth_operator_core::tool::ToolSchema;

use crate::interaction::{InteractionFieldError, InteractionKind, InteractionRequest};

/// Max length of a question's short `header` label (chip/tab caption).
pub const HEADER_MAX_CHARS: usize = 12;

/// One selectable option in a question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChoiceOption {
    /// The option's label — the value the visitor submits.
    pub label: String,
    /// A short human-readable gloss shown under/next to the label.
    #[serde(default)]
    pub description: String,
}

/// One question in a [`choices`](ChoicesKind) raise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChoiceQuestion {
    /// The question prompt shown to the visitor.
    pub question: String,
    /// A short label (≤[`HEADER_MAX_CHARS`] chars) — the answer key and the
    /// chip/tab caption. Must be unique within a raise.
    pub header: String,
    /// The 2–4 enumerated options. An implicit free-text "Other" is always
    /// available in addition to these.
    pub options: Vec<ChoiceOption>,
    /// Whether the visitor may pick more than one option (default `false`).
    #[serde(default, rename = "multiSelect")]
    pub multi_select: bool,
}

/// The visitor's answer to one question, submitted via `submit_interaction`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChoiceAnswer {
    /// Which question this answers — matches the spec question's `header`.
    pub header: String,
    /// The selected option label(s). One for single-select; the empty vec when
    /// the visitor only used the free-text "Other" escape hatch.
    #[serde(default)]
    pub options: Vec<String>,
    /// The free-text "Other" answer, when the visitor answered outside the
    /// enumerated options. Blank ⇒ omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other: Option<String>,
}

impl ChoiceAnswer {
    /// Total picks the visitor made (selected labels + one for a non-blank
    /// "Other").
    fn selection_count(&self) -> usize {
        self.options.len() + usize::from(self.other.is_some())
    }
}

/// Validated, normalized choice answers — the structured payload the parked
/// turn resumes with (identical on the card and conversational paths).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChoiceValues {
    /// One entry per answered question (in submission order).
    #[serde(default)]
    pub answers: Vec<ChoiceAnswer>,
}

/// A single per-question validation failure. `field` is the question's `header`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChoiceFieldError {
    /// The question `header` that failed.
    pub field: String,
    /// Human-readable validation message.
    pub message: String,
}

/// Validate submitted `values` against the raised `questions`, returning the
/// normalized [`ChoiceValues`] or the full list of per-question errors.
///
/// Rules (see the design doc):
/// - **every** question must be answered (a selection or a non-blank "Other");
/// - each selected label must be one of that question's option labels;
/// - single-select (`multiSelect: false`): exactly one pick (one label XOR
///   "Other"); multi-select: one or more picks (labels and/or "Other");
/// - a blank/whitespace "Other" is treated as absent; labels are trimmed.
///
/// When `questions` is empty (a prior-turn fallback raise whose spec is gone),
/// validation degrades to **format-only**: labels can't be checked for
/// membership, so any answer with at least one pick is accepted as-is.
///
/// # Errors
/// Returns every failed question (not just the first) so a card can annotate
/// all of them in one round-trip.
pub fn validate_choices(
    questions: &[ChoiceQuestion],
    values: &ChoiceValues,
) -> Result<ChoiceValues, Vec<ChoiceFieldError>> {
    // Normalize the raw answers first (trim labels + "Other", drop blanks).
    let normalized: Vec<ChoiceAnswer> = values
        .answers
        .iter()
        .map(|a| ChoiceAnswer {
            header: a.header.trim().to_string(),
            options: a
                .options
                .iter()
                .map(|o| o.trim().to_string())
                .filter(|o| !o.is_empty())
                .collect(),
            other: a
                .other
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        })
        .collect();

    // Format-only path: no spec to check membership/required-ness against.
    if questions.is_empty() {
        let mut errors = Vec::new();
        for answer in &normalized {
            if answer.selection_count() == 0 {
                errors.push(ChoiceFieldError {
                    field: answer.header.clone(),
                    message: "select an option or provide an 'other' answer".to_string(),
                });
            }
        }
        if normalized.is_empty() {
            errors.push(ChoiceFieldError {
                field: "answers".to_string(),
                message: "provide an answer for each question, or declined=true".to_string(),
            });
        }
        return if errors.is_empty() {
            Ok(ChoiceValues {
                answers: normalized,
            })
        } else {
            Err(errors)
        };
    }

    let mut errors = Vec::new();
    let mut out = Vec::with_capacity(questions.len());

    for question in questions {
        let Some(answer) = normalized.iter().find(|a| a.header == question.header) else {
            errors.push(ChoiceFieldError {
                field: question.header.clone(),
                message: "this question must be answered".to_string(),
            });
            continue;
        };

        // Every selected label must be one of the enumerated options.
        let mut bad_label = false;
        for label in &answer.options {
            if !question.options.iter().any(|o| &o.label == label) {
                bad_label = true;
                errors.push(ChoiceFieldError {
                    field: question.header.clone(),
                    message: format!("'{label}' is not one of the offered options"),
                });
            }
        }

        let count = answer.selection_count();
        if count == 0 {
            errors.push(ChoiceFieldError {
                field: question.header.clone(),
                message: "select an option or provide an 'other' answer".to_string(),
            });
        } else if !question.multi_select && count > 1 {
            errors.push(ChoiceFieldError {
                field: question.header.clone(),
                message: "this question takes a single answer".to_string(),
            });
        }

        if !bad_label {
            out.push(answer.clone());
        }
    }

    if errors.is_empty() {
        Ok(ChoiceValues { answers: out })
    } else {
        Err(errors)
    }
}

/// Parse the raise tool's `questions` argument into validated [`ChoiceQuestion`]s.
///
/// Enforces the LLM-facing contract so the model produces usable cards: 1–4
/// questions, each with a non-empty prompt, a non-empty header ≤12 chars
/// (unique within the raise), and 2–4 options with non-empty labels.
fn parse_questions(raw: &Value) -> anyhow::Result<Vec<ChoiceQuestion>> {
    let items = raw
        .as_array()
        .ok_or_else(|| anyhow!("'questions' must be an array"))?;
    if !(1..=4).contains(&items.len()) {
        return Err(anyhow!(
            "'questions' must contain between 1 and 4 questions"
        ));
    }
    let mut questions = Vec::with_capacity(items.len());
    let mut seen_headers = Vec::with_capacity(items.len());
    for item in items {
        let obj = item
            .as_object()
            .ok_or_else(|| anyhow!("each question must be an object"))?;
        let question = obj
            .get("question")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("each question needs a non-empty 'question'"))?
            .to_string();
        let header = obj
            .get("header")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("each question needs a non-empty 'header'"))?
            .to_string();
        if header.chars().count() > HEADER_MAX_CHARS {
            return Err(anyhow!(
                "header '{header}' is too long (max {HEADER_MAX_CHARS} characters)"
            ));
        }
        if seen_headers.contains(&header) {
            return Err(anyhow!("duplicate question header '{header}'"));
        }
        seen_headers.push(header.clone());

        let raw_options = obj
            .get("options")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("question '{header}' needs an 'options' array"))?;
        if !(2..=4).contains(&raw_options.len()) {
            return Err(anyhow!(
                "question '{header}' must offer between 2 and 4 options"
            ));
        }
        let mut options = Vec::with_capacity(raw_options.len());
        for opt in raw_options {
            // Accept the object form `{ label, description? }` and the shorthand
            // a bare string the model sometimes emits.
            let option = match opt {
                Value::String(s) => ChoiceOption {
                    label: s.trim().to_string(),
                    description: String::new(),
                },
                Value::Object(o) => ChoiceOption {
                    label: o
                        .get("label")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .unwrap_or_default()
                        .to_string(),
                    description: o
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_string(),
                },
                other => return Err(anyhow!("invalid option entry in '{header}': {other}")),
            };
            if option.label.is_empty() {
                return Err(anyhow!("an option in '{header}' has an empty label"));
            }
            options.push(option);
        }

        questions.push(ChoiceQuestion {
            question,
            header,
            options,
            multi_select: obj
                .get("multiSelect")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        });
    }
    Ok(questions)
}

/// The `choices` Rich Interaction kind — a structured multiple-choice ask
/// modeled on `AskUserQuestion` (see the module docs and
/// `spec/interactions/choices.schema.json`).
pub struct ChoicesKind;

impl InteractionKind for ChoicesKind {
    fn kind(&self) -> &'static str {
        "choices"
    }

    fn capability(&self) -> &'static str {
        "choice_chips"
    }

    fn tool_schema(&self) -> ToolSchema {
        ToolSchema {
            name: "request_choices".to_string(),
            description: "Ask the visitor a structured multiple-choice question (1–4 questions, \
                          each with 2–4 labeled options) and wait for their pick. On channels \
                          that can render chips/menus the visitor taps an option; on text \
                          channels you will be told to enumerate the options and accept a \
                          natural-language answer. An implicit free-text \"Other\" is always \
                          available, so use this whenever the answer is likely (but not \
                          certainly) one of a small set — never free-form the menu yourself."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 4,
                        "description": "The questions to ask, in order (1–4).",
                        "items": {
                            "type": "object",
                            "properties": {
                                "question": { "type": "string", "description": "The question prompt shown to the visitor." },
                                "header": { "type": "string", "maxLength": HEADER_MAX_CHARS, "description": "A short label (≤12 chars), unique within the raise. Used as the answer key and the chip/tab caption." },
                                "options": {
                                    "type": "array",
                                    "minItems": 2,
                                    "maxItems": 4,
                                    "description": "The 2–4 options to offer. A free-text 'Other' is always available in addition.",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": { "type": "string", "description": "The option label (the value submitted)." },
                                            "description": { "type": "string", "description": "A short gloss for the option." }
                                        },
                                        "required": ["label"]
                                    }
                                },
                                "multiSelect": { "type": "boolean", "description": "Allow selecting more than one option (default false)." }
                            },
                            "required": ["question", "header", "options"]
                        }
                    },
                    "reason": {
                        "type": "string",
                        "description": "Why you're asking, phrased for the visitor (e.g. \"to route you to the right team\")."
                    }
                },
                "required": ["questions", "reason"]
            }),
        }
    }

    fn parse_request(&self, args: &Value) -> anyhow::Result<InteractionRequest> {
        let questions = parse_questions(args.get("questions").unwrap_or(&Value::Null))?;
        let reason = args
            .get("reason")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("to help you better")
            .to_string();
        Ok(InteractionRequest {
            kind: self.kind().to_string(),
            spec: json!({ "questions": questions }),
            reason,
        })
    }

    fn validate(&self, spec: &Value, values: &Value) -> Result<Value, Vec<InteractionFieldError>> {
        let questions: Vec<ChoiceQuestion> = spec
            .get("questions")
            .cloned()
            .and_then(|q| serde_json::from_value(q).ok())
            .unwrap_or_default();
        let values: ChoiceValues = serde_json::from_value(values.clone()).map_err(|e| {
            vec![InteractionFieldError {
                field: "values".to_string(),
                message: format!("invalid values shape: {e}"),
            }]
        })?;
        match validate_choices(&questions, &values) {
            Ok(validated) => Ok(serde_json::to_value(validated).unwrap_or(Value::Null)),
            Err(errors) => Err(errors
                .into_iter()
                .map(|e| InteractionFieldError {
                    field: e.field,
                    message: e.message,
                })
                .collect()),
        }
    }

    fn fallback_directive(&self, spec: &Value, reason: &str) -> String {
        let enumerated = spec
            .get("questions")
            .and_then(Value::as_array)
            .map(|questions| {
                questions
                    .iter()
                    .filter_map(|q| {
                        let question = q.get("question").and_then(Value::as_str)?;
                        let header = q.get("header").and_then(Value::as_str).unwrap_or(question);
                        let multi = q
                            .get("multiSelect")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        let opts = q
                            .get("options")
                            .and_then(Value::as_array)
                            .map(|os| {
                                os.iter()
                                    .filter_map(|o| o.get("label").and_then(Value::as_str))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            })
                            .unwrap_or_default();
                        Some(format!(
                            "- [{header}] {question} Options: {opts}{}.",
                            if multi { " (choose one or more)" } else { "" }
                        ))
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        format!(
            "This visitor's channel cannot display choice chips. Ask the following question(s) \
             conversationally, naturally weaving in the reason ({reason}), and read out each \
             option so the visitor can pick:\n{enumerated}\nThe visitor may also answer with \
             something not listed (that's fine — capture it as their 'other' answer). When you \
             have their pick(s), call the `submit_interaction` tool with kind \"choices\" and \
             `values.answers` — one entry per question `{{ header, options: [chosen label(s)], \
             other?: \"their free-text answer\" }}`. It validates each answer and will tell you \
             if a pick isn't offered so you can re-ask. If the visitor declines to choose, call \
             `submit_interaction` with declined=true and continue helping them."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opt(label: &str) -> ChoiceOption {
        ChoiceOption {
            label: label.to_string(),
            description: String::new(),
        }
    }

    fn question(header: &str, labels: &[&str], multi: bool) -> ChoiceQuestion {
        ChoiceQuestion {
            question: format!("{header}?"),
            header: header.to_string(),
            options: labels.iter().map(|l| opt(l)).collect(),
            multi_select: multi,
        }
    }

    fn answer(header: &str, options: &[&str], other: Option<&str>) -> ChoiceAnswer {
        ChoiceAnswer {
            header: header.to_string(),
            options: options.iter().map(|s| (*s).to_string()).collect(),
            other: other.map(str::to_string),
        }
    }

    #[test]
    fn valid_single_select_normalizes() {
        let qs = [question("Plan", &["Basic", "Pro"], false)];
        let values = ChoiceValues {
            answers: vec![answer("Plan", &["  Pro  "], None)],
        };
        let out = validate_choices(&qs, &values).expect("valid");
        assert_eq!(out.answers.len(), 1);
        assert_eq!(out.answers[0].options, vec!["Pro".to_string()]);
        assert!(out.answers[0].other.is_none());
    }

    #[test]
    fn valid_multi_select_keeps_all_picks() {
        let qs = [question("Topics", &["Sales", "Support", "Billing"], true)];
        let values = ChoiceValues {
            answers: vec![answer("Topics", &["Sales", "Billing"], None)],
        };
        let out = validate_choices(&qs, &values).expect("valid");
        assert_eq!(out.answers[0].options, vec!["Sales", "Billing"]);
    }

    #[test]
    fn other_escape_hatch_is_accepted() {
        let qs = [question("Plan", &["Basic", "Pro"], false)];
        let values = ChoiceValues {
            answers: vec![answer("Plan", &[], Some("  Enterprise, actually  "))],
        };
        let out = validate_choices(&qs, &values).expect("valid");
        assert!(out.answers[0].options.is_empty());
        assert_eq!(
            out.answers[0].other.as_deref(),
            Some("Enterprise, actually")
        );
    }

    #[test]
    fn unknown_label_is_a_field_error() {
        let qs = [question("Plan", &["Basic", "Pro"], false)];
        let values = ChoiceValues {
            answers: vec![answer("Plan", &["Platinum"], None)],
        };
        let err = validate_choices(&qs, &values).unwrap_err();
        assert_eq!(err.len(), 1);
        assert_eq!(err[0].field, "Plan");
        assert!(err[0].message.contains("not one of the offered"));
    }

    #[test]
    fn single_select_rejects_multiple_picks() {
        let qs = [question("Plan", &["Basic", "Pro"], false)];
        let values = ChoiceValues {
            answers: vec![answer("Plan", &["Basic", "Pro"], None)],
        };
        let err = validate_choices(&qs, &values).unwrap_err();
        assert!(err.iter().any(|e| e.message.contains("single answer")));
    }

    #[test]
    fn unanswered_question_is_required() {
        let qs = [
            question("Plan", &["Basic", "Pro"], false),
            question("Size", &["S", "M"], false),
        ];
        let values = ChoiceValues {
            answers: vec![answer("Plan", &["Pro"], None)],
        };
        let err = validate_choices(&qs, &values).unwrap_err();
        assert_eq!(err.len(), 1);
        assert_eq!(err[0].field, "Size");
        assert!(err[0].message.contains("must be answered"));
    }

    #[test]
    fn empty_answer_needs_a_pick_or_other() {
        let qs = [question("Plan", &["Basic", "Pro"], false)];
        let values = ChoiceValues {
            answers: vec![answer("Plan", &[], None)],
        };
        let err = validate_choices(&qs, &values).unwrap_err();
        assert!(err.iter().any(|e| e.message.contains("select an option")));
    }

    #[test]
    fn parse_questions_enforces_the_contract() {
        // Happy path with shorthand string options.
        let qs = parse_questions(&json!([
            { "question": "Which plan?", "header": "Plan", "options": ["Basic", "Pro"] }
        ]))
        .expect("valid");
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].options[0].label, "Basic");
        assert!(!qs[0].multi_select);

        // Too many questions.
        let too_many: Vec<Value> = (0..5)
            .map(|i| json!({ "question": "q", "header": format!("H{i}"), "options": ["a", "b"] }))
            .collect();
        assert!(parse_questions(&json!(too_many)).is_err());

        // Too few options.
        assert!(parse_questions(&json!([
            { "question": "q", "header": "H", "options": ["only"] }
        ]))
        .is_err());

        // Header too long.
        assert!(parse_questions(&json!([
            { "question": "q", "header": "ThisHeaderIsWayTooLong", "options": ["a", "b"] }
        ]))
        .is_err());

        // Duplicate headers.
        assert!(parse_questions(&json!([
            { "question": "q1", "header": "H", "options": ["a", "b"] },
            { "question": "q2", "header": "H", "options": ["a", "b"] }
        ]))
        .is_err());
    }

    #[test]
    fn kind_wires_the_reference_surface() {
        let kind = ChoicesKind;
        assert_eq!(kind.kind(), "choices");
        assert_eq!(kind.capability(), "choice_chips");
        assert_eq!(kind.tool_schema().name, "request_choices");

        let req = kind
            .parse_request(&json!({
                "questions": [
                    { "question": "Which plan interests you?", "header": "Plan",
                      "options": [{ "label": "Basic" }, { "label": "Pro" }] }
                ],
                "reason": "to route you"
            }))
            .expect("parse");
        assert_eq!(req.kind, "choices");
        assert_eq!(req.reason, "to route you");
        assert_eq!(req.spec["questions"][0]["header"], "Plan");

        // The validator, through the trait, produces the canonical values.
        let canonical = kind
            .validate(
                &req.spec,
                &json!({ "answers": [{ "header": "Plan", "options": ["Pro"] }] }),
            )
            .expect("valid");
        assert_eq!(canonical["answers"][0]["options"][0], "Pro");

        // The fallback directive enumerates the options.
        let directive = kind.fallback_directive(&req.spec, "to route you");
        assert!(directive.contains("Basic, Pro"));
        assert!(directive.contains("submit_interaction"));
    }
}
