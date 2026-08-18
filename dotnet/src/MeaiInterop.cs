// Microsoft.Extensions.AI (MEAI) ecosystem interop.
//
// This file makes smooth-operator consumable as a first-class .NET AI
// component: a MEAI/MAF/Semantic-Kernel app can talk to the remote agent through
// the de-facto-standard IChatClient abstraction instead of the protocol-level
// SmoothAgentClient. The single highest-value alignment (per docs/DOTNET.md) is the
// streaming IChatClient facade — a MEAI app consumes the remote agent as a
// streaming IChatClient, with stream_token deltas mapped to ChatResponseUpdates.
//
// What's here
// -----------
//   • SmoothAgentChatClient : IChatClient   — facade over the remote SmoothAgentClient.
//   • SmoothAgentThread                      — AgentThread-style session handle
//                                              (sessionId/conversationId) with
//                                              RunStreamingAsync ergonomics.
//   • SmoothAgentOptions + AddSmoothAgent    — DI wiring (see Hosting.cs).
//
// What's deliberately NOT here (per docs/DOTNET.md "Do NOT copy"):
//   • No Azure/Foundry coupling, no in-process AIAgent execution. The agent runs
//     behind the WebSocket protocol; this is a thin skin over the remote client.

using System.Text;
using System.Text.Json;
using Microsoft.Extensions.AI;

namespace SmooAI.SmoothOperator;

/// <summary>
/// Configuration for the MEAI facade and its DI registration. Carries everything
/// needed to stand up a <see cref="SmoothAgentClient"/> plus the agent identity used
/// when a new session must be created implicitly.
/// </summary>
public sealed class SmoothAgentOptions
{
    /// <summary>WebSocket URL, e.g. <c>wss://realtime.prod.smooth-agent.dev</c>. Ignored when <see cref="Transport"/> is set.</summary>
    public string Url { get; set; } = string.Empty;

    /// <summary>The agent to converse with. Required when the facade must create a session implicitly.</summary>
    public string AgentId { get; set; } = string.Empty;

    /// <summary>Optional display name passed when creating a session.</summary>
    public string? UserName { get; set; }

    /// <summary>Optional email passed when creating a session.</summary>
    public string? UserEmail { get; set; }

    /// <summary>Inject a transport (for tests / custom sockets). Defaults to a WebSocket transport over <see cref="Url"/>.</summary>
    public ITransport? Transport { get; set; }

    /// <summary>Per-request timeout for non-streaming actions. Default 30s.</summary>
    public TimeSpan RequestTimeout { get; set; } = TimeSpan.FromSeconds(30);

    /// <summary>Override the default request-id generator.</summary>
    public Func<string>? GenerateRequestId { get; set; }

    /// <summary>Serializer options forwarded to the underlying client.</summary>
    public JsonSerializerOptions? JsonOptions { get; set; }

    internal SmoothAgentClientOptions ToClientOptions() => new()
    {
        Url = Url,
        Transport = Transport,
        RequestTimeout = RequestTimeout,
        GenerateRequestId = GenerateRequestId,
        JsonOptions = JsonOptions,
    };
}

/// <summary>
/// An <c>AgentThread</c>-style handle over a smooth-operator conversation
/// session. Wraps the <c>sessionId</c>/<c>conversationId</c> so multi-turn chat is
/// <c>thread.RunStreamingAsync(msg)</c> rather than manual id plumbing, mirroring the
/// Microsoft Agent Framework <c>AgentThread</c> ergonomics.
/// </summary>
public sealed class SmoothAgentThread
{
    private readonly SmoothAgentClient _client;

    /// <summary>The server session this thread is bound to.</summary>
    public string SessionId { get; }

    /// <summary>The conversation this session belongs to (the durable multi-turn id).</summary>
    public string? ConversationId { get; }

    /// <summary>The agent backing this thread, when known.</summary>
    public string? AgentId { get; }

    public SmoothAgentThread(SmoothAgentClient client, string sessionId, string? conversationId = null, string? agentId = null)
    {
        _client = client ?? throw new ArgumentNullException(nameof(client));
        SessionId = sessionId ?? throw new ArgumentNullException(nameof(sessionId));
        ConversationId = conversationId;
        AgentId = agentId;
    }

    /// <summary>
    /// Create a brand-new session/thread for <paramref name="agentId"/> and return a
    /// handle to it. The transport must already be connected.
    /// </summary>
    public static async Task<SmoothAgentThread> CreateAsync(
        SmoothAgentClient client, string agentId, string? userName = null, string? userEmail = null,
        CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(client);
        var session = await client.CreateConversationSessionAsync(
            new CreateConversationSessionAction { AgentId = agentId, UserName = userName, UserEmail = userEmail },
            cancellationToken).ConfigureAwait(false);
        return new SmoothAgentThread(client, session.SessionId, session.ConversationId, session.AgentId);
    }

    /// <summary>
    /// Send <paramref name="message"/> on this thread and return the streaming
    /// <see cref="MessageTurn"/> (async-iterate for events, await
    /// <see cref="MessageTurn.Completion"/> for the terminal response).
    /// </summary>
    public MessageTurn RunStreamingAsync(string message)
        => _client.SendMessageAsync(new SendMessageAction { SessionId = SessionId, Message = message, Stream = true });
}

/// <summary>
/// A <see cref="IChatClient"/> facade over the remote <see cref="SmoothAgentClient"/>,
/// so smooth-operator slots into any Microsoft Agent Framework /
/// Semantic-Kernel / MEAI application. Streaming maps each <c>stream_token</c> (and
/// the <c>token</c> mirror of <c>stream_chunk</c>) to a text-delta
/// <see cref="ChatResponseUpdate"/>; the terminal <c>eventual_response</c> completes
/// the stream and carries the final reply text.
/// </summary>
public sealed class SmoothAgentChatClient : IChatClient
{
    private readonly SmoothAgentClient _client;
    private readonly SmoothAgentOptions _options;
    private readonly ChatClientMetadata _metadata;
    private readonly bool _ownsClient;

    // A thread the chat client manages when the caller doesn't supply one. Lazily
    // created on first turn and reused for subsequent calls so multi-turn memory
    // works across GetResponseAsync / GetStreamingResponseAsync invocations.
    private SmoothAgentThread? _ambientThread;
    private readonly SemaphoreSlim _threadGate = new(1, 1);

    /// <summary>
    /// Wrap an existing, connected <see cref="SmoothAgentClient"/>. Pass an explicit
    /// <paramref name="thread"/> to bind every call to a known session, or omit it to
    /// let the facade create and reuse one lazily.
    /// </summary>
    public SmoothAgentChatClient(SmoothAgentClient client, SmoothAgentOptions options, SmoothAgentThread? thread = null, bool ownsClient = false)
    {
        _client = client ?? throw new ArgumentNullException(nameof(client));
        _options = options ?? throw new ArgumentNullException(nameof(options));
        _ambientThread = thread;
        _ownsClient = ownsClient;
        _metadata = new ChatClientMetadata(
            providerName: "smooth-operator",
            providerUri: TryUri(options.Url),
            defaultModelId: string.IsNullOrEmpty(options.AgentId) ? null : options.AgentId);
    }

    private static Uri? TryUri(string url)
        => Uri.TryCreate(url, UriKind.Absolute, out var u) ? u : null;

    /// <summary>The bound thread, if one has been created/supplied yet.</summary>
    public SmoothAgentThread? Thread => _ambientThread;

    // ─────────────────────────── IChatClient ───────────────────────────

    /// <summary>
    /// Send the last user message in <paramref name="messages"/> on a session
    /// (creating/reusing the ambient thread), await the terminal response, and return
    /// it as a <see cref="ChatResponse"/> with the assistant reply text.
    /// </summary>
    public async Task<ChatResponse> GetResponseAsync(
        IEnumerable<ChatMessage> messages, ChatOptions? options = null, CancellationToken cancellationToken = default)
    {
        var thread = await EnsureThreadAsync(options, cancellationToken).ConfigureAwait(false);
        var text = LastUserText(messages);

        var turn = thread.RunStreamingAsync(text);
        EventualResponseEvent? eventual;
        try
        {
            eventual = await turn.Completion.WaitAsync(cancellationToken).ConfigureAwait(false);
        }
        catch (OperationCanceledException)
        {
            throw;
        }

        // A null completion means the turn was stopped via SmoothAgentClient.Cancel while this
        // MEAI call was in flight — surface it as the idiomatic "generation cancelled" signal.
        if (eventual is null)
            throw new OperationCanceledException("The agent turn was cancelled.");

        var reply = ExtractText(eventual);
        var response = new ChatResponse(new ChatMessage(ChatRole.Assistant, reply))
        {
            ConversationId = thread.ConversationId,
            ModelId = thread.AgentId ?? _options.AgentId,
            ResponseId = eventual.Data.Payload.MessageId,
            FinishReason = ChatFinishReason.Stop,
            RawRepresentation = eventual,
        };
        return response;
    }

    /// <summary>
    /// THE key interop path: stream the agent's reply as MEAI
    /// <see cref="ChatResponseUpdate"/>s. Each <c>stream_token</c> (and the
    /// <c>token</c> mirror carried on a <c>stream_chunk</c>) becomes a text-delta
    /// update; the terminal <c>eventual_response</c> completes the stream. If the
    /// server never emitted token deltas, the terminal reply text is emitted as a
    /// single final update so callers still receive the full text.
    /// </summary>
    public async IAsyncEnumerable<ChatResponseUpdate> GetStreamingResponseAsync(
        IEnumerable<ChatMessage> messages, ChatOptions? options = null,
        [System.Runtime.CompilerServices.EnumeratorCancellation] CancellationToken cancellationToken = default)
    {
        var thread = await EnsureThreadAsync(options, cancellationToken).ConfigureAwait(false);
        var text = LastUserText(messages);

        var turn = thread.RunStreamingAsync(text);
        var responseId = (string?)null;
        var emittedAnyText = false;
        var streamed = new StringBuilder();

        await foreach (var ev in turn.WithCancellation(cancellationToken).ConfigureAwait(false))
        {
            switch (ev)
            {
                case StreamTokenEvent token:
                {
                    var delta = token.Token ?? token.Data.Token;
                    if (!string.IsNullOrEmpty(delta))
                    {
                        emittedAnyText = true;
                        streamed.Append(delta);
                        yield return TextUpdate(delta, thread, responseId);
                    }
                    break;
                }

                case StreamChunkEvent:
                    // Per-node workflow snapshots carry no user-facing text deltas
                    // (token text rides on stream_token). Skip them for the IChatClient
                    // surface; consumers wanting node state use the raw MessageTurn.
                    break;

                case EventualResponseEvent eventual:
                {
                    responseId = eventual.Data.Payload.MessageId ?? responseId;
                    var finalText = ExtractText(eventual);

                    // If we never streamed token deltas, emit the full reply once so the
                    // caller still receives text. If we did stream and the terminal text
                    // is longer (server may include the whole reply), emit the remainder.
                    var toEmit = finalText;
                    if (emittedAnyText)
                    {
                        var already = streamed.ToString();
                        toEmit = finalText.StartsWith(already, StringComparison.Ordinal)
                            ? finalText.Substring(already.Length)
                            : string.Empty;
                    }

                    var final = new ChatResponseUpdate(ChatRole.Assistant, toEmit)
                    {
                        ConversationId = thread.ConversationId,
                        ResponseId = responseId,
                        MessageId = responseId,
                        ModelId = thread.AgentId ?? _options.AgentId,
                        FinishReason = ChatFinishReason.Stop,
                        RawRepresentation = eventual,
                    };
                    yield return final;
                    break;
                }
            }
        }
    }

    /// <summary>MEAI service-resolution hook. Surfaces metadata and the wrapped clients.</summary>
    public object? GetService(Type serviceType, object? serviceKey = null)
    {
        ArgumentNullException.ThrowIfNull(serviceType);
        if (serviceKey is not null) return null;
        if (serviceType.IsInstanceOfType(this)) return this;
        if (serviceType == typeof(ChatClientMetadata)) return _metadata;
        if (serviceType == typeof(SmoothAgentClient)) return _client;
        if (serviceType == typeof(SmoothAgentThread)) return _ambientThread;
        return null;
    }

    public void Dispose()
    {
        _threadGate.Dispose();
        if (_ownsClient)
        {
            // SmoothAgentClient is IAsyncDisposable; fire-and-forget the async teardown.
            _ = _client.DisposeAsync().AsTask();
        }
    }

    // ─────────────────────────── Internals ───────────────────────────

    private async Task<SmoothAgentThread> EnsureThreadAsync(ChatOptions? options, CancellationToken cancellationToken)
    {
        // Honor an explicit ConversationId passed through ChatOptions by binding to it
        // as a session — callers that already have a session id can route this way.
        if (options?.ConversationId is { Length: > 0 } convoSession &&
            (_ambientThread is null || _ambientThread.SessionId != convoSession))
        {
            return _ambientThread = new SmoothAgentThread(_client, convoSession, convoSession, _options.AgentId);
        }

        if (_ambientThread is not null) return _ambientThread;

        await _threadGate.WaitAsync(cancellationToken).ConfigureAwait(false);
        try
        {
            if (_ambientThread is not null) return _ambientThread;
            if (string.IsNullOrEmpty(_options.AgentId))
                throw new InvalidOperationException(
                    "SmoothAgentChatClient cannot create a session: SmoothAgentOptions.AgentId is not set and no thread was supplied.");

            _ambientThread = await SmoothAgentThread.CreateAsync(
                _client, _options.AgentId, _options.UserName, _options.UserEmail, cancellationToken).ConfigureAwait(false);
            return _ambientThread;
        }
        finally
        {
            _threadGate.Release();
        }
    }

    private static ChatResponseUpdate TextUpdate(string text, SmoothAgentThread thread, string? responseId)
        => new(ChatRole.Assistant, text)
        {
            ConversationId = thread.ConversationId,
            ResponseId = responseId,
            MessageId = responseId,
        };

    /// <summary>The most recent user message text, joined across its content parts.</summary>
    private static string LastUserText(IEnumerable<ChatMessage> messages)
    {
        ArgumentNullException.ThrowIfNull(messages);
        ChatMessage? lastUser = null;
        ChatMessage? lastAny = null;
        foreach (var m in messages)
        {
            lastAny = m;
            if (m.Role == ChatRole.User) lastUser = m;
        }
        var chosen = lastUser ?? lastAny
            ?? throw new ArgumentException("No messages supplied to send.", nameof(messages));
        return chosen.Text ?? string.Empty;
    }

    /// <summary>
    /// Extract the assistant reply text from a terminal <c>eventual_response</c>. The
    /// payload puts the reply in <c>data.data.response</c> as either a bare string or
    /// <c>{ responseParts: [...] }</c> (mirrors the LiveE2E extraction).
    /// </summary>
    internal static string ExtractText(EventualResponseEvent eventual)
    {
        if (eventual.Data.Payload.Response is not { } resp)
            return string.Empty;

        if (resp.ValueKind == JsonValueKind.String)
            return resp.GetString() ?? string.Empty;

        if (resp.ValueKind == JsonValueKind.Object &&
            resp.TryGetProperty("responseParts", out var parts) &&
            parts.ValueKind == JsonValueKind.Array)
        {
            return string.Join(" ",
                parts.EnumerateArray()
                    .Where(p => p.ValueKind == JsonValueKind.String)
                    .Select(p => p.GetString()));
        }

        return resp.ToString();
    }
}
