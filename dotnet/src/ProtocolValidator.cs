// Runtime validation against the spec JSON Schemas, using NJsonSchema.
//
// The spec ships draft 2020-12 schemas with internal $defs (no cross-file $refs).
// NJsonSchema resolves its native `definitions` keyword (including def-to-def refs)
// but not draft 2020-12 `$defs`, so each file is loaded with `$defs` rewritten to
// `definitions` and `#/$defs/` rewritten to `#/definitions/`. We then expose:
//
//   • ValidateAt(schemaRef, instance) — validate against a spec-relative ref like
//     "events/stream-chunk.schema.json" or
//     "actions/send-message.schema.json#/$defs/Request" (the exact form used by
//     conformance/fixtures.json).
//   • ValidateEvent / ValidateAction — convenience validators that pick the right
//     schema from a frame's discriminator and validate it.
//
// Schemas are loaded from the spec directory on disk. This type is for build/test/
// server use; the wire client does not depend on it (validation is opt-in).

using System.Text.Json;
using NJsonSchema;
using NJsonSchema.Validation;

namespace SmooAI.SmoothOperator;

/// <summary>Result of a schema validation: validity plus any errors.</summary>
public sealed class ValidationResult
{
    public bool IsValid { get; init; }
    public IReadOnlyList<ValidationError> Errors { get; init; } = Array.Empty<ValidationError>();

    /// <summary>Render the errors into a single human-readable string.</summary>
    public string FormatErrors()
        => string.Join("; ", Errors.Select(e => $"{(string.IsNullOrEmpty(e.Path) ? "<root>" : e.Path)} {e.Kind}".Trim()));
}

public sealed class ProtocolValidator
{
    /// <summary>Maps an event <c>type</c> to its schema file (spec-relative).</summary>
    private static readonly IReadOnlyDictionary<string, string> EventSchemaFile = new Dictionary<string, string>
    {
        [EventTypes.ImmediateResponse] = "events/immediate-response.schema.json",
        [EventTypes.EventualResponse] = "events/eventual-response.schema.json",
        [EventTypes.StreamChunk] = "events/stream-chunk.schema.json",
        [EventTypes.StreamToken] = "events/stream-token.schema.json",
        [EventTypes.Keepalive] = "events/keepalive.schema.json",
        [EventTypes.WriteConfirmationRequired] = "events/write-confirmation-required.schema.json",
        [EventTypes.OtpVerificationRequired] = "events/otp-verification-required.schema.json",
        [EventTypes.OtpSent] = "events/otp-sent.schema.json",
        [EventTypes.OtpVerified] = "events/otp-verified.schema.json",
        [EventTypes.OtpInvalid] = "events/otp-invalid.schema.json",
        [EventTypes.Error] = "events/error.schema.json",
        [EventTypes.Pong] = "events/pong.schema.json",
    };

    /// <summary>Maps an action <c>action</c> to its request schema ref (spec-relative).</summary>
    private static readonly IReadOnlyDictionary<string, string> ActionSchemaRef = new Dictionary<string, string>
    {
        [ActionTypes.CreateConversationSession] = "actions/create-conversation-session.schema.json#/$defs/Request",
        [ActionTypes.SendMessage] = "actions/send-message.schema.json#/$defs/Request",
        [ActionTypes.GetSession] = "actions/get-session.schema.json#/$defs/Request",
        [ActionTypes.GetConversationMessages] = "actions/get-messages.schema.json#/$defs/Request",
        [ActionTypes.ConfirmToolAction] = "actions/confirm-tool-action.schema.json#/$defs/Request",
        [ActionTypes.VerifyOtp] = "actions/verify-otp.schema.json#/$defs/Request",
        [ActionTypes.Ping] = "actions/ping.schema.json#/$defs/Request",
    };

    private readonly Dictionary<string, JsonSchema> _byFile;

    private ProtocolValidator(Dictionary<string, JsonSchema> byFile) => _byFile = byFile;

    /// <summary>Default spec directory relative to the loaded assembly (walks up to find <c>spec/</c>).</summary>
    public static string DefaultSpecDir()
    {
        var dir = new DirectoryInfo(AppContext.BaseDirectory);
        while (dir is not null)
        {
            var candidate = Path.Combine(dir.FullName, "spec");
            if (Directory.Exists(candidate)) return candidate;
            dir = dir.Parent;
        }
        throw new DirectoryNotFoundException("Could not locate a spec/ directory above the application base.");
    }

    /// <summary>Load every <c>*.schema.json</c> under <paramref name="specDir"/> and register it.</summary>
    public static async Task<ProtocolValidator> LoadAsync(string? specDir = null)
    {
        specDir ??= DefaultSpecDir();
        var byFile = new Dictionary<string, JsonSchema>(StringComparer.Ordinal);

        foreach (var sub in new[] { "", "actions", "events", "domain", "interactions" })
        {
            var dir = string.IsNullOrEmpty(sub) ? specDir : Path.Combine(specDir, sub);
            if (!Directory.Exists(dir)) continue;

            foreach (var file in Directory.GetFiles(dir, "*.schema.json"))
            {
                var rel = (string.IsNullOrEmpty(sub) ? Path.GetFileName(file) : $"{sub}/{Path.GetFileName(file)}");
                var raw = await File.ReadAllTextAsync(file).ConfigureAwait(false);
                // NJsonSchema resolves `definitions`, not draft 2020-12 `$defs`.
                var njson = raw.Replace("\"$defs\"", "\"definitions\"").Replace("#/$defs/", "#/definitions/");
                byFile[rel] = await JsonSchema.FromJsonAsync(njson).ConfigureAwait(false);
            }
        }

        return new ProtocolValidator(byFile);
    }

    /// <summary>
    /// Validate <paramref name="instance"/> against a spec-relative schema ref — the
    /// form used in fixtures.json: a file path, optionally with a JSON-pointer fragment
    /// into the schema's $defs (e.g. "actions/ping.schema.json#/$defs/Request").
    /// </summary>
    public ValidationResult ValidateAt(string schemaRef, object instance)
    {
        var schema = Resolve(schemaRef);
        var json = instance as string ?? JsonSerializer.Serialize(instance);
        var errors = schema.Validate(json);
        return new ValidationResult { IsValid = errors.Count == 0, Errors = errors.ToList() };
    }

    /// <summary>Validate a server event frame by selecting the schema from its <c>type</c>.</summary>
    public ValidationResult ValidateEvent(string eventType, object instance)
    {
        if (!EventSchemaFile.TryGetValue(eventType, out var file))
            return Synthetic($"Unknown event type: {eventType}");
        return ValidateAt(file, instance);
    }

    /// <summary>Validate a client action frame by selecting the schema from its <c>action</c>.</summary>
    public ValidationResult ValidateAction(string action, object instance)
    {
        if (!ActionSchemaRef.TryGetValue(action, out var reference))
            return Synthetic($"Unknown action: {action}");
        return ValidateAt(reference, instance);
    }

    private JsonSchema Resolve(string schemaRef)
    {
        var parts = schemaRef.Split('#', 2);
        var file = parts[0];
        if (!_byFile.TryGetValue(file, out var schema))
            throw new InvalidOperationException($"No schema registered for \"{file}\" (ref \"{schemaRef}\").");

        if (parts.Length == 1 || string.IsNullOrEmpty(parts[1]))
            return schema;

        // Pointer like "/$defs/Request" — but we rewrote $defs → definitions on load,
        // and NJsonSchema exposes them via .Definitions. Resolve the trailing name.
        var pointer = parts[1].TrimStart('/');
        var segments = pointer.Split('/');
        if (segments.Length == 2 && (segments[0] == "$defs" || segments[0] == "definitions"))
        {
            if (schema.Definitions.TryGetValue(segments[1], out var def))
                return def;
            throw new InvalidOperationException($"Definition \"{segments[1]}\" not found in \"{file}\".");
        }

        throw new InvalidOperationException($"Unsupported schema pointer \"{parts[1]}\" in ref \"{schemaRef}\".");
    }

    private static ValidationResult Synthetic(string message)
        => new()
        {
            IsValid = false,
            Errors = new List<ValidationError>
            {
                new(ValidationErrorKind.Unknown, null, message, null!, null!),
            },
        };
}
