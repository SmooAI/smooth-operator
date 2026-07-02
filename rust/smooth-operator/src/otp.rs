//! End-user identity verification (OTP) — the seam that lets a host wire a
//! one-time-code flow behind a public agent's `end_user` tool auth, while the
//! reference server stays credential-free.
//!
//! A public chat agent may gate certain tools behind [`AuthLevel::EndUser`]
//! (`agent_config`): the tool only runs once the caller's identity is verified.
//! The reference server does not generate, deliver, or validate OTP codes — that
//! is the host's job (it owns the code store, expiry, attempt counting, and the
//! email/SMS delivery channel). This module is the **hook**: the public server
//! defines the [`OtpService`] trait + the value types on the wire; a host
//! application plugs in a concrete service via `AppState::with_otp_service`.
//!
//! With no service installed the server behaves exactly as before — the auth
//! gate fail-closed-refuses an `end_user` tool and no OTP is ever offered.
//!
//! ## Flow the server drives around this trait
//!
//! 1. A turn calls an `end_user` tool on an unverified session; the auth gate
//!    refuses it. The server sees an [`OtpService`] is installed and the session
//!    has a [contact](OtpContact), so it emits `otp_verification_required`, calls
//!    [`send_otp`](OtpService::send_otp), and emits `otp_sent`.
//! 2. The client submits the code the user received via a `verify_otp` action.
//!    The server calls [`verify_otp`](OtpService::verify_otp): a
//!    [`Verified`](OtpVerifyOutcome::Verified) outcome marks the session
//!    authenticated (`otp_verified`); an [`Invalid`](OtpVerifyOutcome::Invalid)
//!    outcome is surfaced as `otp_invalid` with the remaining attempts.
//!
//! The server never holds a code: generation, expiry, and attempt accounting are
//! entirely the host's, opaque behind `send_otp` / `verify_otp`.

use async_trait::async_trait;

/// A delivery channel for an OTP code. Serializes to the `email` / `sms` strings
/// the wire schemas (`otp-sent`, `otp-verification-required`) use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtpChannel {
    /// Deliver the code to the caller's email address.
    Email,
    /// Deliver the code to the caller's phone number by SMS.
    Sms,
}

impl OtpChannel {
    /// The wire string for this channel (`"email"` / `"sms"`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Email => "email",
            Self::Sms => "sms",
        }
    }
}

/// The contact points the server knows for a session's caller, handed to
/// [`OtpService::send_otp`] so the host can deliver a code. The reference
/// create-session path captures only an email (see the handler); a host that
/// also captures a phone gets an SMS channel for free.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OtpContact {
    /// The caller's email address, when known.
    pub email: Option<String>,
    /// The caller's phone number, when known.
    pub phone: Option<String>,
}

impl OtpContact {
    /// `true` when neither an email nor a phone is known — the server can't offer
    /// OTP for this session (no channel to deliver a code to).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.email.is_none() && self.phone.is_none()
    }

    /// The channels a code could be delivered to, given the known contacts —
    /// email first, then SMS. Empty when [`is_empty`](Self::is_empty). Surfaced
    /// as `availableChannels` in `otp_verification_required` so the client can
    /// offer the user a choice.
    #[must_use]
    pub fn available_channels(&self) -> Vec<OtpChannel> {
        let mut channels = Vec::new();
        if self.email.is_some() {
            channels.push(OtpChannel::Email);
        }
        if self.phone.is_some() {
            channels.push(OtpChannel::Sms);
        }
        channels
    }
}

/// Acknowledgement returned by [`OtpService::send_otp`]: which channel the code
/// went to and a masked destination safe to show the user (e.g.
/// `j***@example.com`). Surfaced verbatim as `otp_sent.data.data`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtpDelivery {
    /// The channel the code was delivered through.
    pub channel: OtpChannel,
    /// A partially masked destination for display — enough for the user to
    /// recognize their own address without exposing it in full.
    pub masked_destination: String,
}

/// Machine-readable reason an OTP attempt failed. Serializes to the enum the
/// `otp-invalid` schema documents (`INVALID_CODE` / `MAX_ATTEMPTS` / `NOT_FOUND`
/// / `EXPIRED`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtpError {
    /// The code entered did not match.
    InvalidCode,
    /// Too many failed attempts — the record is locked; a new code is required.
    MaxAttempts,
    /// No active verification record for this session.
    NotFound,
    /// The code expired before it was submitted.
    Expired,
}

impl OtpError {
    /// The wire string for this error (`"INVALID_CODE"`, …).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidCode => "INVALID_CODE",
            Self::MaxAttempts => "MAX_ATTEMPTS",
            Self::NotFound => "NOT_FOUND",
            Self::Expired => "EXPIRED",
        }
    }
}

/// Outcome of an [`OtpService::verify_otp`] call. On [`Verified`](Self::Verified)
/// the server marks the session authenticated and emits `otp_verified`; on
/// [`Invalid`](Self::Invalid) it emits `otp_invalid` carrying the host-supplied
/// attempt count, reason, and message. A richer type than a bare `bool` because
/// the `otp_invalid` wire schema *requires* `attemptsRemaining` + `message`,
/// which only the host (owner of the code store) can supply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OtpVerifyOutcome {
    /// The code was correct; the session is now identity-verified.
    Verified,
    /// The code was rejected. Carries how many attempts remain (0 ⇒ locked, the
    /// client must restart the flow), an optional machine-readable reason, and a
    /// human-readable message for the verification UI.
    Invalid {
        /// Remaining attempts before the code is locked; 0 means locked.
        attempts_remaining: u32,
        /// Machine-readable reason, when the host can determine one.
        error: Option<OtpError>,
        /// Human-readable failure message for the UI.
        message: String,
    },
}

/// Host seam for end-user OTP identity verification. Implemented by the host
/// application (it owns code generation, delivery, expiry, and attempt
/// counting); the reference server only orchestrates the wire flow around it.
///
/// Installing one via `AppState::with_otp_service` turns the fail-closed
/// `end_user` auth gate into an OTP-offered flow. Leaving it unset keeps the
/// current behavior — a refused `end_user` tool with no verification offered.
#[async_trait]
pub trait OtpService: Send + Sync {
    /// Generate and deliver a fresh OTP code for `session_id` to one of the
    /// caller's `contact` points. Returns the channel + a masked destination for
    /// the `otp_sent` acknowledgement, or an error if delivery failed.
    async fn send_otp(&self, session_id: &str, contact: &OtpContact)
        -> anyhow::Result<OtpDelivery>;

    /// Validate a submitted `code` for `session_id`. The host owns the code
    /// store, expiry, and attempt accounting; the server treats the result as
    /// opaque and simply reflects it onto the wire.
    async fn verify_otp(&self, session_id: &str, code: &str) -> OtpVerifyOutcome;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_wire_strings() {
        assert_eq!(OtpChannel::Email.as_str(), "email");
        assert_eq!(OtpChannel::Sms.as_str(), "sms");
    }

    #[test]
    fn error_wire_strings() {
        assert_eq!(OtpError::InvalidCode.as_str(), "INVALID_CODE");
        assert_eq!(OtpError::MaxAttempts.as_str(), "MAX_ATTEMPTS");
        assert_eq!(OtpError::NotFound.as_str(), "NOT_FOUND");
        assert_eq!(OtpError::Expired.as_str(), "EXPIRED");
    }

    #[test]
    fn empty_contact_offers_no_channels() {
        let contact = OtpContact::default();
        assert!(contact.is_empty());
        assert!(contact.available_channels().is_empty());
    }

    #[test]
    fn email_only_contact_offers_email() {
        let contact = OtpContact {
            email: Some("a@example.com".into()),
            phone: None,
        };
        assert!(!contact.is_empty());
        assert_eq!(contact.available_channels(), vec![OtpChannel::Email]);
    }

    #[test]
    fn both_contacts_offer_email_then_sms() {
        let contact = OtpContact {
            email: Some("a@example.com".into()),
            phone: Some("+15551234567".into()),
        };
        assert_eq!(
            contact.available_channels(),
            vec![OtpChannel::Email, OtpChannel::Sms]
        );
    }

    #[test]
    fn phone_only_contact_offers_sms() {
        let contact = OtpContact {
            email: None,
            phone: Some("+15551234567".into()),
        };
        assert_eq!(contact.available_channels(), vec![OtpChannel::Sms]);
    }
}
