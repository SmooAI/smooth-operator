using System.Text.Json;
using System.Text.Json.Nodes;
using System.Text.Json.Serialization;

namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// The <c>identity_intake</c> Rich Interaction kind — channel-normalized name/email/phone lead capture,
/// the second hosted kind (after <see cref="ChoicesKind"/>) and the first to carry a host <b>effect</b>:
/// on a valid submit it stamps the captured contact onto the session so the OTP seam can reach the
/// visitor. On an <c>identity_form</c> channel the client renders a structured form the turn parks on;
/// on text-only channels the raise degrades to a conversational directive and the model submits the
/// collected values via the <c>submit_interaction</c> tool. Both paths validate through
/// <see cref="IdentityIntakeValidator.Validate"/>. A faithful C# port of the Rust reference
/// <c>smooth-operator/src/identity_intake.rs</c>.
/// </summary>
public sealed class IdentityIntakeKind : IInteractionKind
{
    public string Kind => "identity_intake";
    public string Capability => "identity_form";

    public string ToolDescription =>
        "Ask the visitor for their contact details (name, email, and/or phone) in a channel-appropriate " +
        "way. On channels that can render a form the visitor fills a structured form; on text channels " +
        "you will be told to collect the fields conversationally. Always use this tool instead of " +
        "free-forming a request for contact details.";

    private static readonly JsonElement Schema = JsonSerializer.Deserialize<JsonElement>("""
        {
            "type": "object",
            "properties": {
                "fields": {
                    "type": "array",
                    "description": "Which fields to collect, in order. Each entry is either a string (\"name\" | \"email\" | \"phone\") or an object { key, required?, label? }.",
                    "items": {
                        "anyOf": [
                            { "type": "string", "enum": ["name", "email", "phone"] },
                            {
                                "type": "object",
                                "properties": {
                                    "key": { "type": "string", "enum": ["name", "email", "phone"] },
                                    "required": { "type": "boolean" },
                                    "label": { "type": "string" }
                                },
                                "required": ["key"]
                            }
                        ]
                    }
                },
                "reason": {
                    "type": "string",
                    "description": "Why you need these details, phrased for the visitor (e.g. \"to send you the quote\")."
                }
            },
            "required": ["fields", "reason"]
        }
        """);

    public JsonElement ToolSchema => Schema;

    public InteractionRequest ParseRequest(JsonObject args)
    {
        var fields = IdentityIntakeValidator.ParseFields(args["fields"]);
        var reason = args["reason"].Str()?.Trim() is { Length: > 0 } r ? r : "to help you better";
        var spec = new JsonObject { ["fields"] = JsonSerializer.SerializeToNode(fields, IdentityIntakeValidator.SerializerOptions) };
        return new InteractionRequest(Kind, spec, reason);
    }

    public InteractionValidation Validate(JsonNode? spec, JsonNode? values)
    {
        // The spec's fields drive required-ness; a missing/garbled spec (a prior-turn fallback whose
        // spec is gone) degrades to format-only validation — mirrors Rust's unwrap_or_default.
        var fields = IdentityIntakeValidator.FieldsFromSpec(spec);

        IntakeValues parsed;
        try
        {
            parsed = values is null
                ? throw new JsonException("values is required")
                : values.Deserialize<IntakeValues>(IdentityIntakeValidator.SerializerOptions) ?? throw new JsonException("values is null");
        }
        catch (JsonException e)
        {
            return InteractionValidation.Invalid(new[] { new InteractionFieldError("values", $"invalid values shape: {e.Message}") });
        }

        if (parsed.IsEmpty)
        {
            return InteractionValidation.Invalid(new[]
            {
                new InteractionFieldError("values", "provide at least one of name/email/phone, or declined=true"),
            });
        }

        var (validated, errors) = IdentityIntakeValidator.Validate(fields, parsed);
        if (errors is not null)
        {
            return InteractionValidation.Invalid(errors);
        }
        return InteractionValidation.Valid(JsonSerializer.SerializeToNode(validated, IdentityIntakeValidator.SerializerOptions)!);
    }

    public string FallbackDirective(JsonNode? spec, string reason)
    {
        var fieldList = spec?["fields"] is JsonArray fields
            ? string.Join(", ", fields.Where(f => f?["key"] is not null).Select(f => f!["key"].Str()))
            : string.Empty;
        return
            "This visitor's channel cannot display a form. Collect the requested details " +
            $"({fieldList}) conversationally: ask for ONE field at a time, in the order given, " +
            $"naturally weaving in the reason ({reason}). When you have the values, call the " +
            "`submit_interaction` tool with kind \"identity_intake\" and the values — it validates each " +
            "field and will tell you if something looks wrong so you can re-ask. If the visitor declines " +
            "to share, call `submit_interaction` with declined=true and continue helping them without the details.";
    }

    /// <summary>
    /// The host effect of an accepted <c>identity_intake</c> submit: stamp the validated contact onto
    /// the session (the same contact the OTP seam reads), so a captured email/phone is immediately
    /// OTP-reachable. Only non-null fields are written (an intake that collected just an email never
    /// clobbers a known name). The C# analog of Rust's <c>attach_interaction_effect</c> →
    /// <c>attach_session_identity</c>; choices has no effect (inherits the interface's no-op default).
    /// </summary>
    public void ApplyEffect(IInteractionEffectContext context, JsonNode canonicalValues)
    {
        if (canonicalValues is not JsonObject obj)
        {
            return;
        }
        context.AttachSessionIdentity(
            name: obj["name"].Str(),
            email: obj["email"].Str(),
            phone: obj["phone"].Str());
    }
}

/// <summary>The closed set of identity fields intake can collect (name / email / phone).</summary>
public sealed class IntakeField
{
    [JsonPropertyName("key")] public string Key { get; set; } = string.Empty;
    [JsonPropertyName("required")] public bool Required { get; set; }

    [JsonPropertyName("label")]
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Label { get; set; }
}

/// <summary>Validated, normalized identity values — the payload the parked turn resumes with (identical
/// on the form and conversational paths). Absent fields serialize away, matching serde's skip.</summary>
public sealed class IntakeValues
{
    [JsonPropertyName("name")]
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Name { get; set; }

    [JsonPropertyName("email")]
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Email { get; set; }

    [JsonPropertyName("phone")]
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Phone { get; set; }

    /// <summary>True when no field carries a value.</summary>
    [JsonIgnore]
    public bool IsEmpty => Name is null && Email is null && Phone is null;
}

/// <summary>
/// The <c>identity_intake</c> validator + raise-arg parser — a faithful port of <c>validate_intake</c>,
/// <c>normalize_email</c>, <c>normalize_phone_e164</c>, and <c>parse_fields</c> from the Rust reference.
/// Kept separate from <see cref="IdentityIntakeKind"/> so it is unit-testable in isolation (like the
/// Rust module's <c>#[cfg(test)]</c> block).
/// </summary>
public static class IdentityIntakeValidator
{
    private static readonly string[] KnownKeys = { "name", "email", "phone" };

    /// <summary>Serializer options for the intake wire types — property names are explicit; nulls skip
    /// per each property's own attribute.</summary>
    public static readonly JsonSerializerOptions SerializerOptions = new();

    /// <summary>Deserialize a spec's <c>fields</c> into the typed list; empty on a missing/garbled spec
    /// (⇒ format-only validation), mirroring Rust's <c>unwrap_or_default</c>.</summary>
    public static List<IntakeField> FieldsFromSpec(JsonNode? spec)
    {
        if (spec?["fields"] is not JsonNode fields)
        {
            return new List<IntakeField>();
        }
        try
        {
            return fields.Deserialize<List<IntakeField>>(SerializerOptions) ?? new List<IntakeField>();
        }
        catch (JsonException)
        {
            return new List<IntakeField>();
        }
    }

    /// <summary>
    /// Validate raw submitted <paramref name="values"/> against the requested <paramref name="fields"/>,
    /// returning the normalized values or EVERY per-field error (one-pass). Rules: every required field
    /// present + non-blank; name non-empty after trim; email <c>local@domain.tld</c> shape; phone → E.164.
    /// Fields not requested but present are still validated and kept. Port of Rust's <c>validate_intake</c>.
    /// </summary>
    public static (IntakeValues? Validated, IReadOnlyList<InteractionFieldError>? Errors) Validate(
        IReadOnlyList<IntakeField> fields,
        IntakeValues values)
    {
        var errors = new List<InteractionFieldError>();
        var outValues = new IntakeValues();

        string? Get(string key) => key switch
        {
            "name" => values.Name,
            "email" => values.Email,
            "phone" => values.Phone,
            _ => null,
        };

        // Required-ness: every required requested field must be present + non-blank.
        foreach (var field in fields)
        {
            if (field.Required && string.IsNullOrWhiteSpace(Get(field.Key)))
            {
                errors.Add(new InteractionFieldError(field.Key, "this field is required"));
            }
        }

        // Format validation + normalization for whatever was provided.
        if (values.Name?.Trim() is { Length: > 0 } name)
        {
            outValues.Name = name;
        }
        if (values.Email?.Trim() is { Length: > 0 } email)
        {
            var normalized = NormalizeEmail(email);
            if (normalized is null)
            {
                errors.Add(new InteractionFieldError("email", "must be a valid email address"));
            }
            else
            {
                outValues.Email = normalized;
            }
        }
        if (values.Phone?.Trim() is { Length: > 0 } phone)
        {
            var normalized = NormalizePhoneE164(phone);
            if (normalized is null)
            {
                errors.Add(new InteractionFieldError("phone", "must be a valid phone number (include your country code, e.g. +1 555 123 4567)"));
            }
            else
            {
                outValues.Phone = normalized;
            }
        }

        return errors.Count == 0 ? (outValues, null) : (null, errors);
    }

    /// <summary>Minimal email-shape validation: exactly one <c>@</c>, non-empty local part, a
    /// dot-containing domain, no whitespace. Returns the trimmed address with a lowercased domain, or
    /// <c>null</c> when malformed. Shape check, not RFC 5322 — deliverability is the host's job.</summary>
    public static string? NormalizeEmail(string raw)
    {
        var s = raw.Trim();
        if (s.Any(char.IsWhiteSpace))
        {
            return null;
        }
        var at = s.IndexOf('@');
        if (at <= 0)
        {
            return null; // no '@', or empty local part
        }
        var local = s[..at];
        var domain = s[(at + 1)..];
        if (domain.Contains('@'))
        {
            return null; // more than one '@'
        }
        var domainLc = domain.ToLowerInvariant();
        var parts = domainLc.Split('.');
        // Domain needs an interior dot: `a.b`, not `.b`, `a.`, or `ab`.
        if (parts.Length < 2 || parts.Any(string.IsNullOrEmpty))
        {
            return null;
        }
        return $"{local}@{domainLc}";
    }

    /// <summary>Normalize a phone number to E.164, or <c>null</c> when unparseable. Strips common
    /// separators, then accepts <c>+</c> + 8–15 digits (already E.164), or a bare 10-digit / 1-prefixed
    /// 11-digit NANP number → <c>+1…</c>. Port of Rust's <c>normalize_phone_e164</c>.</summary>
    public static string? NormalizePhoneE164(string raw)
    {
        var s = new string(raw.Trim().Where(c => c is not (' ' or '-' or '.' or '(' or ')')).ToArray());
        var plus = s.StartsWith('+');
        var digits = plus ? s[1..] : s;
        if (digits.Length == 0 || !digits.All(char.IsAsciiDigit))
        {
            return null;
        }
        if (plus)
        {
            // E.164: country code can't start with 0; total 8–15 digits.
            if (digits.Length is >= 8 and <= 15 && !digits.StartsWith('0'))
            {
                return $"+{digits}";
            }
            return null;
        }
        return digits.Length switch
        {
            10 => $"+1{digits}",
            11 when digits.StartsWith('1') => $"+{digits}",
            _ => null,
        };
    }

    /// <summary>
    /// Parse the raise tool's <c>fields</c> argument. Accepts the structured form (<c>{ key, required?,
    /// label? }</c>) and the bare-string shorthand (<c>"email"</c> ⇒ <c>required: true</c>). Unknown keys
    /// are an error (closed set). Throws <see cref="InteractionParseException"/> — a port of Rust's
    /// <c>parse_fields</c>.
    /// </summary>
    public static List<IntakeField> ParseFields(JsonNode? raw)
    {
        if (raw is not JsonArray items)
        {
            throw new InteractionParseException("'fields' must be an array");
        }
        if (items.Count == 0)
        {
            throw new InteractionParseException("'fields' must contain at least one field");
        }

        static string ParseKey(string s) => Array.IndexOf(KnownKeys, s) >= 0
            ? s
            : throw new InteractionParseException($"unknown intake field '{s}' (expected name, email, or phone)");

        var fields = new List<IntakeField>(items.Count);
        foreach (var item in items)
        {
            switch (item)
            {
                case JsonValue v when v.TryGetValue<string>(out var s):
                    fields.Add(new IntakeField { Key = ParseKey(s), Required = true });
                    break;
                case JsonObject obj:
                    var key = obj["key"].Str()
                        ?? throw new InteractionParseException("each field object needs a string 'key'");
                    fields.Add(new IntakeField
                    {
                        Key = ParseKey(key),
                        Required = obj["required"].Bool() ?? true,
                        Label = obj["label"].Str(),
                    });
                    break;
                default:
                    throw new InteractionParseException("invalid field entry");
            }
        }
        return fields;
    }
}
