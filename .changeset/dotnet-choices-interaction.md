---
'@smooai/smooth-operator': patch
---

Port the Rich Interactions runtime + the `choices` kind (AskUserQuestion) to the .NET (C#) server, at parity with the Rust reference.

The C# server now hosts a kind-agnostic interaction framework (`IInteractionKind` / `InteractionCatalog` / a session-keyed `InteractionParkRegistry` generalizing the write-confirmation park/resume) and the `choices` kind (`request_choices` raise tool, `validate_choices`, conversational fallback, capability id `choice_chips`). A turn on a `choice_chips`-capable session parks emitting `interaction_required` and resumes on a `submit_interaction` frame (invalid values → retryable `interaction_invalid`, never a terminal error); text-only sessions degrade to the enumerated conversational directive and submit via the `submit_interaction` tool. Validated against the shared `spec/interactions/choices.schema.json` conformance fixtures.
