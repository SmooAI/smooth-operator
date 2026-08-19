---
'@smooai/smooth-operator-server': minor
---

Add the `identity_intake` Rich Interaction kind to the TypeScript server (structured name/email/phone lead capture) — the port of the Rust reference `identity_intake.rs`, hosted by default alongside `choices`. On an `identity_form`-capable channel the `request_identity_intake` tool parks the turn on a form; on a text-only channel it degrades to validated conversational collection. Both paths run the shared validator (required-fields present, email shape, phone normalized to E.164, per-field errors) and resume with the same canonical payload.

Also adds the framework's kind-agnostic **host-effect seam**: a valid `submit_interaction` (both the rich WS path and the conversational-fallback tool) now routes to a kind-specific host effect. For `identity_intake` this stamps the captured contacts onto the session metadata (`userName` / `contactEmail` / `contactPhone`) — the same keys the pre-chat create path stashes and the OTP contact seam reads, so a captured contact is immediately OTP-contactable. Kinds with no effect (e.g. `choices`) are unaffected.
