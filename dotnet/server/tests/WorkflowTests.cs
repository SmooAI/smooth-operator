using System.Runtime.CompilerServices;
using System.Text.Json.Nodes;
using Microsoft.Extensions.AI;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// Per-agent config + conversation-workflow behavior: tolerant jsonb parsing, pure step/prompt
/// rendering, judge advancement paths, per-agent prompt assembly + isolation, and malformed-config
/// tolerance. The C# mirror of the monorepo SMOODEV-590 workflow behavior.
/// </summary>
public class WorkflowTests
{
    // ── AgentConfig.ParseInstructions — tolerant ─────────────────────────────

    [Fact]
    public void ParseInstructions_Object_ExtractsPrompt()
    {
        Assert.Equal("Be terse.", AgentConfig.ParseInstructions("""{"prompt":"Be terse."}"""));
    }

    [Fact]
    public void ParseInstructions_BareString_IsAccepted()
    {
        Assert.Equal("Be terse.", AgentConfig.ParseInstructions("\"Be terse.\""));
    }

    [Theory]
    [InlineData(null)]
    [InlineData("")]
    [InlineData("   ")]
    [InlineData("{not json")]
    [InlineData("""{"prompt":""}""")]
    [InlineData("""{"other":"x"}""")]
    [InlineData("42")]
    public void ParseInstructions_MalformedOrEmpty_ReturnsNull(string? json)
    {
        Assert.Null(AgentConfig.ParseInstructions(json));
    }

    // ── AgentConfig.ParseWorkflow — tolerant, schema-bounded ─────────────────

    [Fact]
    public void ParseWorkflow_Valid_RoundTrips()
    {
        var workflow = AgentConfig.ParseWorkflow("""
            {"goal":"Book a demo","steps":[
              {"id":"greet","intent":"Greet","criteria":"Said hi","next":"qualify"},
              {"id":"qualify","intent":"Qualify","criteria":"Got budget"}
            ]}
            """);
        Assert.NotNull(workflow);
        Assert.Equal("Book a demo", workflow!.Goal);
        Assert.Equal(2, workflow.Steps.Count);
        Assert.Equal("qualify", workflow.Steps[0].Next);
        Assert.Null(workflow.Steps[1].Next);
    }

    [Theory]
    [InlineData(null)]
    [InlineData("")]
    [InlineData("not json")]
    [InlineData("[]")] // not an object
    [InlineData("""{"steps":[{"id":"a","intent":"i","criteria":"c"}]}""")] // missing goal
    [InlineData("""{"goal":"g","steps":[]}""")] // empty steps
    [InlineData("""{"goal":"g"}""")] // no steps
    [InlineData("""{"goal":"g","steps":[{"id":"a","intent":"i"}]}""")] // step missing criteria
    [InlineData("""{"goal":"g","steps":[{"intent":"i","criteria":"c"}]}""")] // step missing id
    [InlineData("""{"goal":"g","steps":["nope"]}""")] // step not an object
    public void ParseWorkflow_MalformedOrIncomplete_ReturnsNull(string? json)
    {
        Assert.Null(AgentConfig.ParseWorkflow(json));
    }

    [Fact]
    public void ParseWorkflow_TooManySteps_ReturnsNull()
    {
        var steps = string.Join(",", Enumerable.Range(0, 21).Select(i => $$"""{"id":"s{{i}}","intent":"i","criteria":"c"}"""));
        Assert.Null(AgentConfig.ParseWorkflow($$"""{"goal":"g","steps":[{{steps}}]}"""));
    }

    // ── AgentConfig.ParseToolConfig — authoritative AgentToolConfig shape ─────

    [Fact]
    public void ParseToolConfig_EnabledTools_ParsesEntriesWithDefaults()
    {
        var tools = AgentConfig.ParseToolConfig("""
            {"enabledTools":[
              {"toolId":"knowledge_search"},
              {"toolId":"notify_humans","enabled":false,"authLevel":"oauth","config":{"scope":"x"}}
            ]}
            """);
        Assert.NotNull(tools);
        Assert.Equal(2, tools!.Count);
        Assert.Equal("knowledge_search", tools[0].ToolId);
        Assert.True(tools[0].Enabled); // default true
        Assert.Equal("none", tools[0].AuthLevel); // default none
        Assert.Null(tools[0].Config);
        Assert.False(tools[1].Enabled);
        Assert.Equal("oauth", tools[1].AuthLevel);
        Assert.NotNull(tools[1].Config); // config preserved even if unused
    }

    [Fact]
    public void ParseToolConfig_DropsNonSnakeCaseToolIds_ButKeepsRestrictionActive()
    {
        // camelCase toolId is dropped (SMOODEV-981), but a non-empty enabledTools stays restrictive.
        var tools = AgentConfig.ParseToolConfig("""{"enabledTools":[{"toolId":"knowledgeSearch"}]}""");
        Assert.NotNull(tools);
        Assert.Empty(tools!);
    }

    [Theory]
    [InlineData(null)]
    [InlineData("")]
    [InlineData("not json")]
    [InlineData("[]")] // array, not the object shape
    [InlineData("""{"enabledTools":[]}""")] // empty → no restriction (NOT "no tools")
    [InlineData("""{"other":1}""")] // no enabledTools key
    [InlineData("42")]
    public void ParseToolConfig_MalformedOrEmpty_ReturnsNull(string? json)
    {
        Assert.Null(AgentConfig.ParseToolConfig(json));
    }

    // ── Workflows.ResolveCurrentStep / NextStep ──────────────────────────────

    private static ConversationWorkflow ThreeStep() => new(
        "Goal",
        new[]
        {
            new ConversationWorkflowStep("a", "ia", "ca", "c"), // explicit next skips b
            new ConversationWorkflowStep("b", "ib", "cb", null),
            new ConversationWorkflowStep("c", "ic", "cc", null),
        });

    [Fact]
    public void ResolveCurrentStep_MatchesById()
    {
        Assert.Equal("b", Workflows.ResolveCurrentStep(ThreeStep(), "b")!.Id);
    }

    [Theory]
    [InlineData(null)]
    [InlineData("")]
    [InlineData("unknown-step")]
    public void ResolveCurrentStep_EmptyOrUnknown_ReturnsFirst(string? pointer)
    {
        Assert.Equal("a", Workflows.ResolveCurrentStep(ThreeStep(), pointer)!.Id);
    }

    [Fact]
    public void ResolveCurrentStep_NoSteps_ReturnsNull()
    {
        Assert.Null(Workflows.ResolveCurrentStep(new ConversationWorkflow("g", Array.Empty<ConversationWorkflowStep>()), "a"));
    }

    [Fact]
    public void NextStep_PrefersExplicitNext()
    {
        var wf = ThreeStep();
        Assert.Equal("c", Workflows.NextStep(wf, wf.Steps[0])!.Id); // a.next = c, skipping b
    }

    [Fact]
    public void NextStep_FallsThroughToSequential()
    {
        var wf = ThreeStep();
        Assert.Equal("c", Workflows.NextStep(wf, wf.Steps[1])!.Id); // b has no next → sequential c
    }

    [Fact]
    public void NextStep_TerminalStep_ReturnsNull()
    {
        var wf = ThreeStep();
        Assert.Null(Workflows.NextStep(wf, wf.Steps[2])); // c is last
    }

    [Fact]
    public void NextStep_UnknownExplicitNext_FallsThroughToSequential()
    {
        var wf = new ConversationWorkflow("g", new[]
        {
            new ConversationWorkflowStep("a", "i", "c", "does-not-exist"),
            new ConversationWorkflowStep("b", "i", "c", null),
        });
        Assert.Equal("b", Workflows.NextStep(wf, wf.Steps[0])!.Id);
    }

    // ── Workflows.RenderPromptSection ────────────────────────────────────────

    [Fact]
    public void RenderPromptSection_RendersCurrentStep()
    {
        var section = Workflows.RenderPromptSection(ThreeStep(), "b");
        Assert.Contains("<ConversationWorkflow>", section);
        Assert.Contains("GOAL: Goal", section);
        Assert.Contains("CURRENT STEP (2/3): b", section);
        Assert.Contains("INTENT: ib", section);
        Assert.Contains("CRITERIA: cb", section);
    }

    [Fact]
    public void RenderPromptSection_NoWorkflow_ReturnsEmpty()
    {
        Assert.Equal(string.Empty, Workflows.RenderPromptSection(null, "b"));
    }

    // ── LlmWorkflowJudge.ParseVerdict ────────────────────────────────────────

    [Theory]
    [InlineData("""{"verdict":"yes"}""", WorkflowVerdict.Yes)]
    [InlineData("""{"verdict":"no","reason":"x"}""", WorkflowVerdict.No)]
    [InlineData("""{"verdict":"maybe"}""", WorkflowVerdict.Maybe)]
    [InlineData("""Here you go: {"verdict":"yes"} done""", WorkflowVerdict.Yes)] // fenced/prefixed
    [InlineData("""{"verdict":"YES"}""", WorkflowVerdict.Yes)] // case-insensitive
    public void ParseVerdict_ValidShapes(string text, WorkflowVerdict expected)
    {
        Assert.Equal(expected, LlmWorkflowJudge.ParseVerdict(text));
    }

    [Theory]
    [InlineData(null)]
    [InlineData("")]
    [InlineData("no json here")]
    [InlineData("""{"verdict":"garbage"}""")]
    [InlineData("{not json}")]
    public void ParseVerdict_Unparseable_ReturnsSkipped(string? text)
    {
        Assert.Equal(WorkflowVerdict.Skipped, LlmWorkflowJudge.ParseVerdict(text));
    }

    // ── LlmWorkflowJudge — failure tolerance ─────────────────────────────────

    [Fact]
    public async Task Judge_EmptyReply_ReturnsSkipped_WithoutCallingModel()
    {
        var judge = new LlmWorkflowJudge(new ThrowingChatClient());
        var wf = ThreeStep();
        var verdict = await judge.JudgeAsync(wf, wf.Steps[0], "hi", "");
        Assert.Equal(WorkflowVerdict.Skipped, verdict);
    }

    [Fact]
    public async Task Judge_ModelThrows_ReturnsSkipped()
    {
        var judge = new LlmWorkflowJudge(new ThrowingChatClient());
        var wf = ThreeStep();
        var verdict = await judge.JudgeAsync(wf, wf.Steps[0], "hi", "some reply");
        Assert.Equal(WorkflowVerdict.Skipped, verdict);
    }

    // ── StaticAgentConfigResolver — the delivery seam ───────────────────────

    [Fact]
    public async Task StaticResolver_ReturnsConfigForKnown_NullForUnknown()
    {
        var config = new AgentConfig(InstructionsPrompt: "hi");
        var resolver = new StaticAgentConfigResolver().Set("a", config);

        Assert.Same(config, await resolver.ResolveAsync("a"));
        Assert.Null(await resolver.ResolveAsync("missing"));
        // The empty resolver is the no-op default — every lookup is null (behavior unchanged).
        Assert.Null(await new StaticAgentConfigResolver().ResolveAsync("a"));
    }

    // ── TurnRunner — per-agent prompt assembly + isolation ───────────────────

    [Fact]
    public async Task TurnRunner_UsesPerAgentInstructions_OverDefaultPersona()
    {
        var chat = new CapturingChatClient("Sure!");
        var store = new InMemorySessionStore();
        var session = await store.CreateSessionAsync("agent-1", null, null);
        var config = new AgentConfig(InstructionsPrompt: "You are Ziggy, a pirate concierge.");
        var runner = new TurnRunner(chat, store, agentConfig: config);

        await runner.RunAsync(session.ConversationId, "r1", "hello", _ => { });

        Assert.Contains("Ziggy, a pirate concierge", chat.LastSystemPrompt);
        Assert.DoesNotContain("helpful customer support agent", chat.LastSystemPrompt);
    }

    [Fact]
    public async Task TurnRunner_NoConfig_KeepsDefaultPersona()
    {
        var chat = new CapturingChatClient("Sure!");
        var store = new InMemorySessionStore();
        var session = await store.CreateSessionAsync("agent-1", null, null);
        var runner = new TurnRunner(chat, store);

        await runner.RunAsync(session.ConversationId, "r1", "hello", _ => { });

        Assert.Contains("helpful customer support agent", chat.LastSystemPrompt);
    }

    [Fact]
    public async Task TurnRunner_RendersWorkflowSectionForCurrentStep()
    {
        var chat = new CapturingChatClient("Hi there!");
        var store = new InMemorySessionStore();
        var session = await store.CreateSessionAsync("agent-1", null, null);
        var config = new AgentConfig(InstructionsPrompt: "Base.", Workflow: ThreeStep());
        var runner = new TurnRunner(chat, store, agentConfig: config, judge: new StubJudge(WorkflowVerdict.No));

        await runner.RunAsync(session.ConversationId, "r1", "hello", _ => { });

        Assert.Contains("<ConversationWorkflow>", chat.LastSystemPrompt);
        Assert.Contains("CURRENT STEP (1/3): a", chat.LastSystemPrompt);
    }

    [Fact]
    public async Task TurnRunner_PerAgentIsolation_TwoAgentsDifferentPrompts()
    {
        var resolver = new StaticAgentConfigResolver()
            .Set("agent-1", new AgentConfig(InstructionsPrompt: "I am agent ONE."))
            .Set("agent-2", new AgentConfig(InstructionsPrompt: "I am agent TWO."));
        var store = new InMemorySessionStore();

        var chat1 = new CapturingChatClient("ok");
        var s1 = await store.CreateSessionAsync("agent-1", null, null);
        await new TurnRunner(chat1, store, agentConfig: await resolver.ResolveAsync("agent-1")).RunAsync(s1.ConversationId, "r", "hi", _ => { });

        var chat2 = new CapturingChatClient("ok");
        var s2 = await store.CreateSessionAsync("agent-2", null, null);
        await new TurnRunner(chat2, store, agentConfig: await resolver.ResolveAsync("agent-2")).RunAsync(s2.ConversationId, "r", "hi", _ => { });

        Assert.Contains("agent ONE", chat1.LastSystemPrompt);
        Assert.DoesNotContain("agent TWO", chat1.LastSystemPrompt);
        Assert.Contains("agent TWO", chat2.LastSystemPrompt);
        Assert.DoesNotContain("agent ONE", chat2.LastSystemPrompt);
    }

    [Fact]
    public async Task TurnRunner_MalformedWorkflowConfig_DegradesToDefault()
    {
        // A host store that parsed malformed jsonb returns a config with a null workflow.
        var config = new AgentConfig(InstructionsPrompt: AgentConfig.ParseInstructions("{broken"), Workflow: AgentConfig.ParseWorkflow("{broken"));
        Assert.True(config.IsEmpty);

        var chat = new CapturingChatClient("ok");
        var store = new InMemorySessionStore();
        var session = await store.CreateSessionAsync("agent-1", null, null);
        var runner = new TurnRunner(chat, store, agentConfig: config, judge: new StubJudge(WorkflowVerdict.Yes));

        await runner.RunAsync(session.ConversationId, "r1", "hi", _ => { });

        // Falls back to the default persona, and no workflow step gets persisted.
        Assert.Contains("helpful customer support agent", chat.LastSystemPrompt);
        Assert.DoesNotContain("<ConversationWorkflow>", chat.LastSystemPrompt);
        Assert.Null(await store.GetWorkflowStepAsync(session.ConversationId));
    }

    // ── TurnRunner — judge advancement paths ─────────────────────────────────

    [Fact]
    public async Task TurnRunner_JudgeYes_AdvancesAndPersistsStep()
    {
        var chat = new CapturingChatClient("reply");
        var store = new InMemorySessionStore();
        var session = await store.CreateSessionAsync("agent-1", null, null);
        var config = new AgentConfig(Workflow: ThreeStep());
        var runner = new TurnRunner(chat, store, agentConfig: config, judge: new StubJudge(WorkflowVerdict.Yes));

        await runner.RunAsync(session.ConversationId, "r1", "hi", _ => { });

        // Started on "a" (first step); a.next = "c" (explicit, skipping b).
        Assert.Equal("c", await store.GetWorkflowStepAsync(session.ConversationId));
    }

    [Theory]
    [InlineData(WorkflowVerdict.No)]
    [InlineData(WorkflowVerdict.Maybe)]
    [InlineData(WorkflowVerdict.Skipped)]
    public async Task TurnRunner_JudgeNotYes_StaysOnStep(WorkflowVerdict verdict)
    {
        var chat = new CapturingChatClient("reply");
        var store = new InMemorySessionStore();
        var session = await store.CreateSessionAsync("agent-1", null, null);
        var config = new AgentConfig(Workflow: ThreeStep());
        var runner = new TurnRunner(chat, store, agentConfig: config, judge: new StubJudge(verdict));

        await runner.RunAsync(session.ConversationId, "r1", "hi", _ => { });

        Assert.Equal("a", await store.GetWorkflowStepAsync(session.ConversationId));
    }

    [Fact]
    public async Task TurnRunner_WorkflowResumesFromPersistedStep_AndReachesTerminal()
    {
        var store = new InMemorySessionStore();
        var session = await store.CreateSessionAsync("agent-1", null, null);
        var config = new AgentConfig(Workflow: ThreeStep());
        var judge = new StubJudge(WorkflowVerdict.Yes);

        // Turn 1: a → c (explicit next).
        await new TurnRunner(new CapturingChatClient("x"), store, agentConfig: config, judge: judge)
            .RunAsync(session.ConversationId, "r1", "hi", _ => { });
        Assert.Equal("c", await store.GetWorkflowStepAsync(session.ConversationId));

        // Turn 2: resumes on c (terminal) → stays on c even on "yes".
        await new TurnRunner(new CapturingChatClient("y"), store, agentConfig: config, judge: judge)
            .RunAsync(session.ConversationId, "r2", "next", _ => { });
        Assert.Equal("c", await store.GetWorkflowStepAsync(session.ConversationId));
    }

    [Fact]
    public async Task TurnRunner_WorkflowWithoutJudge_NeverAdvances()
    {
        var store = new InMemorySessionStore();
        var session = await store.CreateSessionAsync("agent-1", null, null);
        var config = new AgentConfig(Workflow: ThreeStep());
        var runner = new TurnRunner(new CapturingChatClient("x"), store, agentConfig: config, judge: null);

        await runner.RunAsync(session.ConversationId, "r1", "hi", _ => { });

        Assert.Null(await store.GetWorkflowStepAsync(session.ConversationId));
    }

    // ── ToolAuthGate.Evaluate — the pure auth-level table ────────────────────

    [Theory]
    [InlineData("none", "public", true, ToolAuthOutcome.Allow)] // no auth required
    [InlineData("admin", "public", false, ToolAuthOutcome.Allow)] // tool doesn't support auth → not gated
    [InlineData("admin", "public", true, ToolAuthOutcome.BlockAdminOnPublic)]
    [InlineData("admin", "internal", true, ToolAuthOutcome.Allow)] // auto-satisfied
    [InlineData("end_user", "internal", true, ToolAuthOutcome.Allow)] // auto-satisfied
    [InlineData("end_user", "public", true, ToolAuthOutcome.ConsultAuthenticator)]
    public void ToolAuthGate_Evaluate(string authLevel, string visibility, bool supportsAuth, ToolAuthOutcome expected)
    {
        Assert.Equal(expected, ToolAuthGate.Evaluate(authLevel, visibility, supportsAuth));
    }

    // ── Greeting — first-turn only ───────────────────────────────────────────

    [Fact]
    public async Task Greeting_InjectedOnFirstTurn_NotOnLater()
    {
        var store = new InMemorySessionStore();
        var session = await store.CreateSessionAsync("agent-1", null, null);
        var config = new AgentConfig(InstructionsPrompt: "Base.", Greeting: "Welcome to Acme!");

        var chat1 = new CapturingChatClient("Hi!");
        await new TurnRunner(chat1, store, agentConfig: config).RunAsync(session.ConversationId, "r1", "hello", _ => { });
        Assert.Contains("<GreetingAwareness>", chat1.LastSystemPrompt);
        Assert.Contains("Welcome to Acme!", chat1.LastSystemPrompt);

        // Turn 2: prior history exists → no greeting re-injected.
        var chat2 = new CapturingChatClient("Sure.");
        await new TurnRunner(chat2, store, agentConfig: config).RunAsync(session.ConversationId, "r2", "another", _ => { });
        Assert.DoesNotContain("<GreetingAwareness>", chat2.LastSystemPrompt);
    }

    [Fact]
    public async Task Greeting_Absent_NoSection()
    {
        var store = new InMemorySessionStore();
        var session = await store.CreateSessionAsync("agent-1", null, null);
        var chat = new CapturingChatClient("Hi!");
        await new TurnRunner(chat, store, agentConfig: new AgentConfig(InstructionsPrompt: "Base.")).RunAsync(session.ConversationId, "r1", "hello", _ => { });
        Assert.DoesNotContain("<GreetingAwareness>", chat.LastSystemPrompt);
    }

    // ── tool_config — the agent's tool allow-list ────────────────────────────

    private static List<AITool> TwoTools() => new()
    {
        AIFunctionFactory.Create(() => "s", "search"),
        AIFunctionFactory.Create(() => "d", "delete_record"),
    };

    private static async Task<CapturingChatClient> RunSendMessage(InMemorySessionStore store, StoredSession session, List<AITool> tools, StaticAgentConfigResolver resolver)
    {
        var chat = new CapturingChatClient("ok");
        var dispatcher = new FrameDispatcher(store, chat, tools: tools, agentConfigResolver: resolver);
        var frame = $$"""{"action":"send_message","requestId":"r","sessionId":"{{session.SessionId}}","message":"hi"}""";
        await dispatcher.DispatchAsync(frame, _ => { });
        await dispatcher.WaitForTurnsAsync();
        return chat;
    }

    private static AgentConfig ToolConfig(params EnabledTool[] tools) => new(EnabledTools: tools);

    [Fact]
    public async Task ToolConfig_RestrictsToEnabledToolIds()
    {
        var store = new InMemorySessionStore();
        var session = await store.CreateSessionAsync("agent-tools", null, null);
        var resolver = new StaticAgentConfigResolver().Set("agent-tools", ToolConfig(
            new EnabledTool("search", true, "none", null),
            new EnabledTool("delete_record", false, "none", null))); // disabled → excluded

        var chat = await RunSendMessage(store, session, TwoTools(), resolver);

        Assert.Equal(new[] { "search" }, chat.LastToolNames);
    }

    [Fact]
    public async Task ToolConfig_Absent_KeepsFullToolSet()
    {
        var store = new InMemorySessionStore();
        var session = await store.CreateSessionAsync("agent-tools", null, null);
        // No resolver entry → null config → no restriction. (Same for empty enabledTools → null.)
        var resolver = new StaticAgentConfigResolver();

        var chat = await RunSendMessage(store, session, TwoTools(), resolver);

        Assert.Equal(new[] { "search", "delete_record" }, chat.LastToolNames);
    }

    [Fact]
    public async Task ToolConfig_UnknownToolIds_Ignored()
    {
        var store = new InMemorySessionStore();
        var session = await store.CreateSessionAsync("agent-tools", null, null);
        var resolver = new StaticAgentConfigResolver().Set("agent-tools", ToolConfig(
            new EnabledTool("search", true, "none", null),
            new EnabledTool("nonexistent", true, "none", null))); // not registered → ignored

        var chat = await RunSendMessage(store, session, TwoTools(), resolver);

        Assert.Equal(new[] { "search" }, chat.LastToolNames);
    }

    [Fact]
    public async Task ToolConfig_AllDisabled_YieldsNoTools()
    {
        var store = new InMemorySessionStore();
        var session = await store.CreateSessionAsync("agent-tools", null, null);
        var resolver = new StaticAgentConfigResolver().Set("agent-tools", ToolConfig(
            new EnabledTool("search", false, "none", null)));

        var chat = await RunSendMessage(store, session, TwoTools(), resolver);

        Assert.Empty(chat.LastToolNames);
    }

    // ── Test doubles ─────────────────────────────────────────────────────────

    /// <summary>Records the last system-role message it was asked to complete, and streams a fixed reply.</summary>
    private sealed class CapturingChatClient : IChatClient
    {
        private readonly string _reply;

        public CapturingChatClient(string reply) => _reply = reply;

        public string LastSystemPrompt { get; private set; } = string.Empty;

        public IReadOnlyList<string> LastToolNames { get; private set; } = Array.Empty<string>();

        private void Capture(IEnumerable<ChatMessage> messages, ChatOptions? options)
        {
            var system = messages.LastOrDefault(m => m.Role == ChatRole.System);
            if (system is not null)
            {
                LastSystemPrompt = system.Text;
            }
            if (options?.Tools is { } tools)
            {
                LastToolNames = tools.Select(t => t.Name).ToList();
            }
        }

        public Task<ChatResponse> GetResponseAsync(IEnumerable<ChatMessage> messages, ChatOptions? options = null, CancellationToken cancellationToken = default)
        {
            Capture(messages, options);
            return Task.FromResult(new ChatResponse(new ChatMessage(ChatRole.Assistant, _reply)) { ModelId = "capture" });
        }

        public async IAsyncEnumerable<ChatResponseUpdate> GetStreamingResponseAsync(
            IEnumerable<ChatMessage> messages, ChatOptions? options = null, [EnumeratorCancellation] CancellationToken cancellationToken = default)
        {
            Capture(messages, options);
            foreach (var update in new ChatResponse(new ChatMessage(ChatRole.Assistant, _reply)).ToChatResponseUpdates())
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

    /// <summary>An <see cref="IChatClient"/> that always throws — proves the judge's failure tolerance.</summary>
    private sealed class ThrowingChatClient : IChatClient
    {
        public Task<ChatResponse> GetResponseAsync(IEnumerable<ChatMessage> messages, ChatOptions? options = null, CancellationToken cancellationToken = default) =>
            throw new InvalidOperationException("model unavailable");

        public IAsyncEnumerable<ChatResponseUpdate> GetStreamingResponseAsync(IEnumerable<ChatMessage> messages, ChatOptions? options = null, CancellationToken cancellationToken = default) =>
            throw new InvalidOperationException("model unavailable");

        public object? GetService(Type serviceType, object? serviceKey = null) => null;

        public void Dispose()
        {
        }
    }

    /// <summary>A judge that returns a fixed verdict — drives the advancement-path tests deterministically.</summary>
    private sealed class StubJudge : IWorkflowJudge
    {
        private readonly WorkflowVerdict _verdict;

        public StubJudge(WorkflowVerdict verdict) => _verdict = verdict;

        public Task<WorkflowVerdict> JudgeAsync(ConversationWorkflow workflow, ConversationWorkflowStep step, string userMessage, string agentReply, CancellationToken cancellationToken = default) =>
            Task.FromResult(_verdict);
    }
}
