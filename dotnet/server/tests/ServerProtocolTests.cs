using System.Text.Json.Nodes;
using SmooAI.SmoothOperator.Core;
using SmooAI.SmoothOperator.Server;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// Server Phase-0 conformance/parity tests: the C# server speaks the wire protocol — the right
/// event sequence with shapes that validate against the SAME spec schemas + fixtures the Rust
/// reference server is held to (checked via the protocol client's ProtocolValidator).
/// </summary>
public class ServerProtocolTests
{
    private static (FrameDispatcher Dispatcher, InMemorySessionStore Store, List<JsonObject> Events) Build(MockChatClient chat, IKnowledgeBase? knowledge = null)
    {
        var store = new InMemorySessionStore();
        var knowledgeAccess = knowledge is null ? null : new StaticAccessKnowledge(knowledge);
        return (new FrameDispatcher(store, chat, knowledgeAccess), store, new List<JsonObject>());
    }

    private static async Task<ProtocolValidator> ValidatorAsync() => await ProtocolValidator.LoadAsync();

    // No agentId → the store mints a UUID (the protocol requires a UUID agentId; a client that
    // supplies one must pass a UUID too). This keeps the session descriptor spec-valid.
    private static string CreateSessionFrame(string requestId) =>
        $$"""{"action":"create_conversation_session","requestId":"{{requestId}}"}""";

    [Fact]
    public async Task Ping_ReturnsPong()
    {
        var (dispatcher, _, events) = Build(new MockChatClient());
        await dispatcher.DispatchAsync("""{"action":"ping","requestId":"p1"}""", events.Add);

        var ev = Assert.Single(events);
        Assert.Equal("pong", ev["type"]!.GetValue<string>());
        Assert.Equal("p1", ev["requestId"]!.GetValue<string>());
    }

    [Fact]
    public async Task CreateSession_ReturnsSpecValidSessionDescriptor()
    {
        var (dispatcher, _, events) = Build(new MockChatClient());
        await dispatcher.DispatchAsync(CreateSessionFrame("r1"), events.Add);

        var ev = Assert.Single(events);
        Assert.Equal("immediate_response", ev["type"]!.GetValue<string>());
        Assert.Equal(200, ev["status"]!.GetValue<int>());
        var data = ev["data"]!.AsObject();
        Assert.False(string.IsNullOrEmpty(data["sessionId"]!.GetValue<string>()));
        Assert.False(string.IsNullOrEmpty(data["conversationId"]!.GetValue<string>()));

        // The session descriptor validates against the same schema the conformance fixture uses.
        var validator = await ValidatorAsync();
        var result = validator.ValidateAt("actions/create-conversation-session.schema.json#/$defs/Response", data.ToJsonString());
        Assert.True(result.IsValid, result.FormatErrors());
    }

    [Fact]
    public async Task SendMessage_EmitsAck_Tokens_AndSpecValidEventualResponse()
    {
        var (dispatcher, store, events) = Build(new MockChatClient().PushText("Your return window is 17 days."));
        // Establish a session first.
        await dispatcher.DispatchAsync(CreateSessionFrame("r1"), events.Add);
        var sessionId = events[0]["data"]!["sessionId"]!.GetValue<string>();
        events.Clear();

        var sendFrame = $$"""{"action":"send_message","requestId":"r2","sessionId":"{{sessionId}}","message":"How long can I return?","stream":true}""";
        await dispatcher.DispatchAsync(sendFrame, events.Add);
        // send_message now runs its turn as a background task (so the read loop stays free to receive a
        // confirm_tool_action while a turn is parked) — await it so the terminal events are present.
        await dispatcher.WaitForTurnsAsync();

        // Sequence: immediate_response(202) → stream_token(s) → eventual_response(200).
        Assert.Equal("immediate_response", events[0]["type"]!.GetValue<string>());
        Assert.Equal(202, events[0]["status"]!.GetValue<int>());
        Assert.Contains(events, e => e["type"]!.GetValue<string>() == "stream_token");

        var terminal = events[^1];
        Assert.Equal("eventual_response", terminal["type"]!.GetValue<string>());
        Assert.Equal(200, terminal["status"]!.GetValue<int>());

        // The reply is carried in the triple-nested response.responseParts.
        var responseParts = terminal["data"]!["data"]!["response"]!["responseParts"]!.AsArray();
        Assert.Contains(responseParts, p => p!.GetValue<string>().Contains("17 days"));

        // It validates against the spec event schema.
        var validator = await ValidatorAsync();
        var result = validator.ValidateEvent("eventual_response", terminal.ToJsonString());
        Assert.True(result.IsValid, result.FormatErrors());
    }

    [Fact]
    public async Task SendMessage_WithKnowledge_AttachesCitations()
    {
        var kb = new InMemoryKnowledgeBase();
        await kb.IngestAsync(new KnowledgeDocument("returns", "The return window is 17 days.", "policies/returns.md"));

        var (dispatcher, _, events) = Build(new MockChatClient().PushText("It's 17 days."), kb);
        await dispatcher.DispatchAsync(CreateSessionFrame("r1"), events.Add);
        var sessionId = events[0]["data"]!["sessionId"]!.GetValue<string>();
        events.Clear();

        var sendFrame = $$"""{"action":"send_message","requestId":"r2","sessionId":"{{sessionId}}","message":"How long is the return window?"}""";
        await dispatcher.DispatchAsync(sendFrame, events.Add);
        // send_message now runs its turn as a background task — await it so the terminal event is present.
        await dispatcher.WaitForTurnsAsync();

        var terminal = events[^1];
        var citations = terminal["data"]!["data"]!["citations"]!.AsArray();
        Assert.NotEmpty(citations);
        Assert.Equal("policies/returns.md", citations[0]!["title"]!.GetValue<string>());

        var validator = await ValidatorAsync();
        var result = validator.ValidateEvent("eventual_response", terminal.ToJsonString());
        Assert.True(result.IsValid, result.FormatErrors());
    }

    [Fact]
    public async Task UnknownAction_ReturnsError()
    {
        var (dispatcher, _, events) = Build(new MockChatClient());
        await dispatcher.DispatchAsync("""{"action":"frobnicate","requestId":"x1"}""", events.Add);
        var ev = Assert.Single(events);
        Assert.Equal("error", ev["type"]!.GetValue<string>());
    }
}
