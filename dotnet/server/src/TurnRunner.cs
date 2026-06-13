using System.Text;
using System.Text.Json.Nodes;
using Microsoft.Extensions.AI;
using SmooAI.SmoothOperator.Core;

namespace SmooAI.SmoothOperator.Server;

/// <summary>What a completed turn produced (the analog of the Rust <c>TurnResult</c>).</summary>
public sealed record TurnResult(string Reply, string MessageId, IReadOnlyList<JsonObject> Citations);

/// <summary>
/// Drives one <c>send_message</c> turn: load prior history, retrieve grounding knowledge, run the
/// C# engine (<see cref="SmoothAgent"/>) streaming, emit <c>stream_token</c> events, persist the
/// reply, and return the citations. The C# analog of the Rust server's <c>run_streaming_turn</c>.
/// (ACL-filtered retrieval, the rerank stage, and tool/HITL stream_chunks arrive in later phases.)
/// </summary>
public sealed class TurnRunner
{
    private const int AutoContextLimit = 3;
    private const int MaxPriorMessages = 50;
    private const int CitationSnippetMaxChars = 280;

    private readonly IChatClient _chatClient;
    private readonly ISessionStore _store;
    private readonly IKnowledgeBase? _knowledge;
    private readonly string _systemPrompt;

    public TurnRunner(IChatClient chatClient, ISessionStore store, IKnowledgeBase? knowledge = null, string? systemPrompt = null)
    {
        _chatClient = chatClient ?? throw new ArgumentNullException(nameof(chatClient));
        _store = store ?? throw new ArgumentNullException(nameof(store));
        _knowledge = knowledge;
        _systemPrompt = systemPrompt ??
            "You are a helpful customer support agent. Answer using only the knowledge provided to you; if it is not there, say you don't know.";
    }

    public async Task<TurnResult> RunAsync(string conversationId, string requestId, string userMessage, Action<JsonObject> sink, CancellationToken cancellationToken = default)
    {
        // 1. Auto-context citations (what grounded the answer). Mirrors the Rust auto_sources.
        var citations = new List<JsonObject>();
        if (_knowledge is not null)
        {
            var hits = await _knowledge.QueryAsync(userMessage, AutoContextLimit, cancellationToken).ConfigureAwait(false);
            foreach (var hit in hits)
            {
                var url = hit.Source.StartsWith("http://", StringComparison.Ordinal) || hit.Source.StartsWith("https://", StringComparison.Ordinal) ? hit.Source : null;
                citations.Add(ProtocolEvents.Citation(hit.DocumentId, hit.Source, url, Truncate(hit.Chunk, CitationSnippetMaxChars), hit.Score));
            }
        }

        // 2. Build the agent + replay prior history as memory (before persisting this turn's inbound).
        var agent = new SmoothAgent(_chatClient, new AgentOptions { Instructions = _systemPrompt, Knowledge = _knowledge });
        var thread = agent.GetNewThread();
        foreach (var message in _store.ListMessages(conversationId, MaxPriorMessages))
        {
            var role = message.Direction == MessageDirection.Outbound ? ChatRole.Assistant : ChatRole.User;
            thread.Add(new ChatMessage(role, message.Text));
        }

        // 3. Persist the inbound user message.
        _store.AppendMessage(conversationId, MessageDirection.Inbound, userMessage);

        // 4. Stream the turn, emitting a stream_token per delta.
        var reply = new StringBuilder();
        await foreach (var update in agent.RunStreamingAsync(userMessage, thread, cancellationToken).ConfigureAwait(false))
        {
            var text = update.Text;
            if (!string.IsNullOrEmpty(text))
            {
                reply.Append(text);
                sink(ProtocolEvents.StreamToken(requestId, text));
            }
        }

        // 5. Persist the outbound reply and return.
        var outbound = _store.AppendMessage(conversationId, MessageDirection.Outbound, reply.ToString());
        return new TurnResult(reply.ToString(), outbound.Id, citations);
    }

    private static string Truncate(string value, int max) => value.Length <= max ? value : value[..max];
}
