package server

import (
	"context"
	"sync"
)

// End-user identity verification (OTP) — the host seam that lets a public agent's
// end_user-gated tools offer a one-time-code identity flow, while the Go reference
// server stays credential-free (it never generates, delivers, or validates a code).
//
// The Go analog of the Rust smooth_operator::otp module (rust/smooth-operator/src/otp.rs).
// A host installs a concrete OtpService via WithOtpService; with none installed the
// server behaves exactly as before — the auth gate fail-closed-refuses an end_user tool
// and no OTP is ever offered. Mirrors the existing resolver seams here (AgentConfigResolver,
// SessionAuthenticator): an interface with an in-memory-free reference default (nil).
//
// Flow the server drives around this seam (see dispatcher.go):
//  1. A turn calls an end_user tool on an unverified session; the auth gate refuses it and
//     records the tool. With an OtpService installed and a session contact, the server emits
//     otp_verification_required, calls SendOtp, and emits otp_sent.
//  2. The client submits the code via a verify_otp action. The server calls VerifyOtp: a
//     Verified outcome marks the session authenticated (otp_verified); an Invalid outcome is
//     surfaced as otp_invalid with the host-supplied remaining attempts.
//
// The server never holds a code: generation, expiry, and attempt accounting are the host's,
// opaque behind SendOtp / VerifyOtp.

// OtpChannel is a delivery channel for an OTP code. Its string value is the wire token the
// otp_sent / otp_verification_required schemas use.
type OtpChannel string

const (
	// OtpChannelEmail delivers the code to the caller's email address.
	OtpChannelEmail OtpChannel = "email"
	// OtpChannelSMS delivers the code to the caller's phone number by SMS.
	OtpChannelSMS OtpChannel = "sms"
)

// OtpContact is the contact points the server knows for a session's caller, handed to
// SendOtp so the host can deliver a code. An empty string means "unknown". The reference
// create-session path captures only an email; a host that also captures a phone gets an SMS
// channel for free.
type OtpContact struct {
	Email string
	Phone string
}

// IsEmpty reports that neither an email nor a phone is known — the server can't offer OTP
// for this session (no channel to deliver a code to).
func (c OtpContact) IsEmpty() bool { return c.Email == "" && c.Phone == "" }

// AvailableChannels are the channels a code could be delivered to, given the known contacts
// — email first, then SMS. Empty when IsEmpty. Surfaced as availableChannels in
// otp_verification_required so the client can offer the user a choice.
func (c OtpContact) AvailableChannels() []OtpChannel {
	var channels []OtpChannel
	if c.Email != "" {
		channels = append(channels, OtpChannelEmail)
	}
	if c.Phone != "" {
		channels = append(channels, OtpChannelSMS)
	}
	return channels
}

// OtpDelivery is the acknowledgement SendOtp returns: which channel the code went to and a
// masked destination safe to show the user (e.g. j***@example.com). Surfaced verbatim as
// otp_sent.data.data.
type OtpDelivery struct {
	Channel           OtpChannel
	MaskedDestination string
}

// OtpErrorCode is a machine-readable reason an OTP attempt failed. Its string value is the
// enum the otp-invalid schema documents. An empty value means the host couldn't determine a
// cause (the error key is then omitted on the wire).
type OtpErrorCode string

const (
	// OtpErrorInvalidCode — the code entered did not match.
	OtpErrorInvalidCode OtpErrorCode = "INVALID_CODE"
	// OtpErrorMaxAttempts — too many failed attempts; the record is locked, a new code is required.
	OtpErrorMaxAttempts OtpErrorCode = "MAX_ATTEMPTS"
	// OtpErrorNotFound — no active verification record for this session.
	OtpErrorNotFound OtpErrorCode = "NOT_FOUND"
	// OtpErrorExpired — the code expired before it was submitted.
	OtpErrorExpired OtpErrorCode = "EXPIRED"
)

// OtpVerifyOutcome is the result of a VerifyOtp call — the Go analog of the Rust
// OtpVerifyOutcome enum (Verified | Invalid{...}). A richer type than a bare bool because the
// otp_invalid wire schema requires attemptsRemaining + message, which only the host (owner of
// the code store) can supply. Build with Verified() / Invalid(); a zero value is a rejected
// attempt with no attempts left.
type OtpVerifyOutcome struct {
	// OK is true when the code was correct; the session is now identity-verified.
	OK bool
	// AttemptsRemaining is how many attempts remain before the code is locked (0 ⇒ locked, the
	// client must restart the flow). Only meaningful when OK is false.
	AttemptsRemaining int
	// Error is an optional machine-readable reason ("" ⇒ omitted). Only meaningful when OK is false.
	Error OtpErrorCode
	// Message is a human-readable failure message for the verification UI. Only meaningful when OK is false.
	Message string
}

// Verified is the success outcome: the code was correct.
func Verified() OtpVerifyOutcome { return OtpVerifyOutcome{OK: true} }

// Invalid is a rejected outcome carrying the host's remaining-attempt count, an optional
// machine-readable reason ("" to omit), and a human-readable message.
func Invalid(attemptsRemaining int, errCode OtpErrorCode, message string) OtpVerifyOutcome {
	return OtpVerifyOutcome{OK: false, AttemptsRemaining: attemptsRemaining, Error: errCode, Message: message}
}

// OtpService is the host seam for end-user OTP identity verification. The host owns code
// generation, delivery, expiry, and attempt counting; the reference server only orchestrates
// the wire flow around it. Installing one via WithOtpService turns the fail-closed end_user
// auth gate into an OTP-offered flow; leaving it unset keeps the current behavior.
type OtpService interface {
	// SendOtp generates and delivers a fresh OTP code for sessionID to one of the caller's
	// contact points, returning the channel + a masked destination for the otp_sent
	// acknowledgement, or an error if delivery failed.
	SendOtp(ctx context.Context, sessionID string, contact OtpContact) (OtpDelivery, error)
	// VerifyOtp validates a submitted code for sessionID. The host owns the code store, expiry,
	// and attempt accounting; the server treats the result as opaque and reflects it onto the wire.
	VerifyOtp(ctx context.Context, sessionID, code string) OtpVerifyOutcome
}

// otpRefusal records the end_user tool an auth gate refused for lack of a verified session
// during a turn — the one refusal an OTP flow can remedy (an admin refusal never can). The
// Go analog of the Rust AuthGateHook.otp_refused_tool (Arc<Mutex<Option<String>>>): a per-turn
// recorder the gate writes (from the turn goroutine) and the dispatcher reads after the turn
// (from its goroutine) to decide whether to offer OTP — so it is mutex-guarded. A nil recorder
// is a safe no-op (nothing to offer).
type otpRefusal struct {
	mu   sync.Mutex
	tool string
}

// record marks tool as an OTP-remediable refusal (last write wins, mirroring the Rust hook).
// A nil recorder is a no-op.
func (r *otpRefusal) record(tool string) {
	if r == nil {
		return
	}
	r.mu.Lock()
	defer r.mu.Unlock()
	r.tool = tool
}

// refusedTool returns the end_user tool refused for lack of verification this turn, or "" if
// none — the dispatcher's signal to offer OTP. Safe on a nil recorder.
func (r *otpRefusal) refusedTool() string {
	if r == nil {
		return ""
	}
	r.mu.Lock()
	defer r.mu.Unlock()
	return r.tool
}

// authenticatedSession is a SessionAuthenticator that always reports authenticated. The
// dispatcher installs it for a session whose server-owned otpVerified bit is set (from a prior
// successful verify_otp), so a verified caller's end_user tools run regardless of any host seam
// — the Go analog of threading Rust's metadata.otpVerified bit into build_auth_gate.
type authenticatedSession struct{}

func (authenticatedSession) IsAuthenticated(context.Context, string) (bool, error) { return true, nil }
