"""End-user identity verification (OTP) — the host seam that lets a public agent's
``end_user``-gated tools offer a one-time-code identity flow, while the reference
server stays credential-free.

The Python analog of the Rust ``otp.rs`` seam. A public chat agent may gate certain
tools behind ``end_user`` auth (``agent_config``): the tool only runs once the
caller's identity is verified. The reference server does **not** generate, deliver,
or validate OTP codes — that is the host's job (it owns the code store, expiry,
attempt counting, and the email/SMS delivery channel). This module is the **hook**:
the server defines the :class:`OtpService` seam + the value types on the wire; a host
plugs a concrete service in via ``ServerState.otp_service`` / ``FrameDispatcher(...,
otp_service=...)``.

With no service installed the server behaves exactly as before — the auth gate
fail-closed-refuses an ``end_user`` tool and no OTP is ever offered.

## Flow the server drives around this seam

1. A turn calls an ``end_user`` tool on an unverified session; the gate refuses it
   and records the tool name. The server sees an :class:`OtpService` is installed and
   the session has a :class:`OtpContact`, so it emits ``otp_verification_required``,
   calls :meth:`OtpService.send_otp`, and emits ``otp_sent``.
2. The client submits the code via a ``verify_otp`` action. The server calls
   :meth:`OtpService.verify_otp`: an :class:`OtpVerified` outcome marks the session
   authenticated (``otp_verified``); an :class:`OtpInvalid` outcome is surfaced as
   ``otp_invalid`` with the host's remaining-attempt count.

The server never holds a code: generation, expiry, and attempt accounting are
entirely the host's, opaque behind ``send_otp`` / ``verify_otp``.
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from dataclasses import dataclass
from enum import Enum


class OtpChannel(str, Enum):
    """A delivery channel for an OTP code. Its ``value`` is the wire string the
    ``otp-sent`` / ``otp-verification-required`` schemas use (``email`` / ``sms``)."""

    EMAIL = "email"
    SMS = "sms"


class OtpError(str, Enum):
    """Machine-readable reason an OTP attempt failed. Its ``value`` is the enum the
    ``otp-invalid`` schema documents."""

    INVALID_CODE = "INVALID_CODE"
    MAX_ATTEMPTS = "MAX_ATTEMPTS"
    NOT_FOUND = "NOT_FOUND"
    EXPIRED = "EXPIRED"


@dataclass(frozen=True)
class OtpContact:
    """The contact points the server knows for a session's caller, handed to
    :meth:`OtpService.send_otp` so the host can deliver a code. The reference
    create-session path captures only an email; a host that also captures a phone
    gets an SMS channel for free."""

    email: str | None = None
    phone: str | None = None

    @property
    def is_empty(self) -> bool:
        """``True`` when neither an email nor a phone is known — the server can't
        offer OTP for this session (no channel to deliver a code to)."""
        return self.email is None and self.phone is None

    def available_channels(self) -> list[OtpChannel]:
        """The channels a code could go to, given the known contacts — email first,
        then SMS. Empty when :attr:`is_empty`. Surfaced as ``availableChannels`` in
        ``otp_verification_required`` so the client can offer the user a choice."""
        channels: list[OtpChannel] = []
        if self.email is not None:
            channels.append(OtpChannel.EMAIL)
        if self.phone is not None:
            channels.append(OtpChannel.SMS)
        return channels


@dataclass(frozen=True)
class OtpDelivery:
    """Acknowledgement returned by :meth:`OtpService.send_otp`: which channel the
    code went to and a masked destination safe to show the user (e.g.
    ``j***@example.com``). Surfaced verbatim as ``otp_sent.data.data``."""

    channel: OtpChannel
    masked_destination: str


@dataclass(frozen=True)
class OtpVerified:
    """The code was correct; the session is now identity-verified. The server marks
    the session authenticated and emits ``otp_verified``."""


@dataclass(frozen=True)
class OtpInvalid:
    """The code was rejected. Carries how many attempts remain (0 ⇒ locked, the
    client must restart the flow), an optional machine-readable reason, and a
    human-readable message for the verification UI. A richer type than a bare bool
    because the ``otp_invalid`` wire schema *requires* ``attemptsRemaining`` +
    ``message``, which only the host (owner of the code store) can supply."""

    attempts_remaining: int
    message: str
    error: OtpError | None = None


#: Outcome of an :meth:`OtpService.verify_otp` call — :class:`OtpVerified` or
#: :class:`OtpInvalid` (mirrors the Rust ``OtpVerifyOutcome`` enum).
OtpVerifyOutcome = OtpVerified | OtpInvalid


class OtpService(ABC):
    """Host seam for end-user OTP identity verification. Implemented by the host (it
    owns code generation, delivery, expiry, and attempt counting); the reference
    server only orchestrates the wire flow around it.

    Installing one turns the fail-closed ``end_user`` auth gate into an OTP-offered
    flow. Leaving it unset (``None``) keeps the current behavior — a refused
    ``end_user`` tool with no verification offered."""

    @abstractmethod
    async def send_otp(self, session_id: str, contact: OtpContact) -> OtpDelivery:
        """Generate and deliver a fresh OTP code for ``session_id`` to one of the
        caller's ``contact`` points. Returns the channel + a masked destination for
        the ``otp_sent`` acknowledgement, or raises if delivery failed."""
        ...

    @abstractmethod
    async def verify_otp(self, session_id: str, code: str) -> OtpVerifyOutcome:
        """Validate a submitted ``code`` for ``session_id``. The host owns the code
        store, expiry, and attempt accounting; the server treats the result as opaque
        and reflects it onto the wire."""
        ...
