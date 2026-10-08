// Conformance: every instance in spec/conformance/fixtures.json must validate against
// the schema it claims to (mirrors the spec's own validation, in C#).

using System.Text.Json;

namespace SmooAI.SmoothOperator.Tests;

public sealed class ConformanceTests : IAsyncLifetime
{
    private ProtocolValidator _validator = null!;
    private Dictionary<string, Fixture> _fixtures = null!;

    public async Task InitializeAsync()
    {
        _validator = await ProtocolValidator.LoadAsync(SpecPaths.SpecDir);

        var raw = await File.ReadAllTextAsync(Path.Combine(SpecPaths.SpecDir, "conformance", "fixtures.json"));
        using var doc = JsonDocument.Parse(raw);
        _fixtures = new Dictionary<string, Fixture>();
        foreach (var prop in doc.RootElement.EnumerateObject())
        {
            if (prop.Name.StartsWith('$')) continue; // skip $comment etc.
            _fixtures[prop.Name] = new Fixture(
                prop.Value.GetProperty("$schema_ref").GetString()!,
                prop.Value.GetProperty("description").GetString()!,
                prop.Value.GetProperty("instance").GetRawText());
        }
    }

    public Task DisposeAsync() => Task.CompletedTask;

    [Fact]
    public void ExposesTheFiveDocumentedFixtures()
    {
        Assert.Contains("create_session_request", _fixtures.Keys);
        Assert.Contains("create_session_response", _fixtures.Keys);
        Assert.Contains("send_message_request", _fixtures.Keys);
        Assert.Contains("stream_chunk_event", _fixtures.Keys);
        Assert.Contains("eventual_response_event", _fixtures.Keys);
    }

    [Fact]
    public void ValidatesEveryFixtureAgainstItsDeclaredSchemaRef()
    {
        Assert.NotEmpty(_fixtures);
        foreach (var (name, fixture) in _fixtures)
        {
            var result = _validator.ValidateAt(fixture.SchemaRef, fixture.InstanceJson);
            Assert.True(result.IsValid,
                $"{name} ({fixture.SchemaRef}): {result.FormatErrors()}");
        }
    }

    [Fact]
    public void RejectsAFixtureMutatedToViolateItsSchema()
    {
        var fixture = _fixtures["stream_chunk_event"];
        var node = System.Text.Json.Nodes.JsonNode.Parse(fixture.InstanceJson)!.AsObject();

        // Drop a required field (`data`) and inject an unexpected property — both
        // violate the schema (required + additionalProperties:false).
        node.Remove("data");
        node["unexpectedField"] = "nope";

        var result = _validator.ValidateAt(fixture.SchemaRef, node.ToJsonString());
        Assert.False(result.IsValid);
        Assert.NotEmpty(result.Errors);
    }

    [Fact]
    public void ValidateActionRoutesSendMessageToItsSchema()
    {
        var send = _fixtures["send_message_request"];
        var result = _validator.ValidateAction(ActionTypes.SendMessage, send.InstanceJson);
        Assert.True(result.IsValid, result.FormatErrors());
    }

    [Fact]
    public void ValidateEventRoutesStreamChunkToItsSchema()
    {
        var chunk = _fixtures["stream_chunk_event"];
        var result = _validator.ValidateEvent(EventTypes.StreamChunk, chunk.InstanceJson);
        Assert.True(result.IsValid, result.FormatErrors());
    }

    [Theory]
    [InlineData("list_conversations_request")]
    [InlineData("list_conversations_search_page_request")]
    public void ValidateActionRoutesListConversationsToItsSchema(string fixtureName)
    {
        var request = _fixtures[fixtureName];
        var result = _validator.ValidateAction(ActionTypes.ListConversations, request.InstanceJson);
        Assert.True(result.IsValid, result.FormatErrors());
    }

    [Fact]
    public void ListConversationsResponseFixtureDeserializesIntoGeneratedTypes()
    {
        var page = System.Text.Json.JsonSerializer.Deserialize<SmooAI.SmoothOperator.Generated.ListConversationsResponse>(
            _fixtures["list_conversations_response"].InstanceJson)!;
        Assert.Single(page.Conversations);
        Assert.True(page.HasMore);
        Assert.False(string.IsNullOrEmpty(page.NextCursor));
        Assert.Equal("33333333-3333-3333-3333-333333333333", page.Conversations.First().ConversationId);
    }

    [Fact]
    public void ValidateActionRejectsAMalformedAction()
    {
        // Missing required `message` field for send_message.
        var result = _validator.ValidateAction(
            ActionTypes.SendMessage,
            """{"action":"send_message","sessionId":"x"}""");
        Assert.False(result.IsValid);
    }

    private sealed record Fixture(string SchemaRef, string Description, string InstanceJson);
}
