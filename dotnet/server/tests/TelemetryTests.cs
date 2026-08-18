using System.Diagnostics;
using System.Runtime.CompilerServices;
using Microsoft.Extensions.AI;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// Telemetry coverage for the .NET server's streaming turn path (<see cref="TurnRunner.RunAsync(string,string,string,Action{System.Text.Json.Nodes.JsonObject},string,System.Threading.CancellationToken,IReadOnlyList{UserImage},IReadOnlyList{UserFile},string)"/>).
/// The C# analog of <c>rust/smooth-operator-server/tests/telemetry.rs</c>: via an in-memory
/// <see cref="ActivityListener"/> (no live OTLP collector) it asserts that a real turn emits
///
/// 1. a <c>gen_ai.chat</c> turn span carrying <c>gen_ai.system</c>, <c>gen_ai.request.model</c>,
///    <c>gen_ai.conversation.id</c>, <c>gen_ai.agent.name</c>, and the usage token counts, and
/// 2. a child <c>gen_ai.tool</c> span carrying <c>gen_ai.tool.name</c> and the (redacted)
///    <c>gen_ai.tool.call.arguments</c> the model passed.
/// </summary>
public class TelemetryTests
{
    private const string Tool = "knowledge_search";

    /// <summary>A scripted streaming <see cref="IChatClient"/>: turn 1 emits a tool call (with args),
    /// turn 2 emits the final answer with usage.</summary>
    private sealed class ScriptedChatClient : IChatClient
    {
        private readonly Queue<ChatResponse> _responses = new();

        public ScriptedChatClient PushToolCall(string callId, string name, IDictionary<string, object?> arguments)
        {
            _responses.Enqueue(new ChatResponse(new ChatMessage(ChatRole.Assistant, new List<AIContent> { new FunctionCallContent(callId, name, arguments) })) { ModelId = "mock-model" });
            return this;
        }

        public ScriptedChatClient PushText(string text)
        {
            _responses.Enqueue(new ChatResponse(new ChatMessage(ChatRole.Assistant, text))
            {
                Usage = new UsageDetails { InputTokenCount = 10, OutputTokenCount = 5, TotalTokenCount = 15 },
                ModelId = "mock-model",
            });
            return this;
        }

        private ChatResponse Next() =>
            _responses.Count > 0 ? _responses.Dequeue() : new ChatResponse(new ChatMessage(ChatRole.Assistant, string.Empty));

        public Task<ChatResponse> GetResponseAsync(IEnumerable<ChatMessage> messages, ChatOptions? options = null, CancellationToken cancellationToken = default) =>
            Task.FromResult(Next());

        public async IAsyncEnumerable<ChatResponseUpdate> GetStreamingResponseAsync(
            IEnumerable<ChatMessage> messages, ChatOptions? options = null, [EnumeratorCancellation] CancellationToken cancellationToken = default)
        {
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

    [Fact]
    public async Task StreamingTurnEmitsGenAiSpansWithModelAndToolArgs()
    {
        var captured = new List<Activity>();
        using var listener = new ActivityListener
        {
            ShouldListenTo = source => source.Name == Telemetry.ActivitySourceName,
            Sample = (ref ActivityCreationOptions<ActivityContext> _) => ActivitySamplingResult.AllData,
            ActivityStopped = captured.Add,
        };
        ActivitySource.AddActivityListener(listener);

        var priorModel = Environment.GetEnvironmentVariable("SMOOTH_AGENT_MODEL");
        Environment.SetEnvironmentVariable("SMOOTH_AGENT_MODEL", "openai/gpt-4o");
        var conversationId = string.Empty;
        try
        {
            var chat = new ScriptedChatClient()
                .PushToolCall("call_kb_1", Tool, new Dictionary<string, object?> { ["query"] = "return policy refund window" })
                .PushText("Items are accepted within 30 days for a full refund.");

            var store = new InMemorySessionStore();
            var session = await store.CreateSessionAsync("agent-1", null, null);
            conversationId = session.ConversationId;

            var tools = new AITool[]
            {
                AIFunctionFactory.Create((string query) => "Returns are accepted within 30 days.", Tool, "Search the knowledge base."),
            };
            // Distinct (dead) preamble client so a parallel test toggling SMOOTH_AGENT_PREAMBLE_MODEL
            // can never steal a response off the scripted main queue.
            var runner = new TurnRunner(chat, store, tools: tools, preambleChatClient: new ScriptedChatClient());

            await runner.RunAsync(conversationId, "req-otel", "what is the return policy?", _ => { });
        }
        finally
        {
            Environment.SetEnvironmentVariable("SMOOTH_AGENT_MODEL", priorModel);
        }

        // (1) The turn span carries system, model, conversation, agent, and usage.
        var chatSpan = captured.SingleOrDefault(a => a.OperationName == Telemetry.SpanChat);
        Assert.NotNull(chatSpan);
        Assert.Equal(Telemetry.SystemName, chatSpan!.GetTagItem(Telemetry.GenAiSystem));
        Assert.Equal("openai/gpt-4o", chatSpan.GetTagItem(Telemetry.GenAiRequestModel));
        Assert.Equal(conversationId, chatSpan.GetTagItem(Telemetry.GenAiConversationId));
        Assert.Equal(Telemetry.AgentName, chatSpan.GetTagItem(Telemetry.GenAiAgentName));
        Assert.Equal(10L, Convert.ToInt64(chatSpan.GetTagItem(Telemetry.GenAiUsageInputTokens)));
        Assert.Equal(5L, Convert.ToInt64(chatSpan.GetTagItem(Telemetry.GenAiUsageOutputTokens)));

        // (2) A child tool span carries the tool name + the model's (redacted) arguments, nested under
        //     the turn span.
        var toolSpan = captured.SingleOrDefault(a => a.OperationName == Telemetry.SpanTool);
        Assert.NotNull(toolSpan);
        Assert.Equal(Tool, toolSpan!.GetTagItem(Telemetry.GenAiToolName));
        var args = toolSpan.GetTagItem(Telemetry.GenAiToolArguments) as string ?? string.Empty;
        Assert.Contains("return policy refund window", args);
        Assert.Equal(chatSpan.Id, toolSpan.ParentId);

        // Being a child is NOT enough. The OTLP ingest builds a span's attributes from
        // the resource attrs plus THAT span's own, with no parent inheritance, so the
        // tool span repeats the identifiers itself — and without gen_ai.system it fails
        // the ingest's LLM-event gate outright and is discarded, which is what happened
        // to Rust's tool spans for their entire existence.
        Assert.Equal(Telemetry.SystemName, toolSpan.GetTagItem(Telemetry.GenAiSystem));
        Assert.Equal(Telemetry.OperationTool, toolSpan.GetTagItem(Telemetry.GenAiOperationName));
        Assert.Equal(conversationId, toolSpan.GetTagItem(Telemetry.GenAiConversationId));

        // Must be exactly "chat"/"tool" — the ingest takes the attribute verbatim when
        // present and its queries filter on operation_name = 'tool'.
        Assert.Equal(Telemetry.OperationChat, chatSpan.GetTagItem(Telemetry.GenAiOperationName));

        // Cost: exactly one of the two is ever set. This scripted turn is unpriced, so
        // the marker must be there INSTEAD of a $0.00 — a missing price must never read
        // as free. Before this, .NET was the one engine that shipped a literal
        // `new TurnUsage(0, ...)` on the fallback path.
        Assert.Null(chatSpan.GetTagItem(Telemetry.GenAiUsageCostUsd));
        Assert.Equal(Telemetry.CostUnavailableUnpriced, chatSpan.GetTagItem(Telemetry.CostUnavailable));
    }

    [Fact]
    public void RedactToolArgumentsScrubsSecretNamedKeysAndCapsLength()
    {
        var outText = Telemetry.RedactToolArguments("{\"query\":\"weather\",\"api_key\":\"sk-live-123\",\"nested\":{\"authToken\":\"abc\"}}");
        Assert.Contains("\"query\":\"weather\"", outText);
        Assert.DoesNotContain("sk-live-123", outText);
        Assert.DoesNotContain("abc", outText);
        Assert.Equal(2, System.Text.RegularExpressions.Regex.Matches(outText, "\\[REDACTED\\]").Count);

        Assert.Equal("not json", Telemetry.RedactToolArguments("not json"));
        var capped = Telemetry.RedactToolArguments(new string('x', 2148));
        Assert.True(capped.Length <= 2049, $"capped near max: {capped.Length}");
        Assert.EndsWith("…", capped);
    }
}
