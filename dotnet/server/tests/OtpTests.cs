using System.Text.Json.Nodes;
using Microsoft.Extensions.AI;
using SmooAI.SmoothOperator.Server;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// Parity tests for the OTP / session-identity seam — the C# counterparts of the Rust reference's
/// <c>otp.rs</c>, <c>protocol.rs</c> otp constructors, <c>agent_config.rs</c> auth-gate capture, and
/// the <c>handle_verify_otp</c> action handler. The server stays credential-free: it never generates,
/// holds, or validates a code — those are the host <see cref="IOtpService"/>'s job. Event shapes are
/// asserted against the SAME <c>spec/events/*.schema.json</c> the fixtures use.
/// </summary>
public class OtpEventBuilderTests
{
    private static async Task<ProtocolValidator> ValidatorAsync() => await ProtocolValidator.LoadAsync();

    [Fact]
    public async Task OtpVerificationRequired_MatchesSpecShape()
    {
        var ev = ProtocolEvents.OtpVerificationRequired("r1", "pay_invoice", "Verify your identity to continue using 'pay_invoice'.", new[] { OtpChannel.Email }, "end_user");

        Assert.Equal("otp_verification_required", ev["type"]!.GetValue<string>());
        Assert.Equal("r1", ev["requestId"]!.GetValue<string>());
        Assert.Equal("r1", ev["data"]!["requestId"]!.GetValue<string>());
        var inner = ev["data"]!["data"]!;
        Assert.Equal("pay_invoice", inner["toolId"]!.GetValue<string>());
        Assert.Equal("end_user", inner["authLevel"]!.GetValue<string>());
        Assert.Equal("email", inner["availableChannels"]![0]!.GetValue<string>());

        var result = (await ValidatorAsync()).ValidateEvent("otp_verification_required", ev.ToJsonString());
        Assert.True(result.IsValid, result.FormatErrors());
    }

    [Fact]
    public async Task OtpSent_MatchesSpecShape()
    {
        var ev = ProtocolEvents.OtpSent("r1", "email", "j***@example.com");

        Assert.Equal("otp_sent", ev["type"]!.GetValue<string>());
        Assert.Equal("email", ev["data"]!["data"]!["channel"]!.GetValue<string>());
        Assert.Equal("j***@example.com", ev["data"]!["data"]!["maskedDestination"]!.GetValue<string>());

        var result = (await ValidatorAsync()).ValidateEvent("otp_sent", ev.ToJsonString());
        Assert.True(result.IsValid, result.FormatErrors());
    }

    [Fact]
    public async Task OtpVerified_MatchesSpecShape()
    {
        var ev = ProtocolEvents.OtpVerified("r1", "Identity verified successfully.");

        Assert.Equal("otp_verified", ev["type"]!.GetValue<string>());
        Assert.Equal("Identity verified successfully.", ev["data"]!["data"]!["message"]!.GetValue<string>());

        var result = (await ValidatorAsync()).ValidateEvent("otp_verified", ev.ToJsonString());
        Assert.True(result.IsValid, result.FormatErrors());
    }

    [Fact]
    public async Task OtpInvalid_CarriesErrorAndAttempts_AndValidates()
    {
        var ev = ProtocolEvents.OtpInvalid("r1", "INVALID_CODE", 2, "Invalid code. 2 attempt(s) remaining.");

        Assert.Equal("otp_invalid", ev["type"]!.GetValue<string>());
        Assert.Equal("INVALID_CODE", ev["data"]!["data"]!["error"]!.GetValue<string>());
        Assert.Equal(2, ev["data"]!["data"]!["attemptsRemaining"]!.GetValue<int>());

        var result = (await ValidatorAsync()).ValidateEvent("otp_invalid", ev.ToJsonString());
        Assert.True(result.IsValid, result.FormatErrors());
    }

    [Fact]
    public async Task OtpInvalid_OmitsErrorWhenNull_AndValidates()
    {
        var ev = ProtocolEvents.OtpInvalid("r1", null, 0, "Verification failed.");

        var inner = ev["data"]!["data"]!.AsObject();
        Assert.False(inner.ContainsKey("error"), "error key must be ABSENT (not null) when the host determined no cause");
        Assert.Equal(0, inner["attemptsRemaining"]!.GetValue<int>());

        var result = (await ValidatorAsync()).ValidateEvent("otp_invalid", ev.ToJsonString());
        Assert.True(result.IsValid, result.FormatErrors());
    }
}

/// <summary>The seam value types — contact channels + wire strings (the C# analog of the Rust
/// <c>OtpContact</c> / <c>OtpChannel</c> / <c>OtpError</c> unit tests).</summary>
public class OtpSeamTypeTests
{
    [Fact]
    public void EmptyContact_OffersNoChannels()
    {
        var contact = new OtpContact();
        Assert.True(contact.IsEmpty);
        Assert.Empty(contact.AvailableChannels);
    }

    [Fact]
    public void EmailOnlyContact_OffersEmail()
    {
        var contact = new OtpContact(Email: "a@example.com");
        Assert.False(contact.IsEmpty);
        Assert.Equal(new[] { OtpChannel.Email }, contact.AvailableChannels);
    }

    [Fact]
    public void PhoneOnlyContact_OffersSms()
    {
        var contact = new OtpContact(Phone: "+15551234567");
        Assert.Equal(new[] { OtpChannel.Sms }, contact.AvailableChannels);
    }

    [Fact]
    public void BothContacts_OfferEmailThenSms()
    {
        var contact = new OtpContact(Email: "a@example.com", Phone: "+15551234567");
        Assert.Equal(new[] { OtpChannel.Email, OtpChannel.Sms }, contact.AvailableChannels);
    }

    [Fact]
    public void ChannelWireStrings()
    {
        Assert.Equal("email", OtpChannel.Email.ToWire());
        Assert.Equal("sms", OtpChannel.Sms.ToWire());
    }

    [Fact]
    public void ErrorWireStrings()
    {
        Assert.Equal("INVALID_CODE", OtpError.InvalidCode.ToWire());
        Assert.Equal("MAX_ATTEMPTS", OtpError.MaxAttempts.ToWire());
        Assert.Equal("NOT_FOUND", OtpError.NotFound.ToWire());
        Assert.Equal("EXPIRED", OtpError.Expired.ToWire());
    }
}

/// <summary>
/// The auth gate records an OTP-remediable refusal (public agent, <c>end_user</c> tool, unverified
/// session) so the server can offer OTP after the turn — and records nothing for an admin refusal or
/// a verified session. The C# analog of the Rust <c>auth_gate_records_end_user_refusal_for_otp</c>
/// tests, exercised through the public <see cref="ToolAuthGate.Apply"/> surface.
/// </summary>
public class OtpRefusalRecorderTests
{
    private sealed class StubAuthenticator : ISessionAuthenticator
    {
        private readonly bool _authed;

        public StubAuthenticator(bool authed) => _authed = authed;

        public Task<bool> IsAuthenticatedAsync(string conversationId, CancellationToken cancellationToken = default) => Task.FromResult(_authed);
    }

    private static AIFunction AuthTool(string name) => (AIFunction)AIFunctionFactory.Create(() => "REAL_RESULT", new AIFunctionFactoryOptions
    {
        Name = name,
        Description = $"{name}",
        AdditionalProperties = new Dictionary<string, object?> { ["supportsAuthRequirement"] = true },
    });

    private static async Task<(string Result, OtpRefusalRecorder Recorder)> InvokeGated(string authLevel, bool authed)
    {
        var recorder = new OtpRefusalRecorder();
        var config = new AgentConfig(EnabledTools: new[] { new EnabledTool("pay", true, authLevel, null) }, Visibility: "public");
        var gated = ToolAuthGate.Apply(new[] { AuthTool("pay") }, config, new StubAuthenticator(authed), "conv-1", recorder);
        var tool = (AIFunction)gated[0];
        var result = (await tool.InvokeAsync(new AIFunctionArguments()))?.ToString() ?? string.Empty;
        return (result, recorder);
    }

    [Fact]
    public async Task EndUserRefusal_IsRecordedForOtp()
    {
        var (result, recorder) = await InvokeGated("end_user", authed: false);
        Assert.Contains("verify your identity", result, StringComparison.Ordinal);
        Assert.Equal("pay", recorder.Refused);
    }

    [Fact]
    public async Task AdminRefusal_IsNotRecordedForOtp()
    {
        var (result, recorder) = await InvokeGated("admin", authed: false);
        Assert.Contains("requires admin authentication", result, StringComparison.Ordinal);
        Assert.Null(recorder.Refused);
    }

    [Fact]
    public async Task VerifiedSession_RecordsNothing_AndRuns()
    {
        var (result, recorder) = await InvokeGated("end_user", authed: true);
        Assert.Equal("REAL_RESULT", result);
        Assert.Null(recorder.Refused);
    }

    [Fact]
    public void Recorder_FirstRefusalWins()
    {
        var recorder = new OtpRefusalRecorder();
        Assert.Null(recorder.Refused);
        recorder.Record("first");
        recorder.Record("second");
        Assert.Equal("first", recorder.Refused);
    }
}

/// <summary>
/// The <c>verify_otp</c> action handler — validation order (requestId → sessionId → code →
/// session-exists → service), fail-closed with no service, and Verified/Invalid outcomes. The C#
/// analog of the Rust <c>otp_flow.rs</c> handler tests. The server never holds a code; it reflects
/// the host <see cref="IOtpService"/>'s opaque outcome onto the wire.
/// </summary>
public class VerifyOtpHandlerTests
{
    private sealed class FakeOtpService : IOtpService
    {
        private readonly OtpVerifyOutcome _outcome;

        public FakeOtpService(OtpVerifyOutcome outcome) => _outcome = outcome;

        public string? LastVerifiedSessionId { get; private set; }

        public Task<OtpDelivery> SendOtpAsync(string sessionId, OtpContact contact, CancellationToken cancellationToken = default) =>
            Task.FromResult(new OtpDelivery(OtpChannel.Email, "j***@example.com"));

        public Task<OtpVerifyOutcome> VerifyOtpAsync(string sessionId, string code, CancellationToken cancellationToken = default)
        {
            LastVerifiedSessionId = sessionId;
            return Task.FromResult(_outcome);
        }
    }

    private static FrameDispatcher Dispatcher(InMemorySessionStore store, IOtpService? otp) =>
        new(store, new MockChatClient(), otpService: otp);

    private static async Task<string> CreateSessionAsync(FrameDispatcher dispatcher, List<JsonObject> events)
    {
        await dispatcher.DispatchAsync("""{"action":"create_conversation_session","requestId":"r-create","agentId":"","userName":"Alice","userEmail":"alice@example.com"}""", events.Add);
        var sessionId = events[^1]["data"]!["sessionId"]!.GetValue<string>();
        events.Clear();
        return sessionId;
    }

    [Fact]
    public async Task Verified_EmitsOtpVerified_AndMarksSessionAuthenticated()
    {
        var store = new InMemorySessionStore();
        var dispatcher = Dispatcher(store, new FakeOtpService(new OtpVerifyOutcome.Verified()));
        var events = new List<JsonObject>();
        var sessionId = await CreateSessionAsync(dispatcher, events);
        var conversationId = (await store.GetSessionAsync(sessionId))!.ConversationId;

        await dispatcher.DispatchAsync($$"""{"action":"verify_otp","requestId":"r2","sessionId":"{{sessionId}}","code":"123456"}""", events.Add);

        var ev = Assert.Single(events);
        Assert.Equal("otp_verified", ev["type"]!.GetValue<string>());
        Assert.Equal("r2", ev["requestId"]!.GetValue<string>());
        Assert.True(await store.GetSessionAuthenticatedAsync(conversationId), "a verified session must be marked authenticated");
    }

    [Fact]
    public async Task Invalid_EmitsOtpInvalid_WithHostAttemptsAndReason()
    {
        var store = new InMemorySessionStore();
        var dispatcher = Dispatcher(store, new FakeOtpService(new OtpVerifyOutcome.Invalid(2, OtpError.InvalidCode, "Invalid code. 2 attempt(s) remaining.")));
        var events = new List<JsonObject>();
        var sessionId = await CreateSessionAsync(dispatcher, events);
        var conversationId = (await store.GetSessionAsync(sessionId))!.ConversationId;

        await dispatcher.DispatchAsync($$"""{"action":"verify_otp","requestId":"r2","sessionId":"{{sessionId}}","code":"000000"}""", events.Add);

        var ev = Assert.Single(events);
        Assert.Equal("otp_invalid", ev["type"]!.GetValue<string>());
        Assert.Equal("INVALID_CODE", ev["data"]!["data"]!["error"]!.GetValue<string>());
        Assert.Equal(2, ev["data"]!["data"]!["attemptsRemaining"]!.GetValue<int>());
        Assert.False(await store.GetSessionAuthenticatedAsync(conversationId), "a rejected code must NOT authenticate the session");
    }

    [Fact]
    public async Task NoService_FailsClosed_WithOtpInvalidNotFound()
    {
        var store = new InMemorySessionStore();
        var dispatcher = Dispatcher(store, otp: null);
        var events = new List<JsonObject>();
        var sessionId = await CreateSessionAsync(dispatcher, events);

        await dispatcher.DispatchAsync($$"""{"action":"verify_otp","requestId":"r2","sessionId":"{{sessionId}}","code":"123456"}""", events.Add);

        var ev = Assert.Single(events);
        Assert.Equal("otp_invalid", ev["type"]!.GetValue<string>());
        Assert.Equal("NOT_FOUND", ev["data"]!["data"]!["error"]!.GetValue<string>());
        Assert.Equal(0, ev["data"]!["data"]!["attemptsRemaining"]!.GetValue<int>());
    }

    [Fact]
    public async Task UnknownSession_Errors_SessionNotFound()
    {
        var dispatcher = Dispatcher(new InMemorySessionStore(), new FakeOtpService(new OtpVerifyOutcome.Verified()));
        var events = new List<JsonObject>();

        await dispatcher.DispatchAsync("""{"action":"verify_otp","requestId":"r2","sessionId":"11111111-1111-1111-1111-111111111111","code":"123456"}""", events.Add);

        var ev = Assert.Single(events);
        Assert.Equal("error", ev["type"]!.GetValue<string>());
        Assert.Equal("SESSION_NOT_FOUND", ev["error"]!["code"]!.GetValue<string>());
    }

    [Theory]
    [InlineData("""{"action":"verify_otp","sessionId":"s","code":"1"}""")] // no requestId
    [InlineData("""{"action":"verify_otp","requestId":"r2","code":"1"}""")] // no sessionId
    [InlineData("""{"action":"verify_otp","requestId":"r2","sessionId":"s"}""")] // no code
    public async Task MissingRequiredField_Errors_ValidationError(string frame)
    {
        var dispatcher = Dispatcher(new InMemorySessionStore(), new FakeOtpService(new OtpVerifyOutcome.Verified()));
        var events = new List<JsonObject>();

        await dispatcher.DispatchAsync(frame, events.Add);

        var ev = Assert.Single(events);
        Assert.Equal("error", ev["type"]!.GetValue<string>());
        Assert.Equal("VALIDATION_ERROR", ev["error"]!["code"]!.GetValue<string>());
    }
}
