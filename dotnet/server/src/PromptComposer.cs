namespace SmooAI.SmoothOperator.Server;

/// <summary>Where <see cref="PromptSections.Base"/> came from (SMOODEV-3798).</summary>
public enum BaseSource
{
    /// <summary>The answering agent's own instructions.</summary>
    Agent,
    /// <summary>The org's saved persona override.</summary>
    OrgPersona,
    /// <summary>The host's server-wide system prompt (persona).</summary>
    DefaultPersona,
    /// <summary>The server's built-in prompt.</summary>
    BuiltIn,
}

/// <summary>
/// Every input to one turn's system prompt (SMOODEV-3798, parity with the Rust reference's
/// <c>smooth_operator_server::prompt_composer::PromptSections</c>). A <c>null</c> section does not
/// apply to this turn.
/// </summary>
public sealed record PromptSections
{
    public required string Base { get; init; }
    public BaseSource BaseSource { get; init; } = BaseSource.BuiltIn;
    public string? Greeting { get; init; }
    public string? Workflow { get; init; }
    public string? Skill { get; init; }
    public string SuggestedReplies { get; init; } = "";
    public AgentConfig? Agent { get; init; }
    public AccessContext? Access { get; init; }
    public bool SessionAuthenticated { get; init; }
    public string ConversationId { get; init; } = "";
}

/// <summary>Assembles the turn's system prompt. Return the sections in order; blank sections are
/// dropped and the rest joined with a blank line.</summary>
public interface IPromptComposer
{
    IReadOnlyList<string> Compose(PromptSections sections);
}

/// <summary>The historical order: base, greeting, workflow, skill, suggested replies.</summary>
public sealed class DefaultPromptComposer : IPromptComposer
{
    public static readonly DefaultPromptComposer Instance = new();

    /// <summary>The default ordered sections, for hosts that extend rather than reorder.</summary>
    public static List<string> Sections(PromptSections s)
    {
        var out_ = new List<string> { s.Base };
        if (s.Greeting is not null) out_.Add(s.Greeting);
        if (s.Workflow is not null) out_.Add(s.Workflow);
        if (s.Skill is not null) out_.Add(s.Skill);
        out_.Add(s.SuggestedReplies);
        return out_;
    }

    public IReadOnlyList<string> Compose(PromptSections sections) => Sections(sections);
}

/// <summary>Joins composed sections.</summary>
public static class PromptComposition
{
    /// <summary>Drop blank sections and join the rest with a blank line.</summary>
    public static string Join(IEnumerable<string> sections) =>
        string.Join("\n\n", sections.Where(s => !string.IsNullOrWhiteSpace(s)));

    /// <summary>Compose <paramref name="sections"/> with <paramref name="composer"/> (or the default) and join.</summary>
    public static string Render(IPromptComposer? composer, PromptSections sections) =>
        Join((composer ?? DefaultPromptComposer.Instance).Compose(sections));
}
