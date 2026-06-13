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

    public static JsonObject Error(string? requestId, string code, string message)
    {
        var ev = new JsonObject
        {
            ["type"] = "error",
            ["data"] = new JsonObject { ["error"] = new JsonObject { ["code"] = code, ["message"] = message } },
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
