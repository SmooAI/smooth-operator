---
"@smooai/smooth-operator": patch
---

TS server: OTP / session-identity seam parity with the Rust reference (pearl th-8078dd).

Brings `typescript/server` to parity with the Rust server's end-user OTP / session-identity seam (#132). The native TS server can now offer a one-time-code identity-verification flow behind a public agent's `end_user` tool auth gate, without holding any credentials itself.

- New host seam `OtpService` (`typescript/server/src/otp.ts`) with `sendOtp` / `verifyOtp`, mirroring the shape of the server's other pluggable seams (`AgentConfigResolver`, `SessionAuthenticator`). Installed via the `otpService` server option; absent → unchanged fail-closed behavior (the `end_user` gate refuses and no OTP is offered). The server never generates, delivers, or validates a code — the host owns generation, delivery, expiry, and attempt counting.
- When a turn's auth gate refuses an `end_user` tool on an unverified session and an `OtpService` is installed and the session has a contact, the server emits `otp_verification_required`, calls `sendOtp`, and emits `otp_sent` — in that order, before the terminal `eventual_response`.
- New `verify_otp` action validates a submitted code: a `verified` outcome marks the session identity-verified and emits `otp_verified`; a non-verified outcome emits `otp_invalid` with the host's remaining-attempt count. No service installed → fail closed (`otp_invalid` / `NOT_FOUND`).
- The session's OTP-verified bit is tracked on the session store (`contactEmail` captured at create-session time, `otpVerified` set by `verify_otp`) and threaded into the `end_user` auth gate, so a verified caller's gated tools run on the re-sent message. Admin refusals are never offered OTP.

The server does not park/auto-resume the original turn; the client re-sends its message after `otp_verified`. Four protocol event builders + the shared `spec/conformance/fixtures.json` OTP fixtures + exhaustive tests (verify_otp happy/invalid/no-service/unknown-session/missing-fields, offer-flow event order, admin-not-offered, verified-session tool execution) added.
