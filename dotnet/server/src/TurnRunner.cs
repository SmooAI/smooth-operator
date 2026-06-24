using System.Text;
using System.Text.Json;
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
    private const int RerankCandidatePool = 15; // fetched before the reranker trims to AutoContextLimit
    private const int MaxPriorMessages = 50;
    private const int CitationSnippetMaxChars = 280;

    private readonly IChatClient _chatClient;
    private readonly ISessionStore _store;
    private readonly IKnowledgeBase? _knowledge;
    private readonly IReranker? _reranker;
    private readonly string _systemPrompt;
    private readonly IReadOnlyList<AITool> _tools;

    public TurnRunner(IChatClient chatClient, ISessionStore store, IKnowledgeBase? knowledge = null, string? systemPrompt = null, IReranker? reranker = null, IReadOnlyList<AITool>? tools = null)
    {
        _chatClient = chatClient ?? throw new ArgumentNullException(nameof(chatClient));
        _store = store ?? throw new ArgumentNullException(nameof(store));
        _knowledge = knowledge;
        _reranker = reranker;
        _systemPrompt = systemPrompt ??
            "You are a helpful customer support agent. Answer using only the knowledge provided to you; if it is not there, say you don't know.";
        _tools = tools ?? Array.Empty<AITool>();
    }

    public async Task<TurnResult> RunAsync(string conversationId, string requestId, string userMessage, Action<JsonObject> sink, CancellationToken cancellationToken = default)
    {
        // 1. Auto-context citations (what grounded the answer). Mirrors the Rust auto_sources.
        //    With a reranker configured, fetch a wider candidate pool and let it reorder down to
        //    the top few before they become citations; without one, fetch exactly the top few
        //    (behavior unchanged — the rerank stage is opt-in).
        var citations = new List<JsonObject>();
        if (_knowledge is not null)
        {
            var fetchLimit = _reranker is not null ? RerankCandidatePool : AutoContextLimit;
            var candidates = await _knowledge.QueryAsync(userMessage, fetchLimit, cancellationToken).ConfigureAwait(false);
            IReadOnlyList<KnowledgeResult> hits;
            try
            {
                hits = await Rerankers.ApplyOptionalAsync(_reranker, userMessage, candidates, AutoContextLimit, cancellationToken).ConfigureAwait(false);
            }
            catch (Exception ex) when (ex is not OperationCanceledException)
            {
                // The reranker is an opt-in retrieval-QUALITY stage (the GatewayReranker hits the
                // network) — a transient failure there must not deny the user an answer. Fall back
                // to the upstream retrieval order, truncated. Cancellation still propagates.
                hits = candidates.Take(AutoContextLimit).ToArray();
            }
            foreach (var hit in hits)
            {
                var url = hit.Source.StartsWith("http://", StringComparison.Ordinal) || hit.Source.StartsWith("https://", StringComparison.Ordinal) ? hit.Source : null;
                citations.Add(ProtocolEvents.Citation(hit.DocumentId, hit.Source, url, Truncate(hit.Chunk, CitationSnippetMaxChars), hit.Score));
            }
        }

        // 2. Build the agent + replay prior history as memory (before persisting this turn's inbound).
        //    Registered tools (default none) are passed straight to the engine's agentic loop; the
        //    streaming block below already translates the resulting tool-call/result events into
        //    stream_chunks, so enabling tools is purely a matter of supplying them here.
        var options = new AgentOptions { Instructions = _systemPrompt, Knowledge = _knowledge };
        foreach (var tool in _tools)
        {
            options.Tools.Add(tool);
        }
        var agent = new SmoothAgent(_chatClient, options);
        var thread = agent.GetNewThread();
        foreach (var message in await _store.ListMessagesAsync(conversationId, MaxPriorMessages, cancellationToken).ConfigureAwait(false))
        {
            var role = message.Direction == MessageDirection.Outbound ? ChatRole.Assistant : ChatRole.User;
            thread.Add(new ChatMessage(role, message.Text));
        }

        // 3. Persist the inbound user message.
        await _store.AppendMessageAsync(conversationId, MessageDirection.Inbound, userMessage, cancellationToken).ConfigureAwait(false);

        // 4. Stream the turn: a stream_token per text delta, and a stream_chunk per tool call /
        //    tool result (mirrors the Rust runner translating ToolCallStart/Complete events). Tool
        //    calls are deduped by callId (streaming can fragment them); results are labeled by
        //    looking the tool name back up from the call.
        var reply = new StringBuilder();
        var toolNames = new Dictionary<string, string>();
        var emittedCalls = new HashSet<string>();
        await foreach (var update in agent.RunStreamingAsync(userMessage, thread, cancellationToken).ConfigureAwait(false))
        {
            var text = update.Text;
            if (!string.IsNullOrEmpty(text))
            {
                reply.Append(text);
                sink(ProtocolEvents.StreamToken(requestId, text));
            }

            foreach (var content in update.Contents)
            {
                switch (content)
                {
                    case FunctionCallContent call when emittedCalls.Add(call.CallId):
                        toolNames[call.CallId] = call.Name;
                        sink(ProtocolEvents.StreamChunk(requestId, call.Name, ToolCallState(call)));
                        break;
                    case FunctionResultContent result:
                        var name = toolNames.TryGetValue(result.CallId, out var resolved) ? resolved : "tool";
                        sink(ProtocolEvents.StreamChunk(requestId, name, ToolResultState(name, result)));
                        break;
                }
            }
        }

        // 5. Persist the outbound reply and return.
        var outbound = await _store.AppendMessageAsync(conversationId, MessageDirection.Outbound, reply.ToString(), cancellationToken).ConfigureAwait(false);
        return new TurnResult(reply.ToString(), outbound.Id, citations);
    }

    private static string Truncate(string value, int max) => value.Length <= max ? value : value[..max];

    private static JsonObject ToolCallState(FunctionCallContent call) => new()
    {
        ["rawResponse"] = new JsonObject
        {
            ["toolCall"] = new JsonObject
            {
                ["name"] = call.Name,
                ["arguments"] = call.Arguments is null ? new JsonObject() : JsonSerializer.SerializeToNode(call.Arguments),
            },
        },
    };

    private static JsonObject ToolResultState(string name, FunctionResultContent result)
    {
        var resultText = result.Result?.ToString() ?? string.Empty;
        // The engine folds tool failures into the result string (see InvokeToolAsync); detect that
        // convention so the chunk's isError flag matches the Rust ToolCallComplete signal.
        var isError = resultText.StartsWith("Error:", StringComparison.Ordinal) || resultText.StartsWith("Denied by human:", StringComparison.Ordinal);
        return new JsonObject
        {
            ["rawResponse"] = new JsonObject
            {
                ["toolResult"] = new JsonObject { ["name"] = name, ["isError"] = isError, ["result"] = resultText },
            },
        };
    }
}
