using System.Runtime.CompilerServices;
using System.Text.Json.Nodes;
using Microsoft.Extensions.AI;
using SmooAI.SmoothOperator.Core;
using SmooAI.SmoothOperator.Server;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// A turn parked at a write-confirmation must not wait forever. A client that closes the tab WITHOUT
/// closing the socket never triggers teardown, so <see cref="ConfirmationRegistry.RejectAll"/> never
/// runs — and the parked turn holds the connection's single turn slot indefinitely: every later
/// <c>send_message</c> is refused <c>TURN_IN_PROGRESS</c> and the graceful drain hangs. The Rust
/// reference passes <c>CONFIRMATION_TIMEOUT</c> (300s) into its <c>ConfirmationHook</c> for exactly this
/// reason; the interaction park already had the same backstop. th-acf8ea.
/// </summary>
public class ConfirmationTimeoutTests
{
    /// <summary>A scripted streaming client: turn 1 calls a confirm-gated tool (parks); every later
    /// response is plain text so the turn (and any subsequent one) settles.</summary>
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

    /// <summary>
    /// Park a turn on a write-confirmation, then never answer. The gate's backstop must deny the tool,
    /// let the turn finish, and free the slot.
    ///
    /// <para>Every assertion is on OUTCOME (the turn settled / the slot accepts a new message / the
    /// registration is gone), never on elapsed wall-clock — this box runs many agents at once and
    /// timing assertions there are noise. The backstop is injected short so the test doesn't sleep;
    /// without the fix <see cref="FrameDispatcher.WaitForTurnsAsync"/> never returns and the bounded
    /// wait below fails the test.</para>
    /// </summary>
    [Fact]
    public async Task UnansweredConfirmation_TimesOut_DeniesTheTool_AndFreesTheTurnSlot()
    {
        const string Tool = "delete_record";
        var chat = new ToolCallingChatClient()
            .PushToolCall("call_1", Tool, new Dictionary<string, object?> { ["id"] = "row-42" })
            .PushText("I did not delete anything.")
            .PushText("hello back");

        var store = new InMemorySessionStore();
        var confirmations = new ConfirmationRegistry();
        var tools = new AITool[]
        {
            AIFunctionFactory.Create((string id) => $"deleted {id}", Tool, "Delete a record."),
        };
        var dispatcher = new FrameDispatcher(store, chat, tools: tools, confirmTools: new[] { Tool }, confirmations: confirmations)
        {
            ConfirmationTimeout = TimeSpan.FromMilliseconds(150),
        };

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

        await dispatcher.DispatchAsync("""{"action":"create_conversation_session","requestId":"cs","agentId":"11111111-1111-1111-1111-111111111111"}""", Sink);
        string sessionId;
        lock (gate)
        {
            sessionId = events[0]["data"]!["sessionId"]!.GetValue<string>();
            events.Clear();
        }

        await dispatcher.DispatchAsync($$"""{"action":"send_message","requestId":"turn1","sessionId":"{{sessionId}}","message":"delete row 42","stream":true}""", Sink);
        await parked.Task.WaitAsync(TimeSpan.FromSeconds(10));

        // NOBODY ever sends confirm_tool_action — the tab is gone but the socket is still open, so no
        // teardown runs. The turn must still finish, on the gate's backstop alone.
        await dispatcher.WaitForTurnsAsync().WaitAsync(TimeSpan.FromSeconds(30));

        lock (gate)
        {
            var turn1 = events.Where(e => ReqId(e) == "turn1").ToList();
            Assert.Equal("eventual_response", Type(turn1[^1]));
            Assert.DoesNotContain(turn1, e => Type(e) == "error");
        }

        // Fails closed: the registration was taken out (denied), so a late confirm_tool_action finds
        // nothing to resolve rather than approving a write nobody is waiting on.
        Assert.False(confirmations.Resolve(sessionId, approved: true), "the timed-out confirmation must not still be registered");
        await dispatcher.DispatchAsync($$"""{"action":"confirm_tool_action","requestId":"cf1","sessionId":"{{sessionId}}","approved":true}""", Sink);
        lock (gate)
        {
            var reply = events.Single(e => ReqId(e) == "cf1");
            Assert.Equal("error", Type(reply));
            Assert.Equal("NO_PENDING_CONFIRMATION", reply["error"]!["code"]!.GetValue<string>());
        }

        // The slot is FREE: a new send_message is accepted (202), not refused TURN_IN_PROGRESS.
        await dispatcher.DispatchAsync($$"""{"action":"send_message","requestId":"turn2","sessionId":"{{sessionId}}","message":"hello again","stream":true}""", Sink);
        await dispatcher.WaitForTurnsAsync().WaitAsync(TimeSpan.FromSeconds(30));
        lock (gate)
        {
            var ack = events.First(e => ReqId(e) == "turn2");
            Assert.Equal("immediate_response", Type(ack));
            Assert.Equal(202, ack["status"]!.GetValue<int>());
            Assert.DoesNotContain(events, e => Type(e) == "error" && ReqId(e) == "turn2");
        }
    }

    /// <summary>
    /// The backstop must not steal a verdict that arrives in time: an approval still runs the tool and
    /// the turn completes normally. (Guards against "fix the hang by denying everything".)
    /// </summary>
    [Fact]
    public async Task ConfirmationAnsweredInTime_StillApproves()
    {
        const string Tool = "delete_record";
        var chat = new ToolCallingChatClient()
            .PushToolCall("call_1", Tool, new Dictionary<string, object?> { ["id"] = "row-42" })
            .PushText("Deleted.");

        var store = new InMemorySessionStore();
        var confirmations = new ConfirmationRegistry();
        var ran = false;
        var tools = new AITool[]
        {
            AIFunctionFactory.Create((string id) => { ran = true; return $"deleted {id}"; }, Tool, "Delete a record."),
        };
        // Long enough that the deadline is never the reason this turn settles.
        var dispatcher = new FrameDispatcher(store, chat, tools: tools, confirmTools: new[] { Tool }, confirmations: confirmations)
        {
            ConfirmationTimeout = TimeSpan.FromSeconds(60),
        };

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

        await dispatcher.DispatchAsync("""{"action":"create_conversation_session","requestId":"cs","agentId":"11111111-1111-1111-1111-111111111111"}""", Sink);
        string sessionId;
        lock (gate)
        {
            sessionId = events[0]["data"]!["sessionId"]!.GetValue<string>();
        }

        await dispatcher.DispatchAsync($$"""{"action":"send_message","requestId":"turn1","sessionId":"{{sessionId}}","message":"delete row 42","stream":true}""", Sink);
        await parked.Task.WaitAsync(TimeSpan.FromSeconds(10));

        await dispatcher.DispatchAsync($$"""{"action":"confirm_tool_action","requestId":"cf1","sessionId":"{{sessionId}}","approved":true}""", Sink);
        await dispatcher.WaitForTurnsAsync().WaitAsync(TimeSpan.FromSeconds(30));

        Assert.True(ran, "an approved tool must still run");
        lock (gate)
        {
            Assert.Contains(events, e => Type(e) == "eventual_response" && ReqId(e) == "turn1");
        }
    }
}
