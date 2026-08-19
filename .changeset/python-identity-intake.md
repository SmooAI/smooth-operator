---
'@smooai/smooth-operator-server': patch
---

Port the `identity_intake` Rich Interaction kind to the Python server, plus the kind-routed host-effect seam the `choices` wave omitted.

`identity_intake` is structured name/email/phone lead capture — the second interaction kind (after `choices`), mirroring the Rust reference. Its `request_identity_intake` raise tool (`{ fields, reason }`) parks the turn on channels that declare the `identity_form` capability and degrades to a conversational directive on text-only channels; both paths run one server-side validator (required fields present, email shape, phone normalized to E.164, per-field errors) and resume with the same canonical payload.

New framework piece: a kind-agnostic **host effect** (`InteractionKind.host_effect`, a no-op by default) fires on a valid submit on BOTH paths — the dispatcher's `submit_interaction` action and the conversational-fallback `submit_interaction` tool. `identity_intake` overrides it to stamp the captured contacts onto the session (`user_name` / `contact_email` / `contact_phone` — the same keys the pre-chat create path stashes and the OTP contact seam reads), so a captured contact is immediately OTP-verifiable (email and/or SMS). `choices` is unaffected. Registered in the default interaction catalog alongside `choices`.
