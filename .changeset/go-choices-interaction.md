---
'@smooai/smooth-operator': patch
---

Port the Rich Interactions runtime + the `choices` kind (AskUserQuestion) to the **Go** LocalServer, mirroring the Rust reference (PR #475) — wave 2 of the polyglot rollout.

The Go server now hosts a kind-agnostic interaction framework (`InteractionKind` / `InteractionKinds` catalog / a per-connection park-resume `InteractionRegistry`, the analog of the write-confirmation `ConfirmationRegistry`) plus the `choices` kind. Each turn registers one `request_<kind>` raise tool per hosted kind: on a session that declared the kind's render capability (`supports` at `create_conversation_session`) the raise **parks the turn** — the tool blocks awaiting a `submit_interaction` while the server emits `interaction_required` — and on a text-only channel it degrades to the kind's conversational fallback directive. A new `submit_interaction` dispatcher action routes the visitor's values to the kind's server-side validator: invalid → retryable `interaction_invalid` (the turn stays parked), valid → the parked raise resumes with the canonical payload. The `choices` validator (`validateChoices`) enforces the same rules as the Rust reference and validates against the shared `spec/interactions/choices.schema.json` conformance fixtures. Capability id: `choice_chips`.
