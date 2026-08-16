using System.Runtime.CompilerServices;
using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.Extensions.AI;
using SmooAI.SmoothOperator.Core;
using SmooAI.SmoothOperator.Server;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// File-transfer parity with the Rust reference (PR #342): <c>send_message.images[]</c> attach as
/// vision content parts on the user turn, <c>send_message.files[]</c> surface on the per-turn
/// <see cref="TurnContext"/> for host tools (never sent to the model), and a host tool's directive is
/// drained onto <c>eventual_response.directive</c> — all fail-soft and back-compatible.
/// </summary>
public class FileTransferTests
{
    private static readonly string PngDataUri = "data:image/png;base64,iVBORw0KGgo=";

    private static async Task<string> CreateSessionAsync(FrameDispatcher dispatcher, List<JsonObject> events)
    {
        await dispatcher.DispatchAsync("""{"action":"create_conversation_session","requestId":"r1"}""", events.Add);
        var sessionId = events[0]["data"]!["sessionId"]!.GetValue<string>();
        events.Clear();
        return sessionId;
    }

    // ── images[] → vision content parts ──────────────────────────────────────

    [Fact]
    public async Task Images_AttachAsContentParts_OnTheUserTurn()
    {
        var chat = new RecordingChatClient("I see a diagram.");
        var dispatcher = new FrameDispatcher(new InMemorySessionStore(), chat);
        var events = new List<JsonObject>();
        var sessionId = await CreateSessionAsync(dispatcher, events);

        var frame = $$"""
            {"action":"send_message","requestId":"r2","sessionId":"{{sessionId}}","message":"Describe this.",
             "images":[{"url":"https://example.com/a.png","detail":"high"},{"url":"{{PngDataUri}}"}]}
            """;
        await dispatcher.DispatchAsync(frame, events.Add);
        await dispatcher.WaitForTurnsAsync();

        // The model received both images as content parts: a UriContent (remote) and a DataContent (data:).
        var imageParts = chat.LastMessages
            .SelectMany(m => m.Contents)
            .Where(c => c is UriContent or DataContent)
            .ToList();
        Assert.Equal(2, imageParts.Count);
        Assert.Contains(imageParts, c => c is UriContent u && u.Uri.ToString() == "https://example.com/a.png");
        Assert.Contains(imageParts, c => c is DataContent);

        // The detail hint rides on the content part.
        var remote = imageParts.OfType<UriContent>().Single();
        Assert.Equal("high", remote.AdditionalProperties?["detail"]);

        // The live user turn is still the typed text (not empty, not duplicated onto the image message).
        var lastUser = chat.LastMessages.Last(m => m.Role == ChatRole.User);
        Assert.Equal("Describe this.", lastUser.Text);
        Assert.DoesNotContain(chat.LastMessages, m => m.Role == ChatRole.User && string.IsNullOrEmpty(m.Text) && m.Contents.All(c => c is TextContent));
    }

    [Fact]
    public async Task Images_Malformed_AreDropped_TurnStillRuns()
    {
        var chat = new RecordingChatClient("ok");
        var dispatcher = new FrameDispatcher(new InMemorySessionStore(), chat);
        var events = new List<JsonObject>();
        var sessionId = await CreateSessionAsync(dispatcher, events);

        // One entry has no url (malformed → dropped), one has an unsupported scheme (dropped), one is valid.
        var frame = $$"""
            {"action":"send_message","requestId":"r2","sessionId":"{{sessionId}}","message":"hi",
             "images":[{"detail":"low"},{"url":"ftp://nope/x.png"},{"url":"{{PngDataUri}}"}]}
            """;
        await dispatcher.DispatchAsync(frame, events.Add);
        await dispatcher.WaitForTurnsAsync();

        // Only the valid data: image survives; the turn is not rejected.
        var imageParts = chat.LastMessages.SelectMany(m => m.Contents).Where(c => c is UriContent or DataContent).ToList();
        Assert.Single(imageParts);
        Assert.IsType<DataContent>(imageParts[0]);
        Assert.Equal("eventual_response", events[^1]["type"]!.GetValue<string>());
    }

    [Fact]
    public async Task NoImages_IsByteIdenticalTextTurn()
    {
        var chat = new RecordingChatClient("ok");
        var dispatcher = new FrameDispatcher(new InMemorySessionStore(), chat);
        var events = new List<JsonObject>();
        var sessionId = await CreateSessionAsync(dispatcher, events);

        await dispatcher.DispatchAsync($$"""{"action":"send_message","requestId":"r2","sessionId":"{{sessionId}}","message":"hi"}""", events.Add);
        await dispatcher.WaitForTurnsAsync();

        Assert.DoesNotContain(chat.LastMessages.SelectMany(m => m.Contents), c => c is UriContent or DataContent);
    }

    // ── files[] → per-turn tool context ──────────────────────────────────────

    [Fact]
    public async Task Files_AreSurfacedOnTurnContext_NotSentToModel()
    {
        IReadOnlyList<UserFile>? seen = null;
        var inspectTool = AIFunctionFactory.Create(() =>
        {
            seen = TurnContext.Current?.Files;
            return "inspected";
        }, "inspect_files");

        var chat = new ToolThenTextChatClient("inspect_files", new JsonObject(), "done");
        var dispatcher = new FrameDispatcher(new InMemorySessionStore(), chat, tools: new List<AITool> { inspectTool });
        var events = new List<JsonObject>();
        var sessionId = await CreateSessionAsync(dispatcher, events);

        var frame = $$"""
            {"action":"send_message","requestId":"r2","sessionId":"{{sessionId}}","message":"read the file",
             "files":[{"name":"data.csv","mimeType":"text/csv","url":"data:text/csv;base64,YSxi"}]}
            """;
        await dispatcher.DispatchAsync(frame, events.Add);
        await dispatcher.WaitForTurnsAsync();

        Assert.NotNull(seen);
        var file = Assert.Single(seen!);
        Assert.Equal("data.csv", file.Name);
        Assert.Equal("text/csv", file.MimeType);

        // Files never become model content parts (no image_url; not in the message text).
        Assert.DoesNotContain(chat.LastMessages.SelectMany(m => m.Contents), c => c is UriContent or DataContent);
    }

    // ── tool directive → eventual_response.directive ─────────────────────────

    [Fact]
    public async Task ToolDirective_IsDrainedOntoEventualResponse()
    {
        var directive = new JsonObject
        {
            ["type"] = "send_file",
            ["files"] = new JsonArray { new JsonObject { ["name"] = "report.pdf", ["mimeType"] = "application/pdf", ["url"] = "data:application/pdf;base64,JVBER" } },
        };
        var sendFile = AIFunctionFactory.Create(() =>
        {
            TurnContext.Current!.Directive = directive.DeepClone();
            return "sent";
        }, "send_file");

        var chat = new ToolThenTextChatClient("send_file", new JsonObject(), "Here is your file.");
        var dispatcher = new FrameDispatcher(new InMemorySessionStore(), chat, tools: new List<AITool> { sendFile });
        var events = new List<JsonObject>();
        var sessionId = await CreateSessionAsync(dispatcher, events);

        await dispatcher.DispatchAsync($$"""{"action":"send_message","requestId":"r2","sessionId":"{{sessionId}}","message":"send me the report"}""", events.Add);
        await dispatcher.WaitForTurnsAsync();

        var terminal = events[^1];
        Assert.Equal("eventual_response", terminal["type"]!.GetValue<string>());
        var emitted = terminal["data"]!["data"]!["directive"]!.AsObject();
        Assert.Equal("send_file", emitted["type"]!.GetValue<string>());
        Assert.Equal("report.pdf", emitted["files"]![0]!["name"]!.GetValue<string>());

        // Still a spec-valid eventual_response.
        var validator = await ProtocolValidator.LoadAsync();
        Assert.True(validator.ValidateEvent("eventual_response", terminal.ToJsonString()).IsValid);
    }

    [Fact]
    public async Task NoToolDirective_OmitsDirective_ForBackCompat()
    {
        var chat = new RecordingChatClient("just text");
        var dispatcher = new FrameDispatcher(new InMemorySessionStore(), chat);
        var events = new List<JsonObject>();
        var sessionId = await CreateSessionAsync(dispatcher, events);

        await dispatcher.DispatchAsync($$"""{"action":"send_message","requestId":"r2","sessionId":"{{sessionId}}","message":"hi"}""", events.Add);
        await dispatcher.WaitForTurnsAsync();

        Assert.Null(events[^1]["data"]!["data"]!.AsObject()["directive"]);
    }

    // ── ProtocolEvents.EventualResponse directive plumbing (pure unit) ────────

    [Fact]
    public void EventualResponse_OmitsDirective_WhenNull()
    {
        var ev = ProtocolEvents.EventualResponse("r", 200, "m1", new JsonObject(), false, null, directive: null);
        Assert.Null(ev["data"]!["data"]!.AsObject()["directive"]);
    }

    [Fact]
    public void EventualResponse_AttachesDirective_WhenPresent()
    {
        var directive = new JsonObject { ["type"] = "send_file" };
        var ev = ProtocolEvents.EventualResponse("r", 200, "m1", new JsonObject(), false, null, directive: directive);
        Assert.Equal("send_file", ev["data"]!["data"]!["directive"]!["type"]!.GetValue<string>());
    }

    // ── test doubles ─────────────────────────────────────────────────────────

    /// <summary>Emits one tool call on the first streamed turn, then plain text on the next — driving the
    /// engine's agentic loop to invoke a host tool exactly once. Records the final message list.</summary>
    private sealed class ToolThenTextChatClient : IChatClient
    {
        private readonly string _toolName;
        private readonly JsonObject _args;
        private readonly string _text;
        private int _calls;

        public ToolThenTextChatClient(string toolName, JsonObject args, string text)
        {
            _toolName = toolName;
            _args = args;
            _text = text;
        }

        public IReadOnlyList<ChatMessage> LastMessages { get; private set; } = Array.Empty<ChatMessage>();

        private ChatResponse Next()
        {
            if (Interlocked.Increment(ref _calls) == 1)
            {
                var args = _args.Deserialize<Dictionary<string, object?>>() ?? new();
                return new ChatResponse(new ChatMessage(ChatRole.Assistant, new List<AIContent>
                {
                    new FunctionCallContent(Guid.NewGuid().ToString("N"), _toolName, args),
                }));
            }
            return new ChatResponse(new ChatMessage(ChatRole.Assistant, _text));
        }

        public Task<ChatResponse> GetResponseAsync(IEnumerable<ChatMessage> messages, ChatOptions? options = null, CancellationToken cancellationToken = default)
        {
            LastMessages = messages.ToList();
            return Task.FromResult(Next());
        }

        public async IAsyncEnumerable<ChatResponseUpdate> GetStreamingResponseAsync(
            IEnumerable<ChatMessage> messages, ChatOptions? options = null, [EnumeratorCancellation] CancellationToken cancellationToken = default)
        {
            LastMessages = messages.ToList();
            foreach (var update in Next().ToChatResponseUpdates())
            {
                await Task.Yield();
                yield return update;
            }
        }

        public object? GetService(Type serviceType, object? serviceKey = null) => null;

        public void Dispose()
        {
        }
    }
}
