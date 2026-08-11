using System.Text.Json.Nodes;

namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// A vision image attachment parsed from <c>send_message.images[]</c>. Attached to the turn's user
/// message as an OpenAI <c>image_url</c> content part. Fail-soft: a malformed wire entry is dropped
/// rather than rejecting the turn.
/// </summary>
public sealed record UserImage(string Url, string? Detail);

/// <summary>
/// A non-image file attachment parsed from <c>send_message.files[]</c>. Surfaced to host tools via
/// <see cref="TurnContext.Files"/> so the host can land it in the agent's workspace — the protocol
/// layer never sends file bytes to the model. Fail-soft, like <see cref="UserImage"/>.
/// </summary>
public sealed record UserFile(string Name, string? MimeType, string Url);

/// <summary>
/// Per-turn ambient context threaded into tool execution — the C# analog of the Rust
/// <c>ToolProviderContext</c>'s file list + directive sink. The engine (a fixed published NuGet)
/// invokes host tools synchronously inside the turn's async flow, so an <see cref="AsyncLocal{T}"/>
/// published around the run reaches a tool's callback without any engine-side seam. A host tool reads
/// <see cref="Files"/> and writes <see cref="Directive"/>; the runner drains the directive after the
/// turn onto <c>eventual_response.directive</c>.
/// </summary>
public sealed class TurnContext
{
    private static readonly AsyncLocal<TurnContext?> Ambient = new();

    /// <summary>The context of the turn currently running on this async flow; <c>null</c> outside a turn.</summary>
    public static TurnContext? Current => Ambient.Value;

    /// <summary>Non-image files the client attached this turn (a host tool may land them in a workspace).</summary>
    public IReadOnlyList<UserFile> Files { get; }

    /// <summary>
    /// The client-side directive a host tool emitted this turn (opaque JSON, e.g. a <c>send_file</c>
    /// object). <c>null</c> ⇒ no host tool wrote one, so <c>eventual_response</c> omits <c>directive</c>
    /// (back-compat). Last-write-wins when several tools write.
    /// </summary>
    public JsonNode? Directive { get; set; }

    public TurnContext(IReadOnlyList<UserFile>? files = null) => Files = files ?? Array.Empty<UserFile>();

    /// <summary>Publish this as the ambient <see cref="Current"/> for the run; disposing restores the prior.</summary>
    public IDisposable Enter()
    {
        var prior = Ambient.Value;
        Ambient.Value = this;
        return new Restore(prior);
    }

    private sealed class Restore(TurnContext? prior) : IDisposable
    {
        private bool _done;

        public void Dispose()
        {
            if (_done)
            {
                return;
            }
            _done = true;
            Ambient.Value = prior;
        }
    }
}
