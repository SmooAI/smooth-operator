// Ergonomic, hand-curated types layered on top of the generated ones.
//
// The generated types in Generated/Types.cs are a faithful 1:1 reflection of the
// JSON Schemas — one class per schema/$def. They are correct but flat: there is no
// single discriminated union over the wire frames.
//
// This module adds the two discriminated unions consumers actually want:
//
//   • ClientAction — everything sent client→server, discriminated by `action`.
//   • ServerEvent  — everything received server→client, discriminated by `type`.
//
// Both lean on System.Text.Json polymorphic (de)serialization. The base type
// carries `[JsonPolymorphic]` keyed on the discriminator property and one
// `[JsonDerivedType]` per concrete frame. Deserializing a raw frame into the base
// type yields the right concrete subtype; pattern-match (`is StreamTokenEvent t`)
// or switch on `.Type` / `.Action` to narrow.

using System.Text.Json;
using System.Text.Json.Serialization;

namespace SmooAI.SmoothOperatorAgent;

/// <summary>Every server→client event <c>type</c> discriminator value.</summary>
public static class EventTypes
{
    public const string ImmediateResponse = "immediate_response";
    public const string EventualResponse = "eventual_response";
    public const string StreamChunk = "stream_chunk";
    public const string StreamToken = "stream_token";
    public const string Keepalive = "keepalive";
    public const string WriteConfirmationRequired = "write_confirmation_required";
    public const string OtpVerificationRequired = "otp_verification_required";
    public const string OtpSent = "otp_sent";
    public const string OtpVerified = "otp_verified";
    public const string OtpInvalid = "otp_invalid";
    public const string Error = "error";
    public const string Pong = "pong";

    public static readonly IReadOnlySet<string> All = new HashSet<string>
    {
        ImmediateResponse, EventualResponse, StreamChunk, StreamToken, Keepalive,
        WriteConfirmationRequired, OtpVerificationRequired, OtpSent, OtpVerified,
        OtpInvalid, Error, Pong,
    };
}

/// <summary>Every client→server <c>action</c> discriminator value.</summary>
public static class ActionTypes
{
    public const string CreateConversationSession = "create_conversation_session";
    public const string SendMessage = "send_message";
    public const string GetSession = "get_session";
    public const string GetConversationMessages = "get_conversation_messages";
    public const string ConfirmToolAction = "confirm_tool_action";
    public const string VerifyOtp = "verify_otp";
    public const string Ping = "ping";

    public static readonly IReadOnlySet<string> All = new HashSet<string>
    {
        CreateConversationSession, SendMessage, GetSession, GetConversationMessages,
        ConfirmToolAction, VerifyOtp, Ping,
    };
}

// ───────────────────────────── Server events ───────────────────────────────

/// <summary>
/// Discriminated union over every server→client event frame, keyed on <c>type</c>.
/// Deserialize a raw frame into <see cref="ServerEvent"/> to get the concrete
/// subtype, then pattern-match (<c>is StreamTokenEvent</c>) or switch on
/// <see cref="Type"/>.
/// </summary>
// NOTE on the converter vs. [JsonPolymorphic]:
// We deliberately do NOT use STJ's built-in [JsonPolymorphic]/[JsonDerivedType]
// machinery for ServerEvent. On net8.0 that resolver requires the discriminator
// (`type`) to appear FIRST in the JSON object — but the Rust server serializes
// via serde_json without `preserve_order`, so object keys come out alphabetically
// (`data` before `type`). The built-in resolver then fails trying to instantiate
// the abstract base. ServerEventConverter below reads `type` regardless of its
// position and dispatches to the concrete subtype, so real-server frames decode.
[JsonConverter(typeof(ServerEventConverter))]
public abstract class ServerEvent
{
    /// <summary>The event <c>type</c> discriminator (also emitted by STJ on serialize).</summary>
    [JsonIgnore]
    public abstract string Type { get; }

    /// <summary>Echoes the originating action's <c>requestId</c>, where applicable.</summary>
    [JsonPropertyName("requestId")]
    public string? RequestId { get; set; }
}

/// <summary>
/// Position-independent polymorphic converter for <see cref="ServerEvent"/>.
/// Reads the <c>type</c> discriminator anywhere in the object (the Rust server
/// emits keys alphabetically, so <c>type</c> is not first) and deserializes into
/// the matching concrete subtype. On write, delegates to the concrete runtime
/// type and injects the <c>type</c> discriminator.
/// </summary>
public sealed class ServerEventConverter : JsonConverter<ServerEvent>
{
    private static readonly IReadOnlyDictionary<string, Type> ByType = new Dictionary<string, Type>
    {
        [EventTypes.ImmediateResponse] = typeof(ImmediateResponseEvent),
        [EventTypes.EventualResponse] = typeof(EventualResponseEvent),
        [EventTypes.StreamChunk] = typeof(StreamChunkEvent),
        [EventTypes.StreamToken] = typeof(StreamTokenEvent),
        [EventTypes.Keepalive] = typeof(KeepaliveEvent),
        [EventTypes.WriteConfirmationRequired] = typeof(WriteConfirmationRequiredEvent),
        [EventTypes.OtpVerificationRequired] = typeof(OtpVerificationRequiredEvent),
        [EventTypes.OtpSent] = typeof(OtpSentEvent),
        [EventTypes.OtpVerified] = typeof(OtpVerifiedEvent),
        [EventTypes.OtpInvalid] = typeof(OtpInvalidEvent),
        [EventTypes.Error] = typeof(ErrorEvent),
        [EventTypes.Pong] = typeof(PongEvent),
    };

    public override ServerEvent? Read(ref Utf8JsonReader reader, Type typeToConvert, JsonSerializerOptions options)
    {
        using var doc = JsonDocument.ParseValue(ref reader);
        var root = doc.RootElement;
        if (!root.TryGetProperty("type", out var typeEl) || typeEl.ValueKind != JsonValueKind.String)
            throw new JsonException("ServerEvent frame is missing a string \"type\" discriminator.");

        var discriminator = typeEl.GetString()!;
        if (!ByType.TryGetValue(discriminator, out var target))
            throw new JsonException($"Unknown ServerEvent type \"{discriminator}\".");

        // Deserialize the concrete type directly (the subtypes have no converter,
        // so this won't recurse back into this converter).
        return (ServerEvent?)root.Deserialize(target, options);
    }

    public override void Write(Utf8JsonWriter writer, ServerEvent value, JsonSerializerOptions options)
    {
        // Serialize the concrete runtime type into a buffer, then re-emit it with
        // the `type` discriminator injected (the subtypes don't carry `type`).
        var concrete = JsonSerializer.SerializeToElement(value, value.GetType(), options);
        writer.WriteStartObject();
        writer.WriteString("type", value.Type);
        foreach (var prop in concrete.EnumerateObject())
        {
            if (prop.NameEquals("type")) continue;
            prop.WriteTo(writer);
        }
        writer.WriteEndObject();
    }
}

/// <summary>Acknowledgement that an action was accepted; also carries the payload for non-streaming actions.</summary>
public sealed class ImmediateResponseEvent : ServerEvent
{
    public override string Type => EventTypes.ImmediateResponse;

    [JsonPropertyName("status")]
    public int? Status { get; set; }

    [JsonPropertyName("message")]
    public string? Message { get; set; }

    /// <summary>Action-specific response payload (session descriptor, message page, …).</summary>
    [JsonPropertyName("data")]
    public JsonElement? Data { get; set; }

    [JsonPropertyName("timestamp")]
    public long? Timestamp { get; set; }
}

/// <summary>Terminal event of a streaming <c>send_message</c> turn.</summary>
public sealed class EventualResponseEvent : ServerEvent
{
    public override string Type => EventTypes.EventualResponse;

    [JsonPropertyName("status")]
    public int? Status { get; set; }

    /// <summary>The terminal response envelope. Note the protocol's double-nested <c>data.data</c>.</summary>
    [JsonPropertyName("data")]
    public EventualResponseData Data { get; set; } = new();

    [JsonPropertyName("timestamp")]
    public long? Timestamp { get; set; }
}

/// <summary>
/// The <c>data</c> field of an <see cref="EventualResponseEvent"/>. The protocol
/// double-nests the payload: <c>event.data.data</c> holds the actual agent output.
/// </summary>
public sealed class EventualResponseData
{
    [JsonPropertyName("requestId")]
    public string? RequestId { get; set; }

    [JsonPropertyName("status")]
    public int? Status { get; set; }

    /// <summary>The final agent output. This is the inner <c>data</c> of <c>data.data</c>.</summary>
    [JsonPropertyName("data")]
    public EventualResponsePayload Payload { get; set; } = new();
}

/// <summary>The innermost agent output payload (the <c>data.data</c> of an eventual response).</summary>
public sealed class EventualResponsePayload
{
    [JsonPropertyName("messageId")]
    public string? MessageId { get; set; }

    /// <summary>
    /// Structured agent response. Shape depends on the agent template, so it is kept
    /// as a raw element; use <see cref="ServerEventExtensions.GetResponse{T}"/> to decode.
    /// </summary>
    [JsonPropertyName("response")]
    public JsonElement? Response { get; set; }

    [JsonPropertyName("needsEscalation")]
    public bool? NeedsEscalation { get; set; }

    [JsonPropertyName("escalationReason")]
    public string? EscalationReason { get; set; }
}

/// <summary>A per-node workflow state snapshot.</summary>
public sealed class StreamChunkEvent : ServerEvent
{
    public override string Type => EventTypes.StreamChunk;

    /// <summary>Name of the workflow node that produced this chunk (top-level mirror).</summary>
    [JsonPropertyName("node")]
    public string? Node { get; set; }

    [JsonPropertyName("data")]
    public StreamChunkData Data { get; set; } = new();

    [JsonPropertyName("timestamp")]
    public long? Timestamp { get; set; }
}

public sealed class StreamChunkData
{
    [JsonPropertyName("requestId")]
    public string? RequestId { get; set; }

    [JsonPropertyName("node")]
    public string? Node { get; set; }

    /// <summary>Filtered node state snapshot (kept raw — shape varies by node).</summary>
    [JsonPropertyName("state")]
    public JsonElement? State { get; set; }

    [JsonPropertyName("done")]
    public bool? Done { get; set; }
}

/// <summary>A single streamed LLM token.</summary>
public sealed class StreamTokenEvent : ServerEvent
{
    public override string Type => EventTypes.StreamToken;

    /// <summary>The raw token text (top-level mirror of <c>data.token</c>).</summary>
    [JsonPropertyName("token")]
    public string? Token { get; set; }

    [JsonPropertyName("data")]
    public StreamTokenData Data { get; set; } = new();

    [JsonPropertyName("timestamp")]
    public long? Timestamp { get; set; }
}

public sealed class StreamTokenData
{
    [JsonPropertyName("requestId")]
    public string? RequestId { get; set; }

    [JsonPropertyName("token")]
    public string? Token { get; set; }
}

/// <summary>Server keepalive during long-running turns (distinct from ping/pong).</summary>
public sealed class KeepaliveEvent : ServerEvent
{
    public override string Type => EventTypes.Keepalive;

    [JsonPropertyName("data")]
    public RequestIdData Data { get; set; } = new();

    [JsonPropertyName("timestamp")]
    public long? Timestamp { get; set; }
}

public sealed class RequestIdData
{
    [JsonPropertyName("requestId")]
    public string? RequestId { get; set; }
}

/// <summary>HITL: the agent paused awaiting confirmation of a state-mutating tool call.</summary>
public sealed class WriteConfirmationRequiredEvent : ServerEvent
{
    public override string Type => EventTypes.WriteConfirmationRequired;

    [JsonPropertyName("data")]
    public NestedData<WriteConfirmationDetails> Data { get; set; } = new();

    [JsonPropertyName("timestamp")]
    public long? Timestamp { get; set; }
}

public sealed class WriteConfirmationDetails
{
    [JsonPropertyName("toolId")]
    public string? ToolId { get; set; }

    [JsonPropertyName("actionDescription")]
    public string? ActionDescription { get; set; }
}

/// <summary>HITL: the agent paused awaiting OTP verification before an authenticated action.</summary>
public sealed class OtpVerificationRequiredEvent : ServerEvent
{
    public override string Type => EventTypes.OtpVerificationRequired;

    [JsonPropertyName("data")]
    public NestedData<OtpVerificationDetails> Data { get; set; } = new();

    [JsonPropertyName("timestamp")]
    public long? Timestamp { get; set; }
}

public sealed class OtpVerificationDetails
{
    [JsonPropertyName("toolId")]
    public string? ToolId { get; set; }

    [JsonPropertyName("actionDescription")]
    public string? ActionDescription { get; set; }

    [JsonPropertyName("availableChannels")]
    public List<string> AvailableChannels { get; set; } = new();

    [JsonPropertyName("authLevel")]
    public string? AuthLevel { get; set; }
}

/// <summary>Acknowledgement that an OTP code was dispatched.</summary>
public sealed class OtpSentEvent : ServerEvent
{
    public override string Type => EventTypes.OtpSent;

    [JsonPropertyName("data")]
    public NestedData<OtpSentDetails> Data { get; set; } = new();

    [JsonPropertyName("timestamp")]
    public long? Timestamp { get; set; }
}

public sealed class OtpSentDetails
{
    [JsonPropertyName("channel")]
    public string? Channel { get; set; }

    [JsonPropertyName("maskedDestination")]
    public string? MaskedDestination { get; set; }
}

/// <summary>OTP verification succeeded; the paused workflow resumes.</summary>
public sealed class OtpVerifiedEvent : ServerEvent
{
    public override string Type => EventTypes.OtpVerified;

    [JsonPropertyName("data")]
    public NestedData<OtpVerifiedDetails> Data { get; set; } = new();

    [JsonPropertyName("timestamp")]
    public long? Timestamp { get; set; }
}

public sealed class OtpVerifiedDetails
{
    [JsonPropertyName("message")]
    public string? Message { get; set; }
}

/// <summary>OTP verification failed.</summary>
public sealed class OtpInvalidEvent : ServerEvent
{
    public override string Type => EventTypes.OtpInvalid;

    [JsonPropertyName("data")]
    public NestedData<OtpInvalidDetails> Data { get; set; } = new();

    [JsonPropertyName("timestamp")]
    public long? Timestamp { get; set; }
}

public sealed class OtpInvalidDetails
{
    [JsonPropertyName("error")]
    public string? Error { get; set; }

    [JsonPropertyName("attemptsRemaining")]
    public int? AttemptsRemaining { get; set; }

    [JsonPropertyName("message")]
    public string? Message { get; set; }
}

/// <summary>An unrecoverable protocol error.</summary>
public sealed class ErrorEvent : ServerEvent
{
    public override string Type => EventTypes.Error;

    /// <summary>Top-level error mirror (duplicate of <c>data.error</c>).</summary>
    [JsonPropertyName("error")]
    public ErrorDescriptor? Error { get; set; }

    [JsonPropertyName("data")]
    public ErrorData Data { get; set; } = new();

    [JsonPropertyName("timestamp")]
    public long? Timestamp { get; set; }
}

public sealed class ErrorData
{
    [JsonPropertyName("requestId")]
    public string? RequestId { get; set; }

    [JsonPropertyName("error")]
    public ErrorDescriptor Error { get; set; } = new();

    [JsonPropertyName("details")]
    public JsonElement? Details { get; set; }
}

public sealed class ErrorDescriptor
{
    [JsonPropertyName("code")]
    public string Code { get; set; } = string.Empty;

    [JsonPropertyName("message")]
    public string Message { get; set; } = string.Empty;
}

/// <summary>Server reply to a <c>ping</c> action.</summary>
public sealed class PongEvent : ServerEvent
{
    public override string Type => EventTypes.Pong;

    /// <summary>Server timestamp (top-level mirror of <c>data.timestamp</c>).</summary>
    [JsonPropertyName("timestamp")]
    public long? Timestamp { get; set; }

    [JsonPropertyName("data")]
    public PongData? Data { get; set; }
}

public sealed class PongData
{
    [JsonPropertyName("timestamp")]
    public long? Timestamp { get; set; }
}

/// <summary>
/// The common two-level <c>{ requestId, data }</c> envelope used by the HITL and OTP
/// events, where the inner <c>data</c> holds the typed details.
/// </summary>
public sealed class NestedData<T> where T : new()
{
    [JsonPropertyName("requestId")]
    public string? RequestId { get; set; }

    [JsonPropertyName("data")]
    public T Data { get; set; } = new();
}

// ───────────────────────────── Client actions ──────────────────────────────

/// <summary>
/// Discriminated union over every client→server action frame, keyed on <c>action</c>.
/// </summary>
[JsonPolymorphic(TypeDiscriminatorPropertyName = "action", UnknownDerivedTypeHandling = JsonUnknownDerivedTypeHandling.FailSerialization)]
[JsonDerivedType(typeof(CreateConversationSessionAction), ActionTypes.CreateConversationSession)]
[JsonDerivedType(typeof(SendMessageAction), ActionTypes.SendMessage)]
[JsonDerivedType(typeof(GetSessionAction), ActionTypes.GetSession)]
[JsonDerivedType(typeof(GetMessagesAction), ActionTypes.GetConversationMessages)]
[JsonDerivedType(typeof(ConfirmToolAction), ActionTypes.ConfirmToolAction)]
[JsonDerivedType(typeof(VerifyOtpAction), ActionTypes.VerifyOtp)]
[JsonDerivedType(typeof(PingAction), ActionTypes.Ping)]
public abstract class ClientAction
{
    [JsonIgnore]
    public abstract string Action { get; }

    /// <summary>Client-generated correlation ID echoed back on all related events.</summary>
    [JsonPropertyName("requestId")]
    public string? RequestId { get; set; }
}

public sealed class CreateConversationSessionAction : ClientAction
{
    public override string Action => ActionTypes.CreateConversationSession;

    [JsonPropertyName("agentId")]
    public string AgentId { get; set; } = string.Empty;

    [JsonPropertyName("userName")]
    public string? UserName { get; set; }

    [JsonPropertyName("userEmail")]
    public string? UserEmail { get; set; }

    [JsonPropertyName("browserFingerprint")]
    public string? BrowserFingerprint { get; set; }

    [JsonPropertyName("metadata")]
    public Dictionary<string, object>? Metadata { get; set; }
}

public sealed class SendMessageAction : ClientAction
{
    public override string Action => ActionTypes.SendMessage;

    [JsonPropertyName("sessionId")]
    public string SessionId { get; set; } = string.Empty;

    [JsonPropertyName("message")]
    public string Message { get; set; } = string.Empty;

    [JsonPropertyName("stream")]
    public bool? Stream { get; set; }
}

public sealed class GetSessionAction : ClientAction
{
    public override string Action => ActionTypes.GetSession;

    [JsonPropertyName("sessionId")]
    public string SessionId { get; set; } = string.Empty;
}

public sealed class GetMessagesAction : ClientAction
{
    public override string Action => ActionTypes.GetConversationMessages;

    [JsonPropertyName("sessionId")]
    public string SessionId { get; set; } = string.Empty;

    [JsonPropertyName("limit")]
    public int? Limit { get; set; }

    [JsonPropertyName("before")]
    public string? Before { get; set; }
}

public sealed class ConfirmToolAction : ClientAction
{
    public override string Action => ActionTypes.ConfirmToolAction;

    [JsonPropertyName("sessionId")]
    public string SessionId { get; set; } = string.Empty;

    [JsonPropertyName("approved")]
    public bool Approved { get; set; }
}

public sealed class VerifyOtpAction : ClientAction
{
    public override string Action => ActionTypes.VerifyOtp;

    [JsonPropertyName("sessionId")]
    public string SessionId { get; set; } = string.Empty;

    [JsonPropertyName("code")]
    public string Code { get; set; } = string.Empty;
}

public sealed class PingAction : ClientAction
{
    public override string Action => ActionTypes.Ping;
}

// ───────────────────────────── Convenience ─────────────────────────────────

/// <summary>Helpers for decoding the template-dependent payloads kept as raw JSON.</summary>
public static class ServerEventExtensions
{
    /// <summary>Decode the structured agent response payload to a concrete type.</summary>
    public static T? GetResponse<T>(this EventualResponsePayload payload, JsonSerializerOptions? options = null)
        => payload.Response is { } el ? el.Deserialize<T>(options) : default;

    /// <summary>Decode the <c>immediate_response</c> data payload to a concrete type.</summary>
    public static T? GetData<T>(this ImmediateResponseEvent ev, JsonSerializerOptions? options = null)
        => ev.Data is { } el ? el.Deserialize<T>(options) : default;
}
