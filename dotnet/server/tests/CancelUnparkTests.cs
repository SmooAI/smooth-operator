using System.Runtime.CompilerServices;
using System.Text.Json.Nodes;
using Microsoft.Extensions.AI;
using SmooAI.SmoothOperator.Core;
using SmooAI.SmoothOperator.Server;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// Parity coverage for cancelling a turn that is PARKED at a write-confirmation (HITL). The park
/// awaits a bare <see cref="TaskCompletionSource{TResult}"/> the per-turn CTS cannot complete, so the
/// cancel path must ALSO discard the session's pending confirmation — otherwise the parked task
/// lingers until the next Register/disconnect evicts it. Mirrors the Rust reference dropping the
/// confirmation future when the turn's task handle is aborted. th cancel-unpark.
/// </summary>
public class CancelUnparkTests
{
    /// <summary>A scripted streaming client: turn 1 emits a tool call to a confirm-gated tool (parks),
    /// any later turn falls back to an empty assistant reply.</summary>
    private sealed class ToolCallingChatClient : IChatClient
    {
        private readonly Queue<ChatResponse> _responses = new();

        public ToolCallingChatClient PushToolCall(string callId, string name, IDictionary<string, object?> arguments)
        {
            _responses.Enqueue(new ChatResponse(new ChatMessage(ChatRole.Assistant, new List<AIContent> { new FunctionCallContent(callId, name, arguments) })) { ModelId = "mock-model" });
            return this;
        }

        public ToolCallingChatClient PushText(string text)
        {
            _responses.Enqueue(new ChatResponse(new ChatMessage(ChatRole.Assistant, text)) { ModelId = "mock-model" });
            return this;
        }

        private ChatResponse Next() =>
            _responses.Count > 0 ? _responses.Dequeue() : new ChatResponse(new ChatMessage(ChatRole.Assistant, string.Empty)) { ModelId = "mock-model" };

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

    private static string? Type(JsonObject ev) => ev["type"]?.GetValue<string>();
    private static string? ReqId(JsonObject ev) => ev["requestId"]?.GetValue<string>();

    [Fact]
    public async Task CancellingAParkedConfirmation_DiscardsPendingConfirmation_AndDropsTheTurnCleanly()
    {
        const string Tool = "delete_record";
        var chat = new ToolCallingChatClient()
            .PushToolCall("call_1", Tool, new Dictionary<string, object?> { ["id"] = "row-42" })
            // Never legitimately consumed by turn 1 (it aborts before a second model call). Present so a
            // subsequent turn always has a response to complete with.
            .PushText("done");

        var store = new InMemorySessionStore();
        var confirmations = new ConfirmationRegistry();
        var tools = new AITool[]
        {
            AIFunctionFactory.Create((string id) => $"deleted {id}", Tool, "Delete a record."),
        };
        var dispatcher = new FrameDispatcher(store, chat, tools: tools, confirmTools: new[] { Tool }, confirmations: confirmations);

        // Thread-safe event capture: the turn runs on a background task, so the sink is invoked off the
        // test thread. `parked` fires the instant the turn emits write_confirmation_required.
        var gate = new object();
        var events = new List<JsonObject>();
        var parked = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        void Sink(JsonObject ev)
        {
            lock (gate)
            {
                events.Add(ev);
            }
            if (Type(ev) == "write_confirmation_required")
            {
                parked.TrySetResult();
            }
        }

        // Establish a session, then send a message that drives the turn into a confirm-gated tool call.
        await dispatcher.DispatchAsync("""{"action":"create_conversation_session","requestId":"cs","agentId":"11111111-1111-1111-1111-111111111111"}""", Sink);
        string sessionId;
        lock (gate)
        {
            sessionId = events[0]["data"]!["sessionId"]!.GetValue<string>();
            events.Clear();
        }

        await dispatcher.DispatchAsync($$"""{"action":"send_message","requestId":"turn1","sessionId":"{{sessionId}}","message":"delete row 42","stream":true}""", Sink);

        // The turn parks at the write-confirmation.
        await parked.Task.WaitAsync(TimeSpan.FromSeconds(10));
        lock (gate)
        {
            Assert.Contains(events, e => Type(e) == "write_confirmation_required" && ReqId(e) == "turn1");
        }

        // Cancel the parked turn. `cancelled` is emitted; the pending confirmation is discarded so the
        // parked task unblocks (denied) instead of lingering.
        await dispatcher.DispatchAsync("""{"action":"cancel","requestId":"c1"}""", Sink);
        lock (gate)
        {
            Assert.Contains(events, e => Type(e) == "cancelled" && ReqId(e) == "turn1");
        }

        // The pending confirmation is GONE — a confirm_tool_action for the session now finds nothing to
        // resolve (no zombie parked turn to approve). Both the wire path and the registry agree.
        await dispatcher.DispatchAsync($$"""{"action":"confirm_tool_action","requestId":"cf1","sessionId":"{{sessionId}}","approved":true}""", Sink);
        lock (gate)
        {
            var confirmReply = events.Single(e => ReqId(e) == "cf1");
            Assert.Equal("error", Type(confirmReply));
            Assert.Equal("NO_PENDING_CONFIRMATION", confirmReply["error"]!["code"]!.GetValue<string>());
        }
        Assert.False(confirmations.Resolve(sessionId, approved: true), "no confirmation should remain registered after cancel");

        // The slot is freed: a NEW send_message is accepted (202 ack), not rejected TURN_IN_PROGRESS.
        await dispatcher.DispatchAsync($$"""{"action":"send_message","requestId":"turn2","sessionId":"{{sessionId}}","message":"hello again","stream":true}""", Sink);
        await dispatcher.WaitForTurnsAsync();
        lock (gate)
        {
            var turn2Ack = events.First(e => ReqId(e) == "turn2");
            Assert.Equal("immediate_response", Type(turn2Ack));
            Assert.Equal(202, turn2Ack["status"]!.GetValue<int>());
            Assert.DoesNotContain(events, e => Type(e) == "error" && ReqId(e) == "turn2");

            // No stray events leaked from the abandoned parked turn. Its events are only the ack, the
            // park prompt (write_confirmation_required + the deferred gated-tool stream_chunk, both
            // emitted synchronously at park time), and the terminal cancelled. Critically, the turn NEVER
            // rides out an eventual_response or an error after cancellation — the gagged sink guarantees
            // it — and cancelled is both terminal and singular.
            var turn1Events = events.Where(e => ReqId(e) == "turn1").ToList();
            Assert.DoesNotContain(turn1Events, e => Type(e) == "eventual_response");
            Assert.DoesNotContain(turn1Events, e => Type(e) == "error");
            Assert.Equal("cancelled", Type(turn1Events[^1]));
            Assert.Single(turn1Events, e => Type(e) == "cancelled");
        }
    }
}
