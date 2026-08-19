// Rich Interaction + ephemeral-stream frames must survive the client's own dispatch.
//
// ConformanceTests already validates the interaction fixtures against the schemas, but
// it never feeds them to ServerEventConverter / the client's frame handler — which is
// exactly the blind spot that let interaction_required, interaction_invalid,
// stream_preamble and stream_reasoning be dropped at runtime (the converter threw a
// JsonException, and SmoothAgentClient caught and discarded it).

using System.Text.Json;

namespace SmooAI.SmoothOperator.Tests;

public sealed class InteractionTests : IAsyncLifetime
{
    private ProtocolValidator _validator = null!;
    private Dictionary<string, JsonElement> _fixtures = null!;

    private const string SubmitRef = "actions/submit-interaction.schema.json#/$defs/Request";

    public async Task InitializeAsync()
    {
        _validator = await ProtocolValidator.LoadAsync(SpecPaths.SpecDir);

        var raw = await File.ReadAllTextAsync(Path.Combine(SpecPaths.SpecDir, "conformance", "fixtures.json"));
        using var doc = JsonDocument.Parse(raw);
        _fixtures = new Dictionary<string, JsonElement>();
        foreach (var prop in doc.RootElement.EnumerateObject())
        {
            if (prop.Name.StartsWith('$')) continue;
            _fixtures[prop.Name] = prop.Value.GetProperty("instance").Clone();
        }
    }

    public Task DisposeAsync() => Task.CompletedTask;

    private static (SmoothAgentClient Client, MockTransport Transport) MakeClient()
    {
        var transport = new MockTransport();
        var counter = 0;
        var client = new SmoothAgentClient(new SmoothAgentClientOptions
        {
            Url = "wss://test",
            Transport = transport,
            GenerateRequestId = () => $"req-test-{++counter}",
            RequestTimeout = TimeSpan.FromSeconds(1),
        });
        return (client, transport);
    }

    /// <summary>Rewrite a fixture's requestId at every nesting depth so it correlates
    /// with the turn under test.</summary>
    private static string Retarget(JsonElement instance, string requestId)
    {
        var node = System.Text.Json.Nodes.JsonNode.Parse(instance.GetRawText())!;
        void Walk(System.Text.Json.Nodes.JsonNode? n)
        {
            if (n is System.Text.Json.Nodes.JsonObject obj)
            {
                if (obj.ContainsKey("requestId")) obj["requestId"] = requestId;
                foreach (var kv in obj.ToList()) Walk(kv.Value);
            }
        }
        Walk(node);
        return node.ToJsonString();
    }

    /// <summary>Build a JSON frame, substituting the {rid} placeholder with the requestId
    /// (same approach as ClientTests — raw-string interpolation collides with JSON braces).</summary>
    private static string Frame(string template, string requestId) => template.Replace("{rid}", requestId);

    private static string Terminal(string requestId) => Frame(
        """{"type":"eventual_response","requestId":"{rid}","status":200,"data":{"requestId":"{rid}","status":200,"data":{"messageId":"msg-1","response":{"responseParts":["done"]},"needsEscalation":false}}}""",
        requestId);

    // ───────────────────────────── drift guard ─────────────────────────────

    /// <summary>
    /// Derives the expected discriminator set from spec/events/*.schema.json — the source
    /// of truth — rather than from a list maintained here. A guard asserting against its
    /// own hand-written constant would lock the drift in instead of catching it. Adding an
    /// event schema without wiring it into ServerEventConverter fails HERE, not silently
    /// at runtime.
    /// </summary>
    [Fact]
    public void ServerEventConverterCoversEverySpecEvent()
    {
        var specEvents = Directory
            .EnumerateFiles(Path.Combine(SpecPaths.SpecDir, "events"), "*.schema.json")
            .Select(path =>
            {
                using var doc = JsonDocument.Parse(File.ReadAllText(path));
                return doc.RootElement.TryGetProperty("properties", out var props)
                       && props.TryGetProperty("type", out var t)
                       && t.TryGetProperty("const", out var c)
                    ? c.GetString()
                    : null;
            })
            .Where(s => s is not null)
            .Select(s => s!)
            .ToList();

        Assert.NotEmpty(specEvents);

        var options = new JsonSerializerOptions();
        options.Converters.Add(new ServerEventConverter());

        var missing = new List<string>();
        foreach (var disc in specEvents)
        {
            // A minimal frame carrying only the discriminator: the converter must at least
            // recognise the type. Unknown types throw JsonException, which the client's
            // frame handler swallows — the exact silent-drop path.
            try
            {
                JsonSerializer.Deserialize<ServerEvent>(
                    """{"type":"__DISC__","data":{}}""".Replace("__DISC__", disc), options);
            }
            catch (JsonException ex) when (ex.Message.Contains("Unknown ServerEvent type"))
            {
                missing.Add(disc);
            }
            catch (JsonException)
            {
                // Payload-shape mismatch on a minimal frame is fine — the discriminator
                // was recognised, which is all this guard is asserting.
            }
        }

        Assert.True(missing.Count == 0,
            $"spec/events declares [{string.Join(", ", missing.OrderBy(x => x))}] but ServerEventConverter.ByType omits them: " +
            "the converter throws and SmoothAgentClient drops the frame silently");

        // EventTypes.All is a SECOND hand-maintained set over the same discriminators
        // and drifts independently of the converter map — it was stale in exactly the
        // same way. Assert it against the spec too, not against the converter.
        var missingFromAll = specEvents.Where(d => !EventTypes.All.Contains(d)).OrderBy(x => x).ToList();
        Assert.True(missingFromAll.Count == 0,
            $"spec/events declares [{string.Join(", ", missingFromAll)}] but EventTypes.All omits them");
    }

    /// <summary>Same guard for the client→server direction.</summary>
    [Fact]
    public void ActionTypesCoverEverySpecAction()
    {
        var specActions = Directory
            .EnumerateFiles(Path.Combine(SpecPaths.SpecDir, "actions"), "*.schema.json")
            .Select(path =>
            {
                using var doc = JsonDocument.Parse(File.ReadAllText(path));
                var root = doc.RootElement;
                // Action schemas nest the frame under $defs/Request.
                if (root.TryGetProperty("$defs", out var defs) && defs.TryGetProperty("Request", out var req))
                    root = req;
                return root.TryGetProperty("properties", out var props)
                       && props.TryGetProperty("action", out var a)
                       && a.TryGetProperty("const", out var c)
                    ? c.GetString()
                    : null;
            })
            .Where(s => s is not null)
            .Select(s => s!)
            .ToList();

        Assert.NotEmpty(specActions);

        var missing = specActions.Where(a => !ActionTypes.All.Contains(a)).OrderBy(x => x).ToList();
        Assert.True(missing.Count == 0,
            $"spec/actions declares [{string.Join(", ", missing)}] but ActionTypes.All omits them");
    }

    // ─────────────────── fixtures through the real dispatch ────────────────

    [Theory]
    [InlineData("interaction_required_event", "interaction_required")]
    [InlineData("interaction_invalid_event", "interaction_invalid")]
    public void InteractionFixturesDeserializeIntoTypedEvents(string fixtureName, string expectedType)
    {
        var options = new JsonSerializerOptions();
        options.Converters.Add(new ServerEventConverter());

        var ev = JsonSerializer.Deserialize<ServerEvent>(_fixtures[fixtureName].GetRawText(), options);

        Assert.NotNull(ev);
        Assert.Equal(expectedType, ev!.Type);
    }

    [Fact]
    public async Task InteractionRequiredAndInvalidReachTheTurn()
    {
        var (client, transport) = MakeClient();
        await client.ConnectAsync();

        var turn = client.SendMessageAsync(new SendMessageAction { SessionId = "sess-1", Message = "quote please" });
        var reqId = transport.LastRequestId();

        var collected = new List<ServerEvent>();
        var iterate = Task.Run(async () =>
        {
            await foreach (var ev in turn)
                collected.Add(ev);
        });

        transport.Emit(Retarget(_fixtures["interaction_required_event"], reqId));
        transport.Emit(Retarget(_fixtures["interaction_invalid_event"], reqId));
        transport.Emit(Terminal(reqId));

        await turn.Completion;
        await iterate.WaitAsync(TimeSpan.FromSeconds(5));

        var types = collected.Select(e => e.Type).ToList();
        Assert.Contains("interaction_required", types);
        Assert.Contains("interaction_invalid", types);

        var park = Assert.IsType<InteractionRequiredEvent>(collected.First(e => e.Type == "interaction_required"));
        Assert.Equal("identity_intake", park.Data.Data.Kind);
        Assert.Equal("88888888-8888-8888-8888-888888888888", park.Data.Data.InteractionId);

        var invalid = Assert.IsType<InteractionInvalidEvent>(collected.First(e => e.Type == "interaction_invalid"));
        Assert.Equal(new[] { "email" }, invalid.Data.Data.Errors.Select(e => e.Field).ToArray());
    }

    [Fact]
    public async Task StreamPreambleAndReasoningReachTheTurn()
    {
        var (client, transport) = MakeClient();
        await client.ConnectAsync();

        var turn = client.SendMessageAsync(new SendMessageAction { SessionId = "sess-1", Message = "think about it" });
        var reqId = transport.LastRequestId();

        // Neither has a conformance fixture, so validate the built frames against their
        // own schemas first — a frame the spec would reject proves nothing.
        var frames = new (string Type, string SchemaRef, string Token)[]
        {
            ("stream_preamble", "events/stream-preamble.schema.json", "Looking that up"),
            ("stream_reasoning", "events/stream-reasoning.schema.json", "let me think"),
        };
        var built = new List<string>();
        foreach (var (type, schemaRef, token) in frames)
        {
            var json = Frame(
                    """{"type":"__T__","requestId":"{rid}","token":"__TOK__","data":{"requestId":"{rid}","token":"__TOK__"}}""",
                    reqId)
                .Replace("__T__", type)
                .Replace("__TOK__", token);
            var result = _validator.ValidateAt(schemaRef, json);
            Assert.True(result.IsValid, $"{type} test frame is not spec-valid: {result.FormatErrors()}");
            built.Add(json);
        }

        var collected = new List<ServerEvent>();
        var iterate = Task.Run(async () =>
        {
            await foreach (var ev in turn)
                collected.Add(ev);
        });

        foreach (var json in built) transport.Emit(json);
        transport.Emit(Terminal(reqId));

        await turn.Completion;
        await iterate.WaitAsync(TimeSpan.FromSeconds(5));

        var types = collected.Select(e => e.Type).ToList();
        Assert.Contains("stream_preamble", types);
        Assert.Contains("stream_reasoning", types);

        var preamble = Assert.IsType<StreamPreambleEvent>(collected.First(e => e.Type == "stream_preamble"));
        Assert.Equal("Looking that up", preamble.Token);
        var reasoning = Assert.IsType<StreamReasoningEvent>(collected.First(e => e.Type == "stream_reasoning"));
        Assert.Equal("let me think", reasoning.Data.Token);
    }

    // ───────────────────────────── the submit verb ─────────────────────────

    [Fact]
    public async Task SubmitInteractionProducesASpecValidFrame()
    {
        var (client, transport) = MakeClient();
        await client.ConnectAsync();

        await client.SubmitInteractionAsync(
            sessionId: "22222222-2222-2222-2222-222222222222",
            requestId: "req-a1b2c3d4-0004",
            interactionId: "88888888-8888-8888-8888-888888888888",
            kind: "identity_intake",
            values: new Dictionary<string, object?>
            {
                ["name"] = "Alice Example",
                ["email"] = "alice@example.com",
                ["phone"] = "+15551234567",
            });

        var sent = transport.LastSent();
        Assert.Equal("submit_interaction", sent.GetProperty("action").GetString());
        Assert.Equal("88888888-8888-8888-8888-888888888888", sent.GetProperty("interactionId").GetString());
        Assert.False(sent.TryGetProperty("declined", out _), "declined must stay off the wire when not declining");

        var result = _validator.ValidateAt(SubmitRef, transport.Sent[^1]);
        Assert.True(result.IsValid, result.FormatErrors());
    }

    [Fact]
    public async Task SubmitInteractionDeclinedOmitsValues()
    {
        var (client, transport) = MakeClient();
        await client.ConnectAsync();

        await client.SubmitInteractionAsync(
            sessionId: "22222222-2222-2222-2222-222222222222",
            requestId: "req-a1b2c3d4-0004",
            interactionId: "88888888-8888-8888-8888-888888888888",
            values: new Dictionary<string, object?> { ["name"] = "ignored" },
            declined: true);

        var sent = transport.LastSent();
        Assert.True(sent.GetProperty("declined").GetBoolean());
        Assert.False(sent.TryGetProperty("values", out _), "values must be omitted when declining");

        var result = _validator.ValidateAt(SubmitRef, transport.Sent[^1]);
        Assert.True(result.IsValid, result.FormatErrors());
    }

    /// <summary>The ONE verb serves a second kind unchanged — choices needs no new method.</summary>
    [Fact]
    public async Task SubmitInteractionCarriesChoicesValues()
    {
        var (client, transport) = MakeClient();
        await client.ConnectAsync();

        var choices = _fixtures["choices_values"];
        var answers = JsonSerializer.Deserialize<object>(choices.GetProperty("answers").GetRawText());

        await client.SubmitInteractionAsync(
            sessionId: "22222222-2222-2222-2222-222222222222",
            requestId: "req-a1b2c3d4-0004",
            interactionId: "88888888-8888-8888-8888-888888888888",
            kind: "choices",
            values: new Dictionary<string, object?> { ["answers"] = answers });

        var sent = transport.LastSent();
        var sentAnswers = sent.GetProperty("values").GetProperty("answers");
        Assert.Equal(choices.GetProperty("answers").GetArrayLength(), sentAnswers.GetArrayLength());
        Assert.Equal("Plan", sentAnswers[0].GetProperty("header").GetString());

        var result = _validator.ValidateAt(SubmitRef, transport.Sent[^1]);
        Assert.True(result.IsValid, result.FormatErrors());
    }
}
