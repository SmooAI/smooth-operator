---
'@smooai/smooth-operator': minor
---

Add the `choices` Rich Interaction kind (a structured multiple-choice ask, modeled on Claude Code's AskUserQuestion) as the second reference kind in the Rust implementation, plus its shared JSON-Schema contract the other servers + web SDK mirror.

An agent raises `request_choices` with 1–4 questions, each `{ question, header (short ≤12-char label), options: [{ label, description }] (2–4), multiSelect? }` and a `reason`. On a channel that declared the **`choice_chips`** capability the turn parks and the client renders chips/menus (`interaction_required { kind: "choices" }`); on text/voice channels the same raise degrades to an enumerated conversational directive. Every question carries an implicit free-text `other` escape hatch (mirroring AskUserQuestion's ever-present "Other"), so the visitor can always answer outside the enumerated options.

Server-side validation (`validate_choices`, shared by the card path's WS handler and the fallback path's `submit_interaction` tool): every question answered, each selected label offered, single-select takes exactly one pick (label XOR `other`), multi-select one or more; invalid submits return retryable per-question `interaction_invalid` errors, never a terminal error. `ChoicesKind` is registered in the default `InteractionRegistry`. The canonical contract lives in `spec/interactions/choices.schema.json` (Spec / Values / Payload) with conformance fixtures; the other-language servers follow as parity work.
