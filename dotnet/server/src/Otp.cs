namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// End-user identity verification (OTP) — the host seam that lets a public agent's
/// <c>end_user</c>-gated tools offer a one-time-code identity flow while this reference server stays
/// credential-free. The C# analog of the Rust <c>smooth_operator::otp</c> module.
///
/// A public chat agent may gate a tool behind <c>end_user</c> auth (see <see cref="ToolAuthGate"/>):
/// the tool only runs once the caller's identity is verified. The reference server never generates,
/// delivers, or validates a code — that is the host's job (it owns the code store, expiry, attempt
/// counting, and the email/SMS channel). This interface is the hook; a host plugs in a concrete
/// service by registering it in DI.
///
/// With no service installed the server behaves exactly as before — the auth gate fail-closed-refuses
/// an <c>end_user</c> tool and no OTP is ever offered.
/// </summary>
public interface IOtpService
{
    /// <summary>
    /// Generate and deliver a fresh OTP code for <paramref name="sessionId"/> to one of the caller's
    /// <paramref name="contact"/> points. Returns the channel + a masked destination for the
    /// <c>otp_sent</c> acknowledgement, or throws if delivery failed (the server surfaces an
    /// <c>OTP_SEND_FAILED</c> error).
    /// </summary>
    Task<OtpDelivery> SendOtpAsync(string sessionId, OtpContact contact, CancellationToken cancellationToken = default);

    /// <summary>
    /// Validate a submitted <paramref name="code"/> for <paramref name="sessionId"/>. The host owns
    /// the code store, expiry, and attempt accounting; the server treats the result as opaque and
    /// reflects it onto the wire (<c>otp_verified</c> / <c>otp_invalid</c>).
    /// </summary>
    Task<OtpVerifyOutcome> VerifyOtpAsync(string sessionId, string code, CancellationToken cancellationToken = default);
}

/// <summary>A delivery channel for an OTP code. Serializes to the <c>email</c> / <c>sms</c> strings
/// the wire schemas (<c>otp-sent</c>, <c>otp-verification-required</c>) use.</summary>
public enum OtpChannel
{
    /// <summary>Deliver the code to the caller's email address.</summary>
    Email,

    /// <summary>Deliver the code to the caller's phone number by SMS.</summary>
    Sms,
}

/// <summary>Machine-readable reason an OTP attempt failed. Serializes to the enum the
/// <c>otp-invalid</c> schema documents.</summary>
public enum OtpError
{
    /// <summary>The code entered did not match.</summary>
    InvalidCode,

    /// <summary>Too many failed attempts — the record is locked; a new code is required.</summary>
    MaxAttempts,

    /// <summary>No active verification record for this session.</summary>
    NotFound,

    /// <summary>The code expired before it was submitted.</summary>
    Expired,
}

/// <summary>Wire-string mappings for the OTP enums (the analog of the Rust <c>as_str</c> methods).</summary>
public static class OtpWire
{
    public static string ToWire(this OtpChannel channel) => channel switch
    {
        OtpChannel.Email => "email",
        OtpChannel.Sms => "sms",
        _ => throw new ArgumentOutOfRangeException(nameof(channel)),
    };

    public static string ToWire(this OtpError error) => error switch
    {
        OtpError.InvalidCode => "INVALID_CODE",
        OtpError.MaxAttempts => "MAX_ATTEMPTS",
        OtpError.NotFound => "NOT_FOUND",
        OtpError.Expired => "EXPIRED",
        _ => throw new ArgumentOutOfRangeException(nameof(error)),
    };
}

/// <summary>
/// The contact points the server knows for a session's caller, handed to
/// <see cref="IOtpService.SendOtpAsync"/> so the host can deliver a code. The reference create-session
/// path captures only an email; a host that also captures a phone gets an SMS channel for free.
/// </summary>
public sealed record OtpContact(string? Email = null, string? Phone = null)
{
    /// <summary><c>true</c> when neither an email nor a phone is known — the server can't offer OTP
    /// for this session (no channel to deliver a code to).</summary>
    public bool IsEmpty => string.IsNullOrEmpty(Email) && string.IsNullOrEmpty(Phone);

    /// <summary>The channels a code could be delivered to, given the known contacts — email first,
    /// then SMS. Empty when <see cref="IsEmpty"/>. Surfaced as <c>availableChannels</c> in
    /// <c>otp_verification_required</c> so the client can offer the user a choice.</summary>
    public IReadOnlyList<OtpChannel> AvailableChannels
    {
        get
        {
            var channels = new List<OtpChannel>(2);
            if (!string.IsNullOrEmpty(Email)) channels.Add(OtpChannel.Email);
            if (!string.IsNullOrEmpty(Phone)) channels.Add(OtpChannel.Sms);
            return channels;
        }
    }
}

/// <summary>Acknowledgement returned by <see cref="IOtpService.SendOtpAsync"/>: which channel the code
/// went to and a masked destination safe to show the user (e.g. <c>j***@example.com</c>). Surfaced
/// verbatim as <c>otp_sent.data.data</c>.</summary>
public sealed record OtpDelivery(OtpChannel Channel, string MaskedDestination);

/// <summary>
/// Outcome of an <see cref="IOtpService.VerifyOtpAsync"/> call. On <see cref="Verified"/> the server
/// marks the session authenticated and emits <c>otp_verified</c>; on <see cref="Invalid"/> it emits
/// <c>otp_invalid</c> carrying the host-supplied attempt count, reason, and message (which only the
/// host, owner of the code store, can supply). A closed hierarchy — a bare bool couldn't carry the
/// <c>attemptsRemaining</c> + <c>message</c> the <c>otp-invalid</c> schema requires.
/// </summary>
public abstract record OtpVerifyOutcome
{
    private OtpVerifyOutcome() { }

    /// <summary>The code was correct; the session is now identity-verified.</summary>
    public sealed record Verified() : OtpVerifyOutcome;

    /// <summary>The code was rejected. Carries how many attempts remain (0 ⇒ locked, the client must
    /// restart the flow), an optional machine-readable reason, and a human-readable UI message.</summary>
    public sealed record Invalid(int AttemptsRemaining, OtpError? Error, string Message) : OtpVerifyOutcome;
}
