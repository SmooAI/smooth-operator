using System.Text.Json.Nodes;
using Microsoft.Extensions.AI;
using SmooAI.SmoothOperator.Server;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// Skill-resolution parity with the Rust reference (PR #338, <c>rust/smooth-operator-server/src/skills.rs</c>).
/// The wire carries the INTENT (<c>send_message.skill: "code-review"</c>); the server resolves the body and
/// composes it into the turn's system prompt, so the persisted user message stays exactly what the user typed.
/// Each test is named after its Rust counterpart so parity gaps stay visible.
/// </summary>
public class SkillTests : IDisposable
{
    private readonly string _tmp = Path.Combine(Path.GetTempPath(), "skilltests-" + Guid.NewGuid().ToString("n"));

    public void Dispose()
    {
        if (Directory.Exists(_tmp))
        {
            Directory.Delete(_tmp, recursive: true);
        }
        GC.SuppressFinalize(this);
    }

    private string WriteSkill(string root, string name, string body)
    {
        var dir = Path.Combine(_tmp, root, name);
        Directory.CreateDirectory(dir);
        File.WriteAllText(Path.Combine(dir, "SKILL.md"), body);
        return Path.Combine(_tmp, root);
    }

    // ── rust: rejects_traversal_and_separators ───────────────────────────────

    [Fact]
    public void RejectsTraversalAndSeparators()
    {
        Assert.True(Skills.IsValidSkillName("code-review"));
        Assert.True(Skills.IsValidSkillName("add_show"));
        Assert.False(Skills.IsValidSkillName(""));
        Assert.False(Skills.IsValidSkillName(".."));
        Assert.False(Skills.IsValidSkillName("../../etc/passwd"));
        Assert.False(Skills.IsValidSkillName("a/b"));
        Assert.False(Skills.IsValidSkillName("a\\b"));
        Assert.False(Skills.IsValidSkillName("a b"));
        Assert.False(Skills.IsValidSkillName(new string('a', 129)));
        Assert.True(Skills.IsValidSkillName(new string('a', 128)));
    }

    // ── rust: strips_frontmatter_only_when_well_formed ───────────────────────

    [Fact]
    public void StripsFrontmatterOnlyWhenWellFormed()
    {
        Assert.Equal("Body here\n", Skills.StripFrontmatter("---\nname: x\ndescription: y\n---\nBody here\n"));
        // No frontmatter → untouched.
        Assert.Equal("Body here\n", Skills.StripFrontmatter("Body here\n"));
        // Unterminated → untouched (don't swallow the file).
        Assert.Equal("---\nname: x\n", Skills.StripFrontmatter("---\nname: x\n"));
        // A `---` mid-body (a markdown rule) after real frontmatter still closes at the FIRST fence.
        Assert.Equal("intro\n\n---\n\nmore\n", Skills.StripFrontmatter("---\nname: x\n---\nintro\n\n---\n\nmore\n"));
    }

    // ── rust: dir_resolver_reads_first_matching_root ─────────────────────────

    [Fact]
    public async Task DirResolverReadsFirstMatchingRoot()
    {
        var high = WriteSkill("high", "greet", "---\nname: greet\n---\nHIGH BODY\n");
        var low = WriteSkill("low", "greet", "---\nname: greet\n---\nLOW BODY\n");

        var resolver = new DirSkillResolver(new[] { high, low });
        Assert.Equal("HIGH BODY", await resolver.ResolveAsync("greet"));
        Assert.Null(await resolver.ResolveAsync("nope"));
        // Traversal can't escape the root even if a file exists above it.
        Assert.Null(await resolver.ResolveAsync("../low/greet"));

        // Low root alone falls through to its own copy.
        var fallback = new DirSkillResolver(new[] { Path.Combine(_tmp, "missing"), low });
        Assert.Equal("LOW BODY", await fallback.ResolveAsync("greet"));
    }

    // ── rust: path_list_parsing_is_off_when_empty ────────────────────────────

    [Fact]
    public void PathListParsingIsOffWhenEmpty()
    {
        Assert.Null(DirSkillResolver.FromPathList(""));
        Assert.Null(DirSkillResolver.FromPathList("  : "));
        var r = DirSkillResolver.FromPathList("/a: /b :");
        Assert.NotNull(r);
        Assert.Equal(new[] { "/a", "/b" }, r!.Roots);
    }

    // ── rust: resolve_section_composes_and_reports_unknown ───────────────────

    [Fact]
    public async Task ResolveSectionComposesAndReportsUnknown()
    {
        var root = WriteSkill("only", "review", "Check the diff.");

        ISkillResolver resolver = new DirSkillResolver(new[] { root });
        var section = await Skills.ResolveSectionAsync(resolver, "review");
        Assert.NotNull(section);
        Assert.StartsWith("## Skill: review\n", section);
        Assert.EndsWith("Check the diff.", section);

        Assert.Null(await Skills.ResolveSectionAsync(resolver, "unknown"));
        // No resolver installed ⇒ every skill is unknown.
        Assert.Null(await Skills.ResolveSectionAsync(null, "review"));
    }

    // ── handler parity: fail-CLOSED, unlike images ───────────────────────────

    [Fact]
    public async Task UnknownSkill_FailsClosed_WithSkillNotFound_AndDoesNotRunTheTurn()
    {
        var chat = new RecordingChatClient("should never run");
        var root = WriteSkill("only", "review", "Check the diff.");
        var dispatcher = new FrameDispatcher(
            new InMemorySessionStore(), chat, skillResolver: new DirSkillResolver(new[] { root }));
        var events = new List<JsonObject>();
        var sessionId = await CreateSessionAsync(dispatcher, events);

        await dispatcher.DispatchAsync(
            $$"""{"action":"send_message","requestId":"r2","sessionId":"{{sessionId}}","message":"hi","skill":"nope"}""",
            events.Add);
        await dispatcher.WaitForTurnsAsync();

        var error = Assert.Single(events, e => e["type"]?.GetValue<string>() == "error");
        // Per spec/events/error.schema.json the descriptor sits at the envelope level AND under data.error.
        Assert.Equal("SKILL_NOT_FOUND", error["error"]!["code"]!.GetValue<string>());
        Assert.Equal("SKILL_NOT_FOUND", error["data"]!["error"]!["code"]!.GetValue<string>());
        Assert.Contains("nope", error["error"]!["message"]!.GetValue<string>());
        // Fail-closed: the model was never called.
        Assert.Empty(chat.LastMessages);
    }

    [Fact]
    public async Task ResolvedSkill_GoesToTheSystemPrompt_NotTheUserMessage()
    {
        var chat = new RecordingChatClient("done");
        var root = WriteSkill("only", "review", "---\nname: review\n---\nCheck the diff.\n");
        var dispatcher = new FrameDispatcher(
            new InMemorySessionStore(), chat, skillResolver: new DirSkillResolver(new[] { root }));
        var events = new List<JsonObject>();
        var sessionId = await CreateSessionAsync(dispatcher, events);

        await dispatcher.DispatchAsync(
            $$"""{"action":"send_message","requestId":"r2","sessionId":"{{sessionId}}","message":"look at this","skill":"review"}""",
            events.Add);
        await dispatcher.WaitForTurnsAsync();

        var system = chat.LastMessages.Single(m => m.Role == ChatRole.System).Text;
        Assert.Contains("## Skill: review", system);
        Assert.Contains("Check the diff.", system);

        // The persisted/replayed user message is exactly what the user typed — no prose on the wire.
        var user = chat.LastMessages.Last(m => m.Role == ChatRole.User);
        Assert.Equal("look at this", user.Text);
        Assert.DoesNotContain("Check the diff.", user.Text);
    }

    [Fact]
    public async Task AbsentSkill_IsUnchangedBehavior()
    {
        var chat = new RecordingChatClient("done");
        var dispatcher = new FrameDispatcher(new InMemorySessionStore(), chat);
        var events = new List<JsonObject>();
        var sessionId = await CreateSessionAsync(dispatcher, events);

        await dispatcher.DispatchAsync(
            $$"""{"action":"send_message","requestId":"r2","sessionId":"{{sessionId}}","message":"plain"}""",
            events.Add);
        await dispatcher.WaitForTurnsAsync();

        Assert.DoesNotContain(events, e => e["type"]?.GetValue<string>() == "error");
        var system = chat.LastMessages.Single(m => m.Role == ChatRole.System).Text;
        Assert.DoesNotContain("## Skill:", system);
    }

    /// <summary>A skill field present but empty/whitespace is treated as absent (Rust trims then filters).</summary>
    [Fact]
    public async Task BlankSkill_IsTreatedAsAbsent_NotAsNotFound()
    {
        var chat = new RecordingChatClient("done");
        var dispatcher = new FrameDispatcher(new InMemorySessionStore(), chat);
        var events = new List<JsonObject>();
        var sessionId = await CreateSessionAsync(dispatcher, events);

        await dispatcher.DispatchAsync(
            $$"""{"action":"send_message","requestId":"r2","sessionId":"{{sessionId}}","message":"plain","skill":"   "}""",
            events.Add);
        await dispatcher.WaitForTurnsAsync();

        Assert.DoesNotContain(events, e => e["type"]?.GetValue<string>() == "error");
    }

    private static async Task<string> CreateSessionAsync(FrameDispatcher dispatcher, List<JsonObject> events)
    {
        await dispatcher.DispatchAsync("""{"action":"create_conversation_session","agentId":"11111111-1111-1111-1111-111111111111","requestId":"r1"}""", events.Add);
        var sessionId = events[0]["data"]!["sessionId"]!.GetValue<string>();
        events.Clear();
        return sessionId;
    }
}
