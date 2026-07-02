---
"@smooai/smooth-operator": patch
---

Go server: OTP / session-identity seam parity for end-user tool auth (th-8078dd).

Brings the Go reference server to parity with the Rust server's OTP / session-identity seam (PR #132). A public agent's `end_user`-gated tools can now offer a one-time-code identity flow, while the Go server stays credential-free — it never generates, delivers, or validates a code.

- New `OtpService` seam (`SendOtp` / `VerifyOtp`) plus the `OtpContact`, `OtpDelivery`, `OtpChannel`, `OtpErrorCode`, and `OtpVerifyOutcome` value types, mirroring the existing resolver seams. Installed via `server.WithOtpService`; absent ⇒ unchanged fail-closed behavior (the gate refuses, no OTP offered).
- The session's OTP-verified bit (`StoredSession.OtpVerified`, set by a successful `verify_otp`) is threaded into the auth gate so a verified caller's `end_user` tools run.
- On an `end_user` refusal, with a service installed and a session contact captured at create-session time, the server emits `otp_verification_required`, calls `SendOtp`, and emits `otp_sent` (before the terminal `eventual_response`, matching the Rust ordering). `admin` refusals are never offered OTP.
- New `verify_otp` action: validation order `requestId → sessionId → code → session-exists → no-service`; a correct code emits `otp_verified` and marks the session authenticated, a rejected code emits `otp_invalid` with the host's remaining attempts, and no installed service fails closed (`otp_invalid` / `NOT_FOUND`).

Semantics match the Rust reference exactly. Exhaustive tests (seam types, verify_otp happy/invalid/no-service/unknown-session/missing-fields, offer-flow event order, admin-not-offered, verified-session-runs-tool); server events validate against the shared `spec/events/*` schemas.
