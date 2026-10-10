namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// PromptComposer seam (SMOODEV-3798) — parity with the Rust reference's <c>prompt_composer</c>
/// tests: the default order, blank sections dropped, a host composer that appends last and sees the
/// context, and a reorder. Runner integration lives in <see cref="WorkflowTests"/>.
/// </summary>
public class PromptComposerTests
{
    private static PromptSections Sections() => new()
    {
        Base = "BASE",
        BaseSource = BaseSource.Agent,
        Greeting = "GREETING",
        Workflow = "WORKFLOW",
        Skill = "SKILL",
        SuggestedReplies = "TRAILER",
        ConversationId = "c1",
    };

    [Fact]
    public void Default_KeepsHistoricalOrder() =>
        Assert.Equal("BASE\n\nGREETING\n\nWORKFLOW\n\nSKILL\n\nTRAILER", PromptComposition.Render(null, Sections()));

    [Fact]
    public void AbsentSections_AreSkipped() =>
        Assert.Equal("BASE\n\nTRAILER", PromptComposition.Render(null, Sections() with { Greeting = null, Workflow = null, Skill = null }));

    [Fact]
    public void Join_DropsBlankSections() =>
        Assert.Equal("a\n\nb", PromptComposition.Join(new[] { "a", "", "  ", "b" }));

    private sealed class Safety : IPromptComposer
    {
        public IReadOnlyList<string> Compose(PromptSections s)
        {
            var out_ = DefaultPromptComposer.Sections(s);
            out_.Add($"SAFETY(source={s.BaseSource}, authed={s.SessionAuthenticated}, conv={s.ConversationId})");
            return out_;
        }
    }

    [Fact]
    public void HostComposer_AppendsLast_AndSeesTheContext()
    {
        var out_ = PromptComposition.Render(new Safety(), Sections());
        Assert.StartsWith("BASE\n\nGREETING", out_);
        Assert.EndsWith("TRAILER\n\nSAFETY(source=Agent, authed=False, conv=c1)", out_);
    }

    private sealed class Reorder : IPromptComposer
    {
        public IReadOnlyList<string> Compose(PromptSections s) => new[] { s.Base, s.Workflow ?? "", s.SuggestedReplies };
    }

    [Fact]
    public void HostComposer_MayReorder() =>
        Assert.Equal("BASE\n\nWORKFLOW\n\nTRAILER", PromptComposition.Render(new Reorder(), Sections()));
}
