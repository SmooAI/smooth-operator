using System.Runtime.CompilerServices;
using System.Text.Json.Nodes;
using Microsoft.Extensions.AI;
using SmooAI.SmoothOperator.Core;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// The optional fast-model preamble (<c>SMOOTH_AGENT_PREAMBLE_MODEL</c>, pearl th-9a5794): one
/// ephemeral <c>stream_preamble</c> sentence generated IN PARALLEL with the turn to cover the main
/// model's time-to-first-token.
///
/// This feature is mostly defined by what must NOT happen, so most of these tests are negatives: off
/// by default with no extra LLM call, silently suppressed once the real answer starts, swallowed on
/// failure, and never persisted or folded into the response. Ordering is controlled with gated fakes
/// (and <see cref="TurnRunner.PreambleCompleted"/>) — never with sleeps.
/// </summary>
public class StreamPreambleTests
{
    private const string PreambleEnvVar = "SMOOTH_AGENT_PREAMBLE_MODEL";
    private const string FastModel = "groq-gpt-oss-20b";
    private const string PreambleText = "Let me pull up your recent conversations.";

    [Fact]
    public async Task EnvUnset_NoPreambleEvent_AndPreambleModelNeverCalled()
    {
        using var _ = new EnvScope(PreambleEnvVar, null);
        var preamble = new GatedChatClient(PreambleText);
        var events = new List<JsonObject>();
        var (runner, conversationId) = await BuildRunnerAsync(new MockChatClient().PushText("The window is 30 days."), preamble);

        var result = await runner.RunAsync(conversationId, "r1", "How long is the return window?", events.Add);
        await runner.PreambleCompleted;

        Assert.Equal(0, preamble.Calls); // the whole point: OFF means no extra LLM call at all
        Assert.DoesNotContain(events, e => e["type"]!.GetValue<string>() == "stream_preamble");
        Assert.Equal("The window is 30 days.", result.Reply);
    }

    [Theory]
    [InlineData("")]
    [InlineData("   ")]
    public async Task EnvBlank_IsTreatedAsOff(string value)
    {
        using var _ = new EnvScope(PreambleEnvVar, value);
        var preamble = new GatedChatClient(PreambleText);
        var events = new List<JsonObject>();
        var (runner, conversationId) = await BuildRunnerAsync(new MockChatClient().PushText("ok"), preamble);

        await runner.RunAsync(conversationId, "r1", "hi", events.Add);
        await runner.PreambleCompleted;

        Assert.Equal(0, preamble.Calls);
        Assert.DoesNotContain(events, e => e["type"]!.GetValue<string>() == "stream_preamble");
    }

    [Fact]
    public async Task EnvSet_EmitsPreambleEvent_WithDocumentedShape()
    {
        using var _ = new EnvScope(PreambleEnvVar, FastModel);
        var preamble = new GatedChatClient(PreambleText);
        var events = new List<JsonObject>();
        var preambleSeen = new TaskCompletionSource();
        // Deterministic ordering: the main answer does not start streaming until the preamble event has
        // landed in the sink, so the race guard cannot suppress it in this test.
        var main = new MockChatClient().PushText("You had three conversations last week.").GateStreamOn(preambleSeen.Task);
        var (runner, conversationId) = await BuildRunnerAsync(main, preamble);

        var result = await runner.RunAsync(conversationId, "r1", "What did we talk about?", ev =>
        {
            events.Add(ev);
            if (ev["type"]!.GetValue<string>() == "stream_preamble")
            {
                preambleSeen.TrySetResult();
            }
        });
        await runner.PreambleCompleted;

        var ev = Assert.Single(events, e => e["type"]!.GetValue<string>() == "stream_preamble");
        Assert.Equal("r1", ev["requestId"]!.GetValue<string>());
        Assert.Equal(PreambleText, ev["token"]!.GetValue<string>());
        Assert.Equal("r1", ev["data"]!["requestId"]!.GetValue<string>()); // token is duplicated inside data
        Assert.Equal(PreambleText, ev["data"]!["token"]!.GetValue<string>());
        Assert.True(ev["timestamp"]!.GetValue<long>() > 0);
        // …and it precedes the real answer, which is what it exists to cover.
        Assert.True(events.FindIndex(e => e["type"]!.GetValue<string>() == "stream_preamble")
            < events.FindIndex(e => e["type"]!.GetValue<string>() == "stream_token"));
        Assert.Equal("You had three conversations last week.", result.Reply);
    }

    [Fact]
    public async Task EnvSet_UsesPreambleModel_TightTokenCap_AndOnlyTheUserMessage()
    {
        using var _ = new EnvScope(PreambleEnvVar, FastModel);
        var preamble = new GatedChatClient(PreambleText);
        var (runner, conversationId) = await BuildRunnerAsync(new MockChatClient().PushText("ok"), preamble);

        await runner.RunAsync(conversationId, "r1", "How long is the return window?", _ => { });
        await runner.PreambleCompleted;

        Assert.Equal(1, preamble.Calls);
        Assert.Equal(FastModel, preamble.LastOptions!.ModelId); // only the model id is overridden…
        Assert.Equal(64, preamble.LastOptions!.MaxOutputTokens); // …plus the one-sentence token cap
        // System prompt + the user's message only — no tool results, no prior history.
        Assert.Collection(
            preamble.LastMessages!,
            m => Assert.Equal(ChatRole.System, m.Role),
            m =>
            {
                Assert.Equal(ChatRole.User, m.Role);
                Assert.Equal("How long is the return window?", m.Text);
            });
        Assert.StartsWith("You are the assistant's voice while it works.", preamble.LastMessages![0].Text, StringComparison.Ordinal);
    }

    [Fact]
    public async Task AnswerStartsFirst_LatePreambleIsSuppressed()
    {
        using var _ = new EnvScope(PreambleEnvVar, FastModel);
        // The preamble call parks until the test releases it — so the ordering is forced, not hoped for:
        // the whole turn (every stream_token) completes BEFORE the preamble resolves.
        var gate = new TaskCompletionSource();
        var preamble = new GatedChatClient(PreambleText, gate.Task);
        var events = new List<JsonObject>();
        var (runner, conversationId) = await BuildRunnerAsync(new MockChatClient().PushText("Thirty days."), preamble);

        var result = await runner.RunAsync(conversationId, "r1", "How long is the return window?", events.Add);
        Assert.Contains(events, e => e["type"]!.GetValue<string>() == "stream_token"); // the answer HAS started

        gate.SetResult();
        await runner.PreambleCompleted; // the preamble task has now fully run — nothing more can arrive

        Assert.Equal(1, preamble.Calls); // it really was called…
        Assert.DoesNotContain(events, e => e["type"]!.GetValue<string>() == "stream_preamble"); // …and dropped
        Assert.Equal("Thirty days.", result.Reply);
    }

    [Fact]
    public async Task PreambleFailure_IsSwallowed_TurnCompletesWithNoErrorEvent()
    {
        using var _ = new EnvScope(PreambleEnvVar, FastModel);
        var preamble = new ThrowingChatClient();
        var events = new List<JsonObject>();
        var (runner, conversationId) = await BuildRunnerAsync(new MockChatClient().PushText("Thirty days."), preamble);

        var result = await runner.RunAsync(conversationId, "r1", "How long is the return window?", events.Add);
        await runner.PreambleCompleted; // completes successfully — the exception never escapes

        Assert.Equal("Thirty days.", result.Reply);
        Assert.DoesNotContain(events, e => e["type"]!.GetValue<string>() == "error");
        Assert.DoesNotContain(events, e => e["type"]!.GetValue<string>() == "stream_preamble");
    }

    [Fact]
    public async Task PreambleText_IsEphemeral_NeverPersisted_NeverInResponse()
    {
        using var _ = new EnvScope(PreambleEnvVar, FastModel);
        var preamble = new GatedChatClient(PreambleText);
        var preambleSeen = new TaskCompletionSource();
        var main = new MockChatClient().PushText("You had three conversations last week.").GateStreamOn(preambleSeen.Task);
        var store = new InMemorySessionStore();
        var session = await store.CreateSessionAsync("agent-1", null, null);
        var runner = new TurnRunner(main, store, preambleChatClient: preamble);

        var result = await runner.RunAsync(session.ConversationId, "r1", "What did we talk about?", ev =>
        {
            if (ev["type"]!.GetValue<string>() == "stream_preamble")
            {
                preambleSeen.TrySetResult();
            }
        });
        await runner.PreambleCompleted;

        // The reply is what eventual_response carries — it must be the answer alone.
        Assert.Equal("You had three conversations last week.", result.Reply);
        Assert.DoesNotContain(PreambleText, result.Reply, StringComparison.Ordinal);
        var messages = await store.ListMessagesAsync(session.ConversationId, 50);
        Assert.DoesNotContain(messages, m => m.Text.Contains(PreambleText, StringComparison.Ordinal));
    }

    private static async Task<(TurnRunner Runner, string ConversationId)> BuildRunnerAsync(IChatClient main, IChatClient preamble)
    {
        var store = new InMemorySessionStore();
        var session = await store.CreateSessionAsync("agent-1", null, null);
        return (new TurnRunner(main, store, preambleChatClient: preamble), session.ConversationId);
    }

    /// <summary>Sets an env var for the duration of a test and restores whatever was there before.</summary>
    private sealed class EnvScope : IDisposable
    {
        private readonly string _key;
        private readonly string? _prior;

        public EnvScope(string key, string? value)
        {
            _key = key;
            _prior = Environment.GetEnvironmentVariable(key);
            Environment.SetEnvironmentVariable(key, value);
        }

        public void Dispose() => Environment.SetEnvironmentVariable(_key, _prior);
    }

    /// <summary>
    /// A preamble <see cref="IChatClient"/> that records every call and can be parked on a gate, so a
    /// test controls exactly when the preamble resolves relative to the answer.
    /// </summary>
    private sealed class GatedChatClient(string text, Task? gate = null) : IChatClient
    {
        public int Calls { get; private set; }
        public ChatOptions? LastOptions { get; private set; }
        public IReadOnlyList<ChatMessage>? LastMessages { get; private set; }

        public async Task<ChatResponse> GetResponseAsync(IEnumerable<ChatMessage> messages, ChatOptions? options = null, CancellationToken cancellationToken = default)
        {
            Calls++;
            LastOptions = options;
            LastMessages = messages.ToArray();
            if (gate is not null)
            {
                await gate.ConfigureAwait(false);
            }
            return new ChatResponse(new ChatMessage(ChatRole.Assistant, text));
        }

        public IAsyncEnumerable<ChatResponseUpdate> GetStreamingResponseAsync(IEnumerable<ChatMessage> messages, ChatOptions? options = null, CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("the preamble uses the non-streaming path");

        public object? GetService(Type serviceType, object? serviceKey = null) => null;

        public void Dispose()
        {
        }
    }

    /// <summary>A preamble client whose call always fails — the bad-model-id / gateway-down case.</summary>
    private sealed class ThrowingChatClient : IChatClient
    {
        public Task<ChatResponse> GetResponseAsync(IEnumerable<ChatMessage> messages, ChatOptions? options = null, CancellationToken cancellationToken = default) =>
            throw new HttpRequestException("model not found");

        public IAsyncEnumerable<ChatResponseUpdate> GetStreamingResponseAsync(IEnumerable<ChatMessage> messages, ChatOptions? options = null, CancellationToken cancellationToken = default) =>
            throw new NotSupportedException();

        public object? GetService(Type serviceType, object? serviceKey = null) => null;

        public void Dispose()
        {
        }
    }
}

/// <summary>
/// Wraps a scripted client so its streaming answer does not begin until a gate task completes — the
/// deterministic way to make the preamble land BEFORE the first answer token.
/// </summary>
internal sealed class GatedStreamChatClient(IChatClient inner, Task gate) : IChatClient
{
    private static readonly TimeSpan GateTimeout = TimeSpan.FromSeconds(10);

    public Task<ChatResponse> GetResponseAsync(IEnumerable<ChatMessage> messages, ChatOptions? options = null, CancellationToken cancellationToken = default) =>
        inner.GetResponseAsync(messages, options, cancellationToken);

    public async IAsyncEnumerable<ChatResponseUpdate> GetStreamingResponseAsync(
        IEnumerable<ChatMessage> messages,
        ChatOptions? options = null,
        [EnumeratorCancellation] CancellationToken cancellationToken = default)
    {
        // Bounded so a broken gate fails the test loudly instead of hanging the suite forever.
        await gate.WaitAsync(GateTimeout, cancellationToken).ConfigureAwait(false);
        await foreach (var update in inner.GetStreamingResponseAsync(messages, options, cancellationToken).ConfigureAwait(false))
        {
            yield return update;
        }
    }

    public object? GetService(Type serviceType, object? serviceKey = null) => inner.GetService(serviceType, serviceKey);

    public void Dispose() => inner.Dispose();
}

internal static class GatedStreamChatClientExtensions
{
    public static IChatClient GateStreamOn(this IChatClient inner, Task gate) => new GatedStreamChatClient(inner, gate);
}
