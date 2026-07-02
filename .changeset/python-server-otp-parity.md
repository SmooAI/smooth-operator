---
"@smooai/smooth-operator": patch
---

Python server: OTP / session-identity seam parity with the Rust reference (SMOODEV pearl th-8078dd).

Brings the Python operator server to behavioral parity with the Rust server's end-user OTP identity-verification seam (landed for Rust in #132). Like the reference, the Python server never generates, delivers, or validates a code — a new host seam, `OtpService` (`smooth_operator_server.otp`, with `OtpContact` / `OtpDelivery` / `OtpChannel` / `OtpError` / `OtpVerified` / `OtpInvalid`), owns generation, delivery, expiry, and attempt counting. Install one via `ServerState.otp_service` (or `FrameDispatcher(..., otp_service=...)`); absent (the default), behavior is unchanged — the `end_user` auth gate fail-closed-refuses and no OTP is offered.

- When a turn's gate refuses an `end_user` tool on an unverified session and an `OtpService` is installed and the session has a contact (the caller's email, captured at create-session time), the server emits `otp_verification_required`, calls `send_otp`, and emits `otp_sent` — in that order, before the terminal `eventual_response`. An `admin` refusal is never OTP-remediable, so it is not offered.
- A new `verify_otp` action validates a submitted code via `OtpService.verify_otp`: an `OtpVerified` outcome marks the session identity-verified (persisted on the session store) and emits `otp_verified`; an `OtpInvalid` outcome emits `otp_invalid` with the host's remaining-attempt count and optional machine-readable reason. Validation order mirrors Rust (requestId, sessionId, code required; unknown session → `SESSION_NOT_FOUND`; no service → fail closed `otp_invalid` / `NOT_FOUND`).
- Per-session verified state is tracked on the session store and threaded into the tool auth gate as the resolved `session_authenticated` bit (the session's OTP-verified state OR'd with the existing `SessionAuthenticator` seam), so a verified caller's `end_user` tools run.

The reference server does not park/auto-resume the original turn; the client re-sends after `otp_verified`. The four OTP event builders reproduce the shared conformance fixtures byte-for-byte; exhaustive tests cover verify happy/invalid/no-service/unknown-session/missing-field, the offer flow's emission order, admin-not-offered, no-contact/no-service/send-failure edges, and a verified session running the gated tool.
