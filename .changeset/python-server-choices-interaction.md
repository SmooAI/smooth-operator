---
'@smooai/smooth-operator-server': minor
---

Port the Rich Interactions runtime + the `choices` kind (AskUserQuestion) to the **Python** server (Wave 2 of the polyglot effort), mirroring the Rust reference (PR #475).

New kind-agnostic framework (`interaction.py`): `InteractionKind` (kind / capability / tool_schema / parse_request / validate / fallback_directive), an `InteractionRegistry` catalog (default: `choices`), and a session-keyed `PendingInteractions` park/resume registry that generalizes the write-confirmation `ConfirmationRegistry`. Each turn registers per-kind `request_<kind>` raise tools — parking on a channel that declared the kind's render capability in `supports` (emit `interaction_required`, await `submit_interaction`, resume with the canonical payload), or degrading to the kind's conversational directive on text-only channels, where the model submits through the generic `submit_interaction` tool. A new `submit_interaction` dispatcher action routes values to the kind validator: invalid values emit retryable `interaction_invalid` (turn stays parked), valid values resume the turn.

The `choices` kind (`choices.py`) mirrors `choices.rs`: `request_choices { questions (1–4), reason }` with 2–4 options and an optional `multiSelect`, the shared `validate_choices` (every question answered, labels ∈ options, single-select one pick XOR `other`, multi-select ≥1, blank `other` dropped, all errors in one pass), the enumerated fallback directive, and capability id `choice_chips`. Validated against the shared `spec/interactions/choices.schema.json` + conformance fixtures.
