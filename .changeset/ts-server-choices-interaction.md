---
'@smooai/smooth-operator-server': minor
---

Port the Rich Interactions runtime + the `choices` kind (AskUserQuestion) to the TypeScript server, mirroring the Rust reference.

A kind-agnostic framework (`interaction.ts`): the `InteractionKind` seam, an `InteractionRegistry` of hosted kinds, a session-keyed `InteractionParkRegistry` (the interaction analog of the write-confirmation registry), and the per-kind `request_<kind>` raise tool + generic `submit_interaction` fallback tool. An agent raise parks the turn when the session declared the kind's render capability (`supports` at `create_conversation_session`) — the raise tool awaits inside `execute`, the server emits `interaction_required { interactionId, kind, spec, reason }`, and a `submit_interaction` action resolves it. On a text-only channel the same raise degrades to the kind's conversational-fallback directive. Both paths run the kind's server-side validator and resume with the same canonical payload.

The `choices` kind (`choices.ts`, mirroring `choices.rs`): `request_choices` with 1–4 questions (each a short ≤12-char header, 2–4 options, optional `multiSelect`) and a `reason`; the `validate_choices` rules (every question answered, each label offered, single-select takes exactly one pick label-XOR-other, multi-select one or more, blank `other` dropped, one-pass errors); capability id `choice_chips`. Invalid submits emit a retryable `interaction_invalid` event and keep the turn parked (never a terminal error). The session's declared `supports` is now persisted (in-memory + Postgres stores) and gates the rich-vs-fallback decision per kind. The server hosts the `choices` kind by default. Validated against the shared `spec/conformance/fixtures.json` `choices` fixtures.
