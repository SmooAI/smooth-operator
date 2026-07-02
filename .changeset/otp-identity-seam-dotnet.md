---
"@smooai/smooth-operator": patch
---

C# server: OTP / session-identity seam parity with the Rust reference (SMOODEV pearl th-8078dd).

Brings the .NET reference server (`SmooAI.SmoothOperator.Server`) to behavioral parity with the Rust server's OTP / session-identity seam (PR #132), so a public agent's `end_user`-gated tools can offer a one-time-code identity flow while the server stays credential-free.

- New host seam `IOtpService` (`SendOtpAsync(sessionId, contact) -> OtpDelivery`; `VerifyOtpAsync(sessionId, code) -> OtpVerifyOutcome.Verified | Invalid`) with the `OtpChannel` / `OtpContact` / `OtpDelivery` / `OtpError` value types. Registered via DI; absent ⇒ unchanged (the `end_user` gate fail-closed-refuses and no OTP is offered).
- When a turn's auth gate refuses an `end_user` tool on an unverified session, an `IOtpService` is installed, and the session has a contact, the server emits `otp_verification_required`, calls `SendOtpAsync`, and emits `otp_sent` — before the terminal response. Admin refusals are never offered OTP.
- New `verify_otp` action: a `Verified` outcome marks the session identity-verified (`otp_verified`); an `Invalid` outcome emits `otp_invalid` with the host's remaining-attempt count. Validation order mirrors Rust (requestId → sessionId → code → session-exists → service); no service installed ⇒ fail closed (`otp_invalid` / `NOT_FOUND`).
- Per-conversation verified state is persisted in the session store and threaded into the auth gate via a store-backed `ISessionAuthenticator` default (replacing the hardcoded deny-all), so a verified caller's `end_user` tools run. The caller's email contact is captured at create-session time. Both are backed in the in-memory and Postgres stores with a shared contract test.

The reference server does not park/auto-resume the original turn; the client re-sends its message after `otp_verified`. Event shapes validate against the same `spec/events/otp-*.schema.json`.
