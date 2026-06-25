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
