/**
 * End-user identity verification (OTP) — the host seam behind a public agent's
 * `end_user` tool auth, while the reference server stays credential-free.
 *
 * The TypeScript port of the Rust reference server's `smooth_operator::otp`. A
 * public chat agent may gate certain tools behind `end_user` auth ({@link gateTools}):
 * the tool only runs once the caller's identity is verified. The reference server
 * does NOT generate, deliver, or validate OTP codes — that is the host's job (it
 * owns the code store, expiry, attempt counting, and the email/SMS delivery
 * channel). This module is the seam: the server defines the {@link OtpService}
 * interface + the wire value types; a host plugs in a concrete service via the
 * `otpService` server option.
 *
 * With no service installed the server behaves exactly as before — the auth gate
 * fail-closed-refuses an `end_user` tool and no OTP is ever offered. Mirrors the
 * shape of the server's other pluggable seams ({@link AgentConfigResolver},
 * {@link SessionAuthenticator}).
 *
 * ## Flow the server drives around this interface
 *
 * 1. A turn calls an `end_user` tool on an unverified session; the auth gate
 *    refuses it. The server sees an {@link OtpService} is installed and the session
 *    has a {@link OtpContact}, so it emits `otp_verification_required`, calls
 *    {@link OtpService.sendOtp}, and emits `otp_sent`.
 * 2. The client submits the received code via a `verify_otp` action. The server
 *    calls {@link OtpService.verifyOtp}: a `verified` outcome marks the session
 *    authenticated (`otp_verified`); a non-verified outcome surfaces as
 *    `otp_invalid` with the host's remaining attempts.
 *
 * The server never holds a code: generation, expiry, and attempt accounting are
 * entirely the host's, opaque behind `sendOtp` / `verifyOtp`.
 */

/** A delivery channel for an OTP code. Serializes to the `email` / `sms` strings
 *  the wire schemas (`otp-sent`, `otp-verification-required`) use. */
export type OtpChannel = 'email' | 'sms';

/** Machine-readable reason an OTP attempt failed (the `otp-invalid` schema enum). */
export type OtpError = 'INVALID_CODE' | 'MAX_ATTEMPTS' | 'NOT_FOUND' | 'EXPIRED';

/**
 * The contact points the server knows for a session's caller, handed to
 * {@link OtpService.sendOtp} so the host can deliver a code. The reference
 * create-session path captures only an email; a host that also captures a phone
 * gets an SMS channel for free.
 */
export interface OtpContact {
    /** The caller's email address, when known. */
    email?: string;
    /** The caller's phone number, when known. */
    phone?: string;
}

/**
 * Acknowledgement returned by {@link OtpService.sendOtp}: which channel the code
 * went to and a masked destination safe to show the user (e.g. `j***@example.com`).
 * Surfaced verbatim as `otp_sent.data.data`.
 */
export interface OtpDelivery {
    /** The channel the code was delivered through. */
    channel: OtpChannel;
    /** A partially masked destination for display — enough for the user to
     *  recognize their own address without exposing it in full. */
    maskedDestination: string;
}

/**
 * Outcome of an {@link OtpService.verifyOtp} call. `verified: true` marks the
 * session authenticated (server emits `otp_verified`); `verified: false` emits
 * `otp_invalid` carrying the host-supplied attempt count, optional reason, and a
 * human-readable message (which only the host, owner of the code store, can
 * supply — the `otp_invalid` schema *requires* `attemptsRemaining` + `message`).
 */
export type OtpVerifyOutcome =
    | { verified: true }
    | {
          verified: false;
          /** Remaining attempts before the code is locked; 0 means locked. */
          attemptsRemaining: number;
          /** Machine-readable reason, when the host can determine one. */
          error?: OtpError;
          /** Human-readable failure message for the verification UI. */
          message: string;
      };

/**
 * Host seam for end-user OTP identity verification. Implemented by the host (it
 * owns code generation, delivery, expiry, and attempt counting); the reference
 * server only orchestrates the wire flow around it.
 *
 * Installing one via the `otpService` server option turns the fail-closed
 * `end_user` auth gate into an OTP-offered flow. Leaving it unset keeps the
 * current behavior — a refused `end_user` tool with no verification offered.
 */
export interface OtpService {
    /** Generate and deliver a fresh OTP code for `sessionId` to one of the caller's
     *  `contact` points. Returns the channel + a masked destination for the
     *  `otp_sent` acknowledgement, or throws if delivery failed. */
    sendOtp(sessionId: string, contact: OtpContact): Promise<OtpDelivery> | OtpDelivery;
    /** Validate a submitted `code` for `sessionId`. The host owns the code store,
     *  expiry, and attempt accounting; the server treats the result as opaque and
     *  reflects it onto the wire. */
    verifyOtp(sessionId: string, code: string): Promise<OtpVerifyOutcome> | OtpVerifyOutcome;
}

/** `true` when neither an email nor a phone is known — the server can't offer OTP
 *  for this session (no channel to deliver a code to). */
export function isContactEmpty(contact: OtpContact): boolean {
    return contact.email === undefined && contact.phone === undefined;
}

/**
 * The channels a code could be delivered to, given the known contacts — email
 * first, then SMS. Empty when {@link isContactEmpty}. Surfaced as
 * `availableChannels` in `otp_verification_required` so the client can offer the
 * user a choice.
 */
export function availableChannels(contact: OtpContact): OtpChannel[] {
    const channels: OtpChannel[] = [];
    if (contact.email !== undefined) channels.push('email');
    if (contact.phone !== undefined) channels.push('sms');
    return channels;
}

/**
 * Records the `end_user` tool an auth gate refused for lack of a verified session
 * during a turn, so the dispatcher can offer OTP after the turn. The TS analog of
 * the Rust `AuthGateHook::otp_refused_tool`. Admin refusals are NOT recorded here
 * (an admin refusal is not OTP-remediable). One fresh recorder per turn.
 */
export interface OtpRefusal {
    /** The name of the refused `end_user` tool, set at execution time when the gate
     *  blocks it for lack of verification. `undefined` → nothing to offer OTP for. */
    refusedTool?: string;
}
