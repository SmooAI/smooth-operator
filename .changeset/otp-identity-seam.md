---
"@smooai/smooth-operator": minor
---

Server-side OTP / session-identity seam so hosts can wire end-user tool auth (SMOODEV pearl th-8e8a89).

The Rust reference server can now offer a one-time-code identity-verification flow behind a public agent's `end_user` tool auth gate, without holding any credentials itself. A new host seam, `OtpService` (`smooth_operator::otp`), owns code generation, delivery, expiry, and attempt counting; the reference server only orchestrates the wire flow around it. Install one via `AppState::with_otp_service`; absent, behavior is unchanged (the `end_user` gate fail-closed-refuses and no OTP is offered).

- When a turn's auth gate refuses an `end_user` tool on an unverified session and an `OtpService` is installed and the session has a contact, the server emits `otp_verification_required`, calls `send_otp`, and emits `otp_sent`.
- A new `verify_otp` action validates a submitted code via `OtpService::verify_otp`: a `Verified` outcome marks the session identity-verified and emits `otp_verified`; an `Invalid` outcome emits `otp_invalid` with the host's remaining-attempt count. With no service installed, verification fails closed (`otp_invalid` / `NOT_FOUND`).
- Per-session verified state is tracked in session metadata and threaded into the auth gate as the real `session_authenticated` bit (previously hardcoded `false`), so a verified caller's `end_user` tools run.

The reference server does not park/auto-resume the original turn; the client re-sends its message after `otp_verified`. Rust-only for now (mirrors how per-agent config landed as separate per-language PRs); parity in the Python/TS/Go/.NET servers is follow-up work.
