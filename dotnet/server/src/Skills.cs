namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// Resolves a skill name to its markdown body. <c>null</c> means "unknown skill" — the dispatcher turns
/// that into a <c>SKILL_NOT_FOUND</c> error and does NOT run the turn, so a typo'd skill never silently
/// degrades into an unskilled answer. The C# analog of the Rust <c>skills::SkillResolver</c> trait.
/// </summary>
public interface ISkillResolver
{
    /// <summary>The skill's markdown body, or <c>null</c> when no such skill exists.</summary>
    Task<string?> ResolveAsync(string name, CancellationToken cancellationToken = default);
}

/// <summary>
/// Skill-resolution helpers — the engine side of <c>send_message.skill</c> (Rust PR #338).
///
/// A <em>skill</em> is a named, reusable recipe (a markdown body). Before this seam every client resolved
/// the skill itself and prepended the body to the message text, so the wire carried prose and the body
/// persisted into conversation history, replayed as context on every later turn. Now the wire carries
/// <em>intent</em> — <c>skill: "code-review"</c> — and the server composes it into the turn's system prompt.
/// </summary>
public static class Skills
{
    /// <summary>Env var naming the skill roots for the default <see cref="DirSkillResolver"/>:
    /// a <c>:</c>-separated directory list, searched in order.</summary>
    public const string SkillsDirEnv = "SMOOTH_SKILLS_DIR";

    /// <summary>
    /// Render a resolved skill as a system-prompt section. The skill moved from the <em>user message</em>
    /// (where clients used to prepend it) to the <em>system prompt</em>, so this framing line is what tells
    /// the model the skill applies to this turn.
    /// </summary>
    public static string SkillSection(string name, string body) =>
        $"## Skill: {name}\n\nThe user invoked this skill for this turn. Follow it.\n\n{body}";

    /// <summary>
    /// Whether <paramref name="name"/> is a legal skill name. Deliberately strict: ASCII alphanumerics,
    /// <c>-</c> and <c>_</c> only. That is the kebab-case convention skills already use, and it makes path
    /// traversal (<c>..</c>, <c>/</c>, <c>\</c>, NUL) unrepresentable rather than filtered — the name is
    /// joined onto a filesystem root by <see cref="DirSkillResolver"/>.
    /// </summary>
    public static bool IsValidSkillName(string? name) =>
        !string.IsNullOrEmpty(name)
        && name.Length <= 128
        && name.All(c => (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') || c == '-' || c == '_');

    /// <summary>
    /// Strip a leading YAML frontmatter block (<c>---</c> … <c>---</c>), returning the body. SKILL.md files
    /// carry frontmatter (description, triggers, allowed tools) that is discovery metadata, not instructions —
    /// the model should see only the body. Unterminated frontmatter is returned untouched rather than
    /// swallowing the file.
    /// </summary>
    public static string StripFrontmatter(string text)
    {
        if (!text.StartsWith("---\n", StringComparison.Ordinal))
        {
            return text;
        }
        var rest = text.Substring(4);
        // The closing fence is a line that is exactly `---`.
        for (var idx = rest.IndexOf("---", StringComparison.Ordinal); idx >= 0; idx = rest.IndexOf("---", idx + 1, StringComparison.Ordinal))
        {
            var atLineStart = idx == 0 || rest[idx - 1] == '\n';
            var restOfLine = rest.Substring(idx + 3);
            if (atLineStart && restOfLine.StartsWith('\n'))
            {
                return restOfLine.TrimStart('\n');
            }
        }
        return text;
    }

    /// <summary>
    /// Resolve <paramref name="name"/> through <paramref name="resolver"/> and render it as a system-prompt
    /// section. <c>null</c> when there is no resolver installed or the skill is unknown — both are
    /// <c>SKILL_NOT_FOUND</c> to the client (the distinction is a deployment detail the caller should not
    /// have to guess at).
    /// </summary>
    public static async Task<string?> ResolveSectionAsync(ISkillResolver? resolver, string name, CancellationToken cancellationToken = default)
    {
        if (resolver is null)
        {
            return null;
        }
        var body = await resolver.ResolveAsync(name, cancellationToken).ConfigureAwait(false);
        return body is null ? null : SkillSection(name, body);
    }
}

/// <summary>
/// The default resolver: reads <c>&lt;root&gt;/&lt;name&gt;/SKILL.md</c>, first root wins. Built from
/// <see cref="Skills.SkillsDirEnv"/>; unset ⇒ nothing is installed and any <c>skill</c> field is a clean
/// <c>SKILL_NOT_FOUND</c>, so a multi-tenant deploy never reads host skills by accident.
/// </summary>
public sealed class DirSkillResolver : ISkillResolver
{
    /// <summary>The roots, searched in order.</summary>
    public IReadOnlyList<string> Roots { get; }

    /// <summary>Build over an explicit list of roots, searched in order.</summary>
    public DirSkillResolver(IReadOnlyList<string> roots) => Roots = roots;

    /// <summary>
    /// Build from <see cref="Skills.SkillsDirEnv"/>. <c>null</c> when the var is unset or names no non-empty
    /// root, so the caller installs nothing and the feature stays off by default.
    /// </summary>
    public static DirSkillResolver? FromEnv() =>
        Environment.GetEnvironmentVariable(Skills.SkillsDirEnv) is { } list ? FromPathList(list) : null;

    /// <summary>
    /// Build from a <c>:</c>-separated path list — the parsed half of <see cref="FromEnv"/>, so it is testable
    /// without touching the process environment. <c>null</c> when the list names no non-empty root.
    /// </summary>
    // ponytail: ':' hardcoded to match the Rust reference rather than Path.PathSeparator. On Windows that
    // makes a drive-qualified root ("C:\skills") unrepresentable; switch to a platform separator only if a
    // Windows host ever sets the var — and change Rust in the same breath, or the two lanes diverge.
    public static DirSkillResolver? FromPathList(string list)
    {
        var roots = list.Split(':').Select(s => s.Trim()).Where(s => s.Length > 0).ToList();
        return roots.Count > 0 ? new DirSkillResolver(roots) : null;
    }

    /// <inheritdoc />
    public Task<string?> ResolveAsync(string name, CancellationToken cancellationToken = default)
    {
        if (!Skills.IsValidSkillName(name))
        {
            return Task.FromResult<string?>(null);
        }
        foreach (var root in Roots)
        {
            // ponytail: blocking read on the async path, matching Rust. A SKILL.md is a few KB off local
            // disk; move to async IO if a resolver ever fronts network storage.
            string text;
            try
            {
                text = File.ReadAllText(Path.Combine(root, name, "SKILL.md"));
            }
            catch (IOException)
            {
                continue;
            }
            catch (UnauthorizedAccessException)
            {
                continue;
            }
            var body = Skills.StripFrontmatter(text).Trim();
            if (body.Length > 0)
            {
                return Task.FromResult<string?>(body);
            }
        }
        return Task.FromResult<string?>(null);
    }
}
