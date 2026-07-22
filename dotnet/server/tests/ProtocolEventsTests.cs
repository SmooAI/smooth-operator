using System.Text.Json.Nodes;
using SmooAI.SmoothOperator.Server;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// Parity unit tests for the event builders — the C# counterparts of the Rust server's
/// <c>protocol.rs</c> tests (stream_token/stream_chunk mirroring, the triple-nested
/// eventual_response, citations present/absent).
/// </summary>
public class ProtocolEventsTests
{
    [Fact]
    public void StreamToken_MirrorsToken()
    {
        var ev = ProtocolEvents.StreamToken("r1", "Hello");
        Assert.Equal("stream_token", ev["type"]!.GetValue<string>());
        Assert.Equal("Hello", ev["token"]!.GetValue<string>());
        Assert.Equal("Hello", ev["data"]!["token"]!.GetValue<string>());
        Assert.Equal("r1", ev["data"]!["requestId"]!.GetValue<string>());
    }

    [Fact]
    public void StreamChunk_MirrorsNode()
    {
        var ev = ProtocolEvents.StreamChunk("r1", "knowledge_search", new JsonObject { ["k"] = "v" });
        Assert.Equal("stream_chunk", ev["type"]!.GetValue<string>());
        Assert.Equal("knowledge_search", ev["node"]!.GetValue<string>());
        Assert.Equal("knowledge_search", ev["data"]!["node"]!.GetValue<string>());
        Assert.Equal("v", ev["data"]!["state"]!["k"]!.GetValue<string>());
    }

    [Fact]
    public void EventualResponse_DoubleNestsPayload()
    {
        var ev = ProtocolEvents.EventualResponse("r1", 200, "msg-1", ProtocolEvents.GeneralResponse("hi"), needsEscalation: false, citations: null);
        // type at the top, then data.status, then data.data.{messageId,response,needsEscalation}.
        Assert.Equal("eventual_response", ev["type"]!.GetValue<string>());
        Assert.Equal(200, ev["data"]!["status"]!.GetValue<int>());
        Assert.Equal("msg-1", ev["data"]!["data"]!["messageId"]!.GetValue<string>());
        Assert.False(ev["data"]!["data"]!["needsEscalation"]!.GetValue<bool>());
    }

    [Fact]
    public void EventualResponse_OmitsCitationsWhenEmpty()
    {
        var ev = ProtocolEvents.EventualResponse("r1", 200, "msg-1", ProtocolEvents.GeneralResponse("hi"), needsEscalation: false, citations: Array.Empty<JsonObject>());
        var inner = ev["data"]!["data"]!.AsObject();
        Assert.False(inner.ContainsKey("citations"), "citations key must be ABSENT (not null) when empty");
    }

    [Fact]
    public void EventualResponse_AttachesCitationsWhenPresent()
    {
        var citation = ProtocolEvents.Citation("doc-1", "policies/returns.md", url: null, "snippet", 0.9);
        var ev = ProtocolEvents.EventualResponse("r1", 200, "msg-1", ProtocolEvents.GeneralResponse("hi"), needsEscalation: false, citations: new[] { citation });
        var citations = ev["data"]!["data"]!["citations"]!.AsArray();
        Assert.Single(citations);
        Assert.Equal("doc-1", citations[0]!["id"]!.GetValue<string>());
    }

    [Fact]
    public void Cancelled_EchoesRequestId_WithTerminalStatus()
    {
        var ev = ProtocolEvents.Cancelled("r1");
        Assert.Equal("cancelled", ev["type"]!.GetValue<string>());
        Assert.Equal("r1", ev["requestId"]!.GetValue<string>());
        Assert.Equal(499, ev["status"]!.GetValue<int>());
        // requestId + status mirrored inside `data` (envelope convention).
        Assert.Equal("r1", ev["data"]!["requestId"]!.GetValue<string>());
        Assert.Equal(499, ev["data"]!["status"]!.GetValue<int>());
        Assert.True(ev["timestamp"]!.GetValue<long>() > 0);
        // No answer payload: a cancelled turn produced no assistant message.
        var data = ev["data"]!.AsObject();
        Assert.False(data.ContainsKey("messageId"));
        Assert.False(data.ContainsKey("response"));
    }

    [Fact]
    public void Cancelled_WithoutRequestId_OmitsTheField()
    {
        var ev = ProtocolEvents.Cancelled(null);
        Assert.Equal("cancelled", ev["type"]!.GetValue<string>());
        Assert.False(ev.ContainsKey("requestId"));
        Assert.False(ev["data"]!.AsObject().ContainsKey("requestId"));
        Assert.Equal(499, ev["status"]!.GetValue<int>());
    }
}
