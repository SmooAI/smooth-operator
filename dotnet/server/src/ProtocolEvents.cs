using System.Text.Json.Nodes;

namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// Builders for the server→client protocol event frames. The JSON shapes mirror the Rust
/// reference server's <c>protocol.rs</c> byte-for-byte (including the triple-nested
/// <c>eventual_response.data.data</c>), so they validate against the same
/// <c>spec/events/*.schema.json</c> and conformance fixtures.
/// </summary>
public static class ProtocolEvents
{
    private static long NowMs() => DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();

    public static JsonObject Pong(string? requestId)
    {
        var ev = new JsonObject { ["type"] = "pong", ["timestamp"] = NowMs() };
        if (requestId is not null) ev["requestId"] = requestId;
        return ev;
    }

    public static JsonObject ImmediateResponse(string? requestId, int status, string message, JsonNode data)
    {
        var ev = new JsonObject
        {
            ["type"] = "immediate_response",
            ["status"] = status,
            ["message"] = message,
            ["data"] = data,
            ["timestamp"] = NowMs(),
        };
        if (requestId is not null) ev["requestId"] = requestId;
        return ev;
    }

    public static JsonObject StreamToken(string requestId, string token) => new()
    {
        ["type"] = "stream_token",
        ["requestId"] = requestId,
        ["token"] = token,
        ["data"] = new JsonObject { ["requestId"] = requestId, ["token"] = token },
        ["timestamp"] = NowMs(),
    };

    /// <summary>
    /// <c>stream_preamble</c> — one short present-tense "what I'm about to do" sentence produced by a
    /// fast model IN PARALLEL with the turn, covering the main model's time-to-first-token. Shaped
    /// exactly like <see cref="StreamToken"/> (so clients reuse the render path) but on a distinct
    /// type, because it is EPHEMERAL: the real answer replaces it, it is never persisted, and it never
    /// appears in <c>eventual_response</c>. Emitted only when <c>SMOOTH_AGENT_PREAMBLE_MODEL</c> is set.
    /// </summary>
    public static JsonObject StreamPreamble(string requestId, string token) => new()
    {
        ["type"] = "stream_preamble",
        ["requestId"] = requestId,
        ["token"] = token,
        ["data"] = new JsonObject { ["requestId"] = requestId, ["token"] = token },
        ["timestamp"] = NowMs(),
    };

    public static JsonObject StreamChunk(string requestId, string node, JsonNode state) => new()
    {
        ["type"] = "stream_chunk",
        ["requestId"] = requestId,
        ["node"] = node,
        ["data"] = new JsonObject { ["requestId"] = requestId, ["node"] = node, ["state"] = state },
        ["timestamp"] = NowMs(),
    };

    /// <summary>
    /// The terminal turn event. Matches the Rust shape: a triple-nested
    /// <c>data.data</c> carrying <c>messageId</c>, the agent <c>response</c>, <c>needsEscalation</c>,
    /// and (only when non-empty) the <c>citations</c> array.
    /// </summary>
    public static JsonObject EventualResponse(string requestId, int status, string messageId, JsonNode response, bool needsEscalation, IReadOnlyList<JsonObject>? citations)
    {
        var inner = new JsonObject
        {
            ["messageId"] = messageId,
            ["response"] = response,
            ["needsEscalation"] = needsEscalation,
        };
        if (citations is { Count: > 0 })
        {
            var array = new JsonArray();
            foreach (var citation in citations)
            {
                array.Add(citation);
            }
            inner["citations"] = array;
        }

        return new JsonObject
        {
            ["type"] = "eventual_response",
            ["requestId"] = requestId,
            ["status"] = status,
            ["data"] = new JsonObject
            {
                ["requestId"] = requestId,
                ["status"] = status,
                ["data"] = inner,
            },
            ["timestamp"] = NowMs(),
        };
    }

    /// <summary>
    /// <c>write_confirmation_required</c> — emitted mid-turn when the agent calls a state-mutating
    /// tool that requires explicit human approval before it runs. The turn is <b>parked</b> (the
    /// engine's <c>IHumanGate</c> awaits the verdict) until the client replies with a
    /// <c>confirm_tool_action</c> action carrying the same <c>requestId</c> and an <c>approved</c>
    /// boolean.
    ///
    /// Wire shape matches <c>spec/events/write-confirmation-required.schema.json</c> and the Rust
    /// reference byte-for-byte: the <c>requestId</c> echoes the originating <c>send_message</c>, and
    /// the prompt detail is double-nested under <c>data.data.{toolId, actionDescription}</c>.
    /// <c>toolId</c> is an opaque correlation handle (the tool name — a turn parks one tool at a
    /// time); <c>actionDescription</c> is the human-readable prompt the client renders.
    /// </summary>
    public static JsonObject WriteConfirmationRequired(string requestId, string toolId, string actionDescription) => new()
    {
        ["type"] = "write_confirmation_required",
        ["requestId"] = requestId,
        ["data"] = new JsonObject
        {
            ["requestId"] = requestId,
            ["data"] = new JsonObject { ["toolId"] = toolId, ["actionDescription"] = actionDescription },
        },
        ["timestamp"] = NowMs(),
    };

    /// <summary>
    /// <c>otp_verification_required</c> — emitted after a turn's auth gate refused an <c>end_user</c>
    /// tool on an unverified session and the host has an OTP service installed. Tells the client to
    /// collect a one-time code. Wire shape matches <c>spec/events/otp-verification-required.schema.json</c>
    /// (double-nested <c>data.data</c>). <paramref name="availableChannels"/> are the delivery channels
    /// the server can offer given the session's known contacts.
    /// </summary>
    public static JsonObject OtpVerificationRequired(string requestId, string toolId, string actionDescription, IReadOnlyList<OtpChannel> availableChannels, string authLevel)
    {
        var channels = new JsonArray();
        foreach (var channel in availableChannels)
        {
            channels.Add(channel.ToWire());
        }
        return new JsonObject
        {
            ["type"] = "otp_verification_required",
            ["requestId"] = requestId,
            ["data"] = new JsonObject
            {
                ["requestId"] = requestId,
                ["data"] = new JsonObject
                {
                    ["toolId"] = toolId,
                    ["actionDescription"] = actionDescription,
                    ["availableChannels"] = channels,
                    ["authLevel"] = authLevel,
                },
            },
            ["timestamp"] = NowMs(),
        };
    }

    /// <summary><c>otp_sent</c> — acknowledgement that a code was dispatched to the caller. Wire shape
    /// matches <c>spec/events/otp-sent.schema.json</c>. <paramref name="maskedDestination"/> is a
    /// partially masked address safe to display (e.g. <c>j***@example.com</c>).</summary>
    public static JsonObject OtpSent(string requestId, string channel, string maskedDestination) => new()
    {
        ["type"] = "otp_sent",
        ["requestId"] = requestId,
        ["data"] = new JsonObject
        {
            ["requestId"] = requestId,
            ["data"] = new JsonObject { ["channel"] = channel, ["maskedDestination"] = maskedDestination },
        },
        ["timestamp"] = NowMs(),
    };

    /// <summary><c>otp_verified</c> — emitted when a <c>verify_otp</c> attempt succeeds. The session is
    /// now identity-verified; the client re-sends its message to run the gated tool. Wire shape matches
    /// <c>spec/events/otp-verified.schema.json</c>.</summary>
    public static JsonObject OtpVerified(string requestId, string message) => new()
    {
        ["type"] = "otp_verified",
        ["requestId"] = requestId,
        ["data"] = new JsonObject
        {
            ["requestId"] = requestId,
            ["data"] = new JsonObject { ["message"] = message },
        },
        ["timestamp"] = NowMs(),
    };

    /// <summary>
    /// <c>otp_invalid</c> — emitted when a <c>verify_otp</c> attempt is rejected. <paramref name="error"/>
    /// is an optional machine-readable reason; <paramref name="attemptsRemaining"/> of 0 means the code
    /// is locked and the client must restart the flow. Wire shape matches
    /// <c>spec/events/otp-invalid.schema.json</c> — <c>error</c> is omitted when the host determined no cause.
    /// </summary>
    public static JsonObject OtpInvalid(string requestId, string? error, int attemptsRemaining, string message)
    {
        var inner = new JsonObject { ["attemptsRemaining"] = attemptsRemaining, ["message"] = message };
        if (error is not null)
        {
            inner["error"] = error;
        }
        return new JsonObject
        {
            ["type"] = "otp_invalid",
            ["requestId"] = requestId,
            ["data"] = new JsonObject { ["requestId"] = requestId, ["data"] = inner },
            ["timestamp"] = NowMs(),
        };
    }

    /// <summary>
    /// <c>cancelled</c> — the terminal event of a turn the client aborted with a <c>cancel</c> action.
    /// Emitted <b>in place of</b> the <c>eventual_response</c> a completed turn would send: it echoes
    /// the cancelled <c>send_message</c>'s <c>requestId</c> so the client can correlate it to the
    /// in-flight turn and reset its UI (drop the streaming indicator, re-enable input).
    ///
    /// Status <c>499</c> mirrors nginx's "client closed request" — a terminal, non-200 outcome distinct
    /// from a server error. The <c>requestId</c> is echoed at the envelope level and inside <c>data</c>
    /// (envelope convention). No answer payload: a cancelled turn produced no assistant message (the
    /// streamed tokens were ephemeral and are NOT persisted; the user's message stays persisted).
    ///
    /// A cancel with no active turn is a no-op and emits nothing — this builder is only called when a
    /// live turn was actually aborted. Wire shape matches the Rust <c>protocol::cancelled</c> and
    /// <c>spec/events/cancelled.schema.json</c>.
    /// </summary>
    public static JsonObject Cancelled(string? requestId)
    {
        var data = new JsonObject { ["status"] = 499 };
        if (requestId is not null) data["requestId"] = requestId;
        var ev = new JsonObject
        {
            ["type"] = "cancelled",
            ["status"] = 499,
            ["data"] = data,
            ["timestamp"] = NowMs(),
        };
        if (requestId is not null) ev["requestId"] = requestId;
        return ev;
    }

    public static JsonObject Error(string? requestId, string code, string message)
    {
        // The {code, message} descriptor is duplicated at the envelope top level (`error`) and nested
        // under `data.error`, per spec/events/error.schema.json — the top-level copy is "kept for clients
        // that pattern-match on the envelope-level `error` field". Mirrors the Python reference server.
        var data = new JsonObject { ["error"] = new JsonObject { ["code"] = code, ["message"] = message } };
        if (requestId is not null) data["requestId"] = requestId;
        var ev = new JsonObject
        {
            ["type"] = "error",
            ["error"] = new JsonObject { ["code"] = code, ["message"] = message },
            ["data"] = data,
            ["timestamp"] = NowMs(),
        };
        if (requestId is not null) ev["requestId"] = requestId;
        return ev;
    }

    /// <summary>A minimal <c>GeneralAgentResponse</c> wrapping the agent's reply text.</summary>
    public static JsonObject GeneralResponse(string reply) => new()
    {
        ["responseParts"] = new JsonArray { reply },
        ["customerHappinessScore"] = 0.5,
        ["needsSatisfactionScore"] = 0.5,
        ["requestSummary"] = string.Empty,
        ["resolutionStatus"] = "in_progress",
        ["suggestedNextActions"] = new JsonArray(),
    };

    public static JsonObject Citation(string id, string title, string? url, string snippet, double score)
    {
        var citation = new JsonObject
        {
            ["id"] = id,
            ["title"] = title,
            ["snippet"] = snippet,
            ["score"] = score,
        };
        if (url is not null)
        {
            citation["url"] = url;
        }
        return citation;
    }
}
