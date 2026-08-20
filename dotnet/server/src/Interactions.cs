using System.Collections.Concurrent;
using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.Extensions.AI;

namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// The <b>Rich Interactions</b> framework — a kind-agnostic generalization of the write-confirmation
/// park/resume (<see cref="ConfirmationRegistry"/>). An agent turn calls a per-kind <em>raise</em> tool
/// (<c>request_&lt;kind&gt;</c>); on a channel that declared the kind's render capability the turn
/// <b>parks</b> emitting <c>interaction_required</c> and resumes when the client replies with a
/// <c>submit_interaction</c> frame; on a text-only channel the raise degrades to a conversational
/// fallback directive and the model submits via the <c>submit_interaction</c> tool. Mirrors the Rust
/// reference <c>smooth-operator/src/interaction.rs</c> + <c>tools/interaction.rs</c>.
/// </summary>
public sealed record InteractionFieldError(string Field, string Message)
{
    public JsonObject ToWire() => new() { ["field"] = Field, ["message"] = Message };
}

/// <summary>A parsed raise: which kind, the kind-specific render spec, and why it was raised.</summary>
public sealed record InteractionRequest(string Kind, JsonNode Spec, string Reason);

/// <summary>Thrown by <see cref="IInteractionKind.ParseRequest"/> when the raise args violate the
/// kind's contract (count/uniqueness/length). The raise tool returns the message to the model as a
/// tool result rather than faulting the turn — mirrors Rust's <c>parse_request</c> <c>?</c>.</summary>
public sealed class InteractionParseException(string message) : Exception(message);

/// <summary>How a parked interaction resolved. Canonicalized <see cref="Values"/> is present only for
/// <c>submitted</c>. The C# analog of Rust's <c>InteractionOutcome</c> plus the no-response backstop.</summary>
public sealed record InteractionOutcome(string Status, JsonNode? Values)
{
    public static InteractionOutcome Submitted(JsonNode values) => new("submitted", values);
    public static readonly InteractionOutcome Declined = new("declined", null);
    public static readonly InteractionOutcome NoResponse = new("no_response", null);
}

/// <summary>Result of a kind's <see cref="IInteractionKind.Validate"/>: canonicalized values, OR a
/// one-pass list of per-field errors (every failure, so a card annotates all in one round-trip).</summary>
public sealed record InteractionValidation(JsonNode? Canonical, IReadOnlyList<InteractionFieldError>? Errors)
{
    public bool Ok => Errors is null;
    public static InteractionValidation Valid(JsonNode canonical) => new(canonical, null);
    public static InteractionValidation Invalid(IReadOnlyList<InteractionFieldError> errors) => new(null, errors);
}

/// <summary>
/// One interaction kind — the extension seam. Selects the client card + the server validator, declares
/// the render capability that gates its rich path, produces the LLM-facing raise tool, and owns the
/// parse/validate/fallback logic. Mirrors the Rust <c>InteractionKind</c> trait.
/// </summary>
public interface IInteractionKind
{
    /// <summary>Wire id (e.g. <c>choices</c>). Selects the client card and this validator.</summary>
    string Kind { get; }

    /// <summary>The client render capability that gates the rich path (e.g. <c>choice_chips</c>). A
    /// session that lists it in <c>supports</c> gets the parked card; otherwise the conversational fallback.</summary>
    string Capability { get; }

    /// <summary>The raise tool name — <c>request_&lt;kind&gt;</c>.</summary>
    string ToolName => $"request_{Kind}";

    /// <summary>The raise tool's model-facing description.</summary>
    string ToolDescription { get; }

    /// <summary>The raise tool's JSON Schema (the <c>parameters</c> the model fills).</summary>
    JsonElement ToolSchema { get; }

    /// <summary>Parse + constraint-check the raise args into a request. Throws
    /// <see cref="InteractionParseException"/> on a contract violation.</summary>
    InteractionRequest ParseRequest(JsonObject args);

    /// <summary>Validate submitted <paramref name="values"/> against <paramref name="spec"/> (which may
    /// be <c>null</c> on the conversational path — the kind then does format-only checks). Returns the
    /// canonicalized values, or ALL field errors.</summary>
    InteractionValidation Validate(JsonNode? spec, JsonNode? values);

    /// <summary>The directive handed to the model when the channel can't render the card: how to ask
    /// the interaction conversationally and submit it via the <c>submit_interaction</c> tool.</summary>
    string FallbackDirective(JsonNode? spec, string reason);

    /// <summary>
    /// The kind's host-side <b>effect</b>, run after a VALID submit on EITHER path — the rich
    /// <c>submit_interaction</c> frame (<c>FrameDispatcher</c>) and the conversational-fallback
    /// <c>submit_interaction</c> tool. Kind-agnostic seam: the caller resolves the kind from the DI
    /// catalog and calls this without knowing which kind it is; <c>choices</c> inherits the no-op default,
    /// <c>identity_intake</c> overrides to stamp the session's captured contact. A new kind adds its
    /// effect by overriding here — no central switch to edit. The C# analog of Rust's kind-routed
    /// <c>attach_interaction_effect</c>.
    /// </summary>
    void ApplyEffect(IInteractionEffectContext context, JsonNode canonicalValues) { }
}

/// <summary>
/// The session-scoped host surface an interaction kind's <see cref="IInteractionKind.ApplyEffect"/> acts
/// through — the C# analog of the <c>AppState</c> handle Rust's <c>attach_interaction_effect</c> closes
/// over. Grows one method per host effect a kind needs; today just the identity attach. Kept minimal and
/// honest (no speculative generality) — the SEAM is kind-agnostic, the effects themselves are not.
/// </summary>
public interface IInteractionEffectContext
{
    /// <summary>The session the accepted interaction belongs to.</summary>
    string SessionId { get; }

    /// <summary>Stamp captured contact identity onto the session — the SAME contact the OTP seam reads,
    /// so a captured email/phone is immediately OTP-reachable. Only non-null fields are written (never
    /// clobbers a known field with a null). <c>identity_intake</c>'s effect.</summary>
    void AttachSessionIdentity(string? name, string? email, string? phone);
}

/// <summary>Captured session contact identity — the typed C# analog of the Rust in-memory session's
/// <c>metadata.userName</c> / <c>contactEmail</c> / <c>contactPhone</c> keys.</summary>
public sealed record SessionIdentity(string? Name = null, string? Email = null, string? Phone = null)
{
    /// <summary>Overlay non-null fields from a newer capture onto this one (an intake that collected
    /// only an email keeps a previously captured name).</summary>
    public SessionIdentity Merge(string? name, string? email, string? phone) =>
        new(name ?? Name, email ?? Email, phone ?? Phone);
}

/// <summary>
/// The in-memory, session-keyed contact overlay stamped by <c>identity_intake</c>'s host effect and read
/// by the OTP contact seam — the C# analog of the Rust reference's <c>AppState</c> session-metadata map
/// (an in-process overlay, NOT the durable <see cref="ISessionStore"/>). Per-connection lifetime, like
/// <c>InteractionParkRegistry</c> and the declared-capabilities map: a client re-declares on reconnect.
/// </summary>
public sealed class SessionIdentityRegistry
{
    private readonly ConcurrentDictionary<string, SessionIdentity> _map = new();

    /// <summary>Merge captured contact for a session (non-null fields overwrite; nulls leave prior
    /// values intact). Idempotent-ish: re-submitting the same values is a no-op.</summary>
    public void Attach(string sessionId, string? name, string? email, string? phone) =>
        _map.AddOrUpdate(
            sessionId,
            _ => new SessionIdentity(name, email, phone),
            (_, prior) => prior.Merge(name, email, phone));

    /// <summary>The captured contact for a session, or null when nothing has been stamped.</summary>
    public SessionIdentity? Get(string sessionId) =>
        _map.TryGetValue(sessionId, out var identity) ? identity : null;

    /// <summary>A session-bound <see cref="IInteractionEffectContext"/> for a kind's effect hook.</summary>
    public IInteractionEffectContext EffectContext(string sessionId) => new Context(sessionId, this);

    private sealed record Context(string SessionId, SessionIdentityRegistry Registry) : IInteractionEffectContext
    {
        public void AttachSessionIdentity(string? name, string? email, string? phone) =>
            Registry.Attach(SessionId, name, email, phone);
    }
}

/// <summary>
/// The hosted interaction kinds, looked up by <see cref="IInteractionKind.Kind"/>. The C# analog of the
/// Rust <c>InteractionRegistry</c>. <see cref="Default"/> is the reference catalog.
/// </summary>
public sealed class InteractionCatalog
{
    private readonly IReadOnlyList<IInteractionKind> _kinds;

    public InteractionCatalog(params IInteractionKind[] kinds) => _kinds = kinds;

    public IReadOnlyList<IInteractionKind> Kinds => _kinds;

    /// <summary>First kind whose <see cref="IInteractionKind.Kind"/> matches, else null.</summary>
    public IInteractionKind? Get(string kind) => _kinds.FirstOrDefault(k => k.Kind == kind);

    /// <summary>The reference catalog: the <c>choices</c> + <c>identity_intake</c> kinds. Additional
    /// kinds register here.</summary>
    public static InteractionCatalog Default { get; } = new(new ChoicesKind(), new IdentityIntakeKind());
}

/// <summary>One parked interaction — the record a <c>submit_interaction</c> frame resolves.</summary>
public sealed record PendingInteraction(
    string InteractionId,
    string Kind,
    JsonNode? Spec,
    TaskCompletionSource<InteractionOutcome> Completion);

/// <summary>
/// Session-keyed park store for Rich Interactions — the interaction analog of
/// <see cref="ConfirmationRegistry"/>, but richer: the pending value carries the <c>interactionId</c>
/// (stale-submit guard), <c>kind</c>, and <c>spec</c> (the validation contract), and it exposes a
/// <see cref="Peek"/> distinct from <see cref="Resolve"/> so an <em>invalid</em> submit re-prompts the
/// card without consuming the park (mirrors the Rust <c>pending_interaction</c> peek vs
/// <c>take_interaction</c>). One outstanding interaction per session.
/// </summary>
public sealed class InteractionParkRegistry
{
    private readonly ConcurrentDictionary<string, PendingInteraction> _pending = new();

    /// <summary>Register a fresh park for <paramref name="sessionId"/> and return the task the raising
    /// turn awaits. Any prior park is resolved <see cref="InteractionOutcome.NoResponse"/> first (the
    /// newest raise wins, no dangling turn) — mirrors <see cref="ConfirmationRegistry.Register"/>.</summary>
    public Task<InteractionOutcome> Register(string sessionId, string interactionId, string kind, JsonNode? spec)
    {
        var tcs = new TaskCompletionSource<InteractionOutcome>(TaskCreationOptions.RunContinuationsAsynchronously);
        if (_pending.TryRemove(sessionId, out var prior))
        {
            prior.Completion.TrySetResult(InteractionOutcome.NoResponse);
        }
        _pending[sessionId] = new PendingInteraction(interactionId, kind, spec, tcs);
        return tcs.Task;
    }

    /// <summary>Peek the parked interaction WITHOUT consuming it — an invalid submit must leave the park
    /// intact for a resubmit. Returns null when nothing is parked.</summary>
    public PendingInteraction? Peek(string sessionId) =>
        _pending.TryGetValue(sessionId, out var pending) ? pending : null;

    /// <summary>Resolve + consume the park for <paramref name="sessionId"/>. Returns false when nothing
    /// was parked (a duplicate/stale submit → clean no-op). Taking the entry out guarantees one resolve.</summary>
    public bool Resolve(string sessionId, InteractionOutcome outcome)
    {
        if (!_pending.TryRemove(sessionId, out var pending))
        {
            return false;
        }
        return pending.Completion.TrySetResult(outcome);
    }

    /// <summary>Drop any park for <paramref name="sessionId"/> (turn ended). Idempotent.</summary>
    public void Clear(string sessionId) => _pending.TryRemove(sessionId, out _);

    /// <summary>Resolve every outstanding park <see cref="InteractionOutcome.NoResponse"/> — called on
    /// connection teardown so a parked turn unparks and finishes cleanly (never hangs).</summary>
    public void RejectAll()
    {
        foreach (var key in _pending.Keys.ToArray())
        {
            if (_pending.TryRemove(key, out var pending))
            {
                pending.Completion.TrySetResult(InteractionOutcome.NoResponse);
            }
        }
    }
}

/// <summary>
/// The per-kind <b>raise</b> tool (<c>request_&lt;kind&gt;</c>), one per hosted kind per turn. On a
/// capability-bearing (rich) session it parks — emits <c>interaction_required</c> and awaits the
/// client's <c>submit_interaction</c> — and resumes with the validated payload. On a text-only session
/// it returns the kind's conversational fallback directive instead (and stashes the raised spec so a
/// later <c>submit_interaction</c> tool call can validate format-only). Never faults the turn: a
/// no-response/decline is a normal status the model reads. Mirrors Rust's <c>RequestInteractionTool</c>.
/// </summary>
internal sealed class RequestInteractionTool : AIFunction
{
    /// <summary>How long a parked raise waits for the visitor before resuming <c>no_response</c>.</summary>
    public static readonly TimeSpan ParkTimeout = TimeSpan.FromSeconds(300);

    private readonly IInteractionKind _kind;
    private readonly bool _rich;
    private readonly Action<JsonObject> _sink;
    private readonly string _requestId;
    private readonly string _sessionId;
    private readonly InteractionParkRegistry _park;
    private readonly ConcurrentDictionary<string, JsonNode> _raised;
    private readonly TimeSpan _timeout;

    public RequestInteractionTool(
        IInteractionKind kind,
        bool rich,
        Action<JsonObject> sink,
        string requestId,
        string sessionId,
        InteractionParkRegistry park,
        ConcurrentDictionary<string, JsonNode> raised,
        TimeSpan? timeout = null)
    {
        _kind = kind;
        _rich = rich;
        _sink = sink;
        _requestId = requestId;
        _sessionId = sessionId;
        _park = park;
        _raised = raised;
        _timeout = timeout ?? ParkTimeout;
    }

    public override string Name => _kind.ToolName;
    public override string Description => _kind.ToolDescription;
    public override JsonElement JsonSchema => _kind.ToolSchema;

    protected override async ValueTask<object?> InvokeCoreAsync(AIFunctionArguments arguments, CancellationToken cancellationToken)
    {
        var args = Interactions.ArgsToObject(arguments);
        // The stream loop DEFERRED this tool's toolCall chunk (see TurnRunner.IsInteractionRaise) so
        // the park path can emit it AFTER interaction_required — the canonical (Rust) order, and the
        // same order the write-confirmation park already uses. Every non-park exit emits it here.
        void EmitCall() => _sink(ProtocolEvents.StreamChunk(_requestId, Name, new JsonObject
        {
            ["rawResponse"] = new JsonObject
            {
                ["toolCall"] = new JsonObject { ["name"] = Name, ["arguments"] = args.DeepClone() },
            },
        }));

        InteractionRequest request;
        try
        {
            request = _kind.ParseRequest(args);
        }
        catch (InteractionParseException ex)
        {
            // Contract violation (count/length/uniqueness) — hand the reason back so the model can fix
            // and re-call, exactly like Rust returning the parse error as the tool result.
            EmitCall();
            return ex.Message;
        }

        if (!_rich)
        {
            // Text-only channel: no card. Stash the spec for a format-only submit, and return the
            // conversational directive telling the model to ask + submit via submit_interaction.
            _raised[request.Kind] = request.Spec;
            EmitCall();
            return new JsonObject
            {
                ["mode"] = "conversational",
                ["kind"] = request.Kind,
                ["spec"] = request.Spec.DeepClone(),
                ["reason"] = request.Reason,
                ["instructions"] = _kind.FallbackDirective(request.Spec, request.Reason),
            }.ToJsonString();
        }

        // Rich: park. Register, emit interaction_required, await the client's submit_interaction. The
        // turn runs on a background task, so this await frees the read loop to receive the reply.
        var interactionId = Guid.NewGuid().ToString();
        var parked = _park.Register(_sessionId, interactionId, request.Kind, request.Spec);
        _sink(ProtocolEvents.InteractionRequired(_requestId, interactionId, request.Kind, request.Spec.DeepClone(), request.Reason));
        EmitCall();

        var outcome = await AwaitOutcome(parked, cancellationToken).ConfigureAwait(false);
        return outcome.Status switch
        {
            "submitted" => new JsonObject { ["status"] = "submitted", ["values"] = outcome.Values?.DeepClone() }.ToJsonString(),
            "declined" => new JsonObject
            {
                ["status"] = "declined",
                ["message"] = "The visitor declined. Continue helping them without this and do not ask again this conversation.",
            }.ToJsonString(),
            _ => new JsonObject
            {
                ["status"] = "no_response",
                ["message"] = "The visitor did not respond to the card. Continue without it; you may offer again later if it becomes relevant.",
            }.ToJsonString(),
        };
    }

    /// <summary>Await the park, backstopped by a timeout and the turn's cancellation — either resolves
    /// <c>no_response</c> so the tool never hangs the turn.</summary>
    private async Task<InteractionOutcome> AwaitOutcome(Task<InteractionOutcome> parked, CancellationToken cancellationToken)
    {
        try
        {
            var completed = await Task.WhenAny(parked, Task.Delay(_timeout, cancellationToken)).ConfigureAwait(false);
            if (completed == parked)
            {
                return await parked.ConfigureAwait(false);
            }
        }
        catch (OperationCanceledException)
        {
            // Turn cancelled/torn down — the park is (or will be) resolved elsewhere; treat as no_response.
        }
        _park.Resolve(_sessionId, InteractionOutcome.NoResponse);
        return InteractionOutcome.NoResponse;
    }
}

/// <summary>
/// The generic <c>submit_interaction</c> <b>tool</b> — the conversational-path counterpart of the
/// client <c>submit_interaction</c> frame. Registered when any hosted kind is running in fallback
/// (text-only) so the model, after asking the question conversationally, submits the visitor's answer
/// for the same server-side validation the rich path runs. Mirrors Rust's <c>SubmitInteractionTool</c>.
/// </summary>
internal sealed class SubmitInteractionTool : AIFunction
{
    private readonly InteractionCatalog _catalog;
    private readonly ConcurrentDictionary<string, JsonNode> _raised;
    private readonly IInteractionEffectContext? _effect;

    public SubmitInteractionTool(InteractionCatalog catalog, ConcurrentDictionary<string, JsonNode> raised, IInteractionEffectContext? effect = null)
    {
        _catalog = catalog;
        _raised = raised;
        _effect = effect;
    }

    public override string Name => "submit_interaction";

    public override string Description =>
        "Submit the visitor's answer to an interaction you asked conversationally (because their channel " +
        "can't render the card). Pass `kind` (the interaction kind), `values` in the kind's shape, or " +
        "`declined: true` if they refused. The answer is validated server-side; if a value isn't valid " +
        "you'll get an error explaining what to re-ask, then call this tool again with the correction.";

    public override JsonElement JsonSchema => Interactions.SubmitToolSchema(_catalog);

    protected override ValueTask<object?> InvokeCoreAsync(AIFunctionArguments arguments, CancellationToken cancellationToken)
    {
        var args = Interactions.ArgsToObject(arguments);
        var kindId = args["kind"]?.GetValue<string>();
        if (string.IsNullOrEmpty(kindId))
        {
            return new ValueTask<object?>("submit_interaction requires a 'kind'.");
        }

        var kind = _catalog.Get(kindId);
        if (kind is null)
        {
            return new ValueTask<object?>($"unknown interaction kind '{kindId}'.");
        }

        if (args["declined"] is JsonValue declined && declined.TryGetValue<bool>(out var isDeclined) && isDeclined)
        {
            return new ValueTask<object?>(new JsonObject
            {
                ["status"] = "declined",
                ["message"] = "Noted. Continue helping the visitor without this and do not ask again this conversation.",
            }.ToJsonString());
        }

        var spec = _raised.TryGetValue(kindId, out var raised) ? raised : null;
        var result = kind.Validate(spec, args["values"]);
        if (!result.Ok)
        {
            var detail = string.Join("; ", result.Errors!.Select(e => $"{e.Field}: {e.Message}"));
            return new ValueTask<object?>(
                $"validation failed — {detail}. Re-ask the visitor for the corrected value(s) and submit again.");
        }

        // Run the kind's host effect (identity_intake stamps the session contact; choices is a no-op)
        // BEFORE returning the submitted result to the model — the SAME effect the rich frame path runs,
        // so a contact captured conversationally is just as OTP-reachable as one captured via the card.
        if (_effect is not null)
        {
            kind.ApplyEffect(_effect, result.Canonical!);
        }

        return new ValueTask<object?>(new JsonObject
        {
            ["status"] = "submitted",
            ["values"] = result.Canonical!.DeepClone(),
        }.ToJsonString());
    }
}

/// <summary>Shared helpers for the interaction tools.</summary>
internal static class Interactions
{
    /// <summary>Project <see cref="AIFunctionArguments"/> (values arrive as <see cref="JsonElement"/>
    /// from the engine, or CLR objects from a scripted client) into a <see cref="JsonObject"/>.</summary>
    public static JsonObject ArgsToObject(AIFunctionArguments arguments)
    {
        var obj = new JsonObject();
        foreach (var (key, value) in arguments)
        {
            obj[key] = ToNode(value);
        }
        return obj;
    }

    private static JsonNode? ToNode(object? value) => value switch
    {
        null => null,
        JsonNode node => node.DeepClone(),
        JsonElement element => JsonSerializer.SerializeToNode(element),
        _ => JsonSerializer.SerializeToNode(value),
    };

    /// <summary>The <c>submit_interaction</c> tool schema: <c>kind</c> (enum of hosted kinds), an opaque
    /// <c>values</c> object, and a <c>declined</c> flag.</summary>
    public static JsonElement SubmitToolSchema(InteractionCatalog catalog)
    {
        var enumArray = new JsonArray();
        foreach (var kind in catalog.Kinds)
        {
            enumArray.Add(kind.Kind);
        }
        var schema = new JsonObject
        {
            ["type"] = "object",
            ["properties"] = new JsonObject
            {
                ["kind"] = new JsonObject { ["type"] = "string", ["enum"] = enumArray, ["description"] = "The interaction kind being submitted." },
                ["values"] = new JsonObject { ["type"] = "object", ["description"] = "The visitor's answer, in the kind's values shape." },
                ["declined"] = new JsonObject { ["type"] = "boolean", ["description"] = "True if the visitor refused to answer." },
            },
            ["required"] = new JsonArray { "kind" },
        };
        return JsonSerializer.Deserialize<JsonElement>(schema.ToJsonString());
    }
}
