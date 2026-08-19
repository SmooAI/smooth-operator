using System.Text.Json;
using System.Text.Json.Nodes;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// Unit tests for the <c>identity_intake</c> Rich Interaction kind — the validator (required/optional,
/// email shape, phone → E.164, one-pass errors), the raise-arg parser, and the host effect. Ports the
/// Rust reference's <c>identity_intake.rs</c> <c>#[cfg(test)]</c> block, and ties the C# validator to the
/// SHARED conformance fixtures (<c>identity_intake_spec</c> / <c>_values</c> / <c>_payload</c>) so the
/// same inputs the Rust server validates against produce the same canonical payload here.
/// </summary>
public sealed class IdentityIntakeInteractionTests
{
    private static IntakeField Field(string key, bool required) => new() { Key = key, Required = required };

    private static IntakeValues Values(string? name = null, string? email = null, string? phone = null) =>
        new() { Name = name, Email = email, Phone = phone };

    // ---- Email shape (port of Rust email_shapes) ----

    [Fact]
    public void Email_ValidLowercasesDomainKeepsLocalCase()
    {
        Assert.Equal("Alice@example.com", IdentityIntakeValidator.NormalizeEmail("Alice@Example.COM"));
    }

    [Theory]
    [InlineData("")]
    [InlineData("no-at")]
    [InlineData("@x.com")]
    [InlineData("a@b")]
    [InlineData("a@.com")]
    [InlineData("a@b.")]
    [InlineData("a b@c.com")]
    [InlineData("a@b@c.com")]
    public void Email_MalformedRejected(string bad)
    {
        Assert.Null(IdentityIntakeValidator.NormalizeEmail(bad));
    }

    // ---- Phone E.164 (port of Rust phone_shapes) ----

    [Theory]
    [InlineData("+1 (555) 123-4567", "+15551234567")]
    [InlineData("555.123.4567", "+15551234567")]        // bare 10-digit NANP
    [InlineData("1 555 123 4567", "+15551234567")]      // 1-prefixed 11-digit NANP
    [InlineData("+447911123456", "+447911123456")]      // non-NANP with country code
    public void Phone_NormalizesToE164(string raw, string expected)
    {
        Assert.Equal(expected, IdentityIntakeValidator.NormalizePhoneE164(raw));
    }

    [Theory]
    [InlineData("")]
    [InlineData("abc")]
    [InlineData("+0123456789")]
    [InlineData("12345")]
    [InlineData("+1234567890123456")]
    public void Phone_MalformedRejected(string bad)
    {
        Assert.Null(IdentityIntakeValidator.NormalizePhoneE164(bad));
    }

    // ---- Validator (port of Rust validate_intake tests) ----

    [Fact]
    public void RequiredFieldMissingIsAnError()
    {
        var fields = new[] { Field("email", true), Field("name", false) };
        var (validated, errors) = IdentityIntakeValidator.Validate(fields, Values());
        Assert.Null(validated);
        Assert.Single(errors!);
        Assert.Equal("email", errors![0].Field);

        // Blank counts as missing.
        var (_, blankErrors) = IdentityIntakeValidator.Validate(fields, Values(email: "   "));
        Assert.NotNull(blankErrors);
    }

    [Fact]
    public void OptionalFieldMayBeOmitted()
    {
        var fields = new[] { Field("name", true), Field("phone", false) };
        var (validated, errors) = IdentityIntakeValidator.Validate(fields, Values(name: "Alice"));
        Assert.Null(errors);
        Assert.Equal("Alice", validated!.Name);
        Assert.Null(validated.Phone);
    }

    [Fact]
    public void ValidSubmitNormalizes()
    {
        var fields = new[] { Field("email", true), Field("phone", false) };
        var (validated, errors) = IdentityIntakeValidator.Validate(
            fields,
            Values(name: "  Alice Example  ", email: "alice@Example.com", phone: "(555) 123-4567"));

        Assert.Null(errors);
        Assert.Equal("Alice Example", validated!.Name);
        Assert.Equal("alice@example.com", validated.Email);
        Assert.Equal("+15551234567", validated.Phone);
    }

    [Fact]
    public void BadEmailProducesFieldError()
    {
        var fields = new[] { Field("email", true) };
        var (validated, errors) = IdentityIntakeValidator.Validate(fields, Values(email: "not-an-email"));
        Assert.Null(validated);
        Assert.Single(errors!);
        Assert.Equal("email", errors![0].Field);
    }

    [Fact]
    public void AllErrorsReportedInOnePass()
    {
        var fields = new[] { Field("name", true) };
        var (_, errors) = IdentityIntakeValidator.Validate(
            fields,
            Values(email: "not-an-email", phone: "nope"));
        // missing name + bad email + bad phone.
        Assert.Equal(3, errors!.Count);
    }

    [Fact]
    public void VolunteeredFieldsAreKept()
    {
        // Only email requested, but the visitor volunteered a phone — keep it.
        var fields = new[] { Field("email", true) };
        var (validated, errors) = IdentityIntakeValidator.Validate(
            fields,
            Values(email: "a@b.co", phone: "+15551234567"));
        Assert.Null(errors);
        Assert.Equal("+15551234567", validated!.Phone);
    }

    // ---- Kind-level: empty values, spec-less format-only, decline surface ----

    [Fact]
    public void EmptyValuesIsInvalid()
    {
        var result = new IdentityIntakeKind().Validate(
            JsonNode.Parse("""{ "fields": [ { "key": "email", "required": true } ] }"""),
            JsonNode.Parse("{}"));
        Assert.False(result.Ok);
        Assert.Equal("values", result.Errors![0].Field);
    }

    [Fact]
    public void MissingSpecDegradesToFormatOnly()
    {
        // No spec (a prior-turn fallback whose spec is gone): required-ness can't be enforced, but
        // format is still checked and provided values are kept/normalized.
        var result = new IdentityIntakeKind().Validate(
            spec: null,
            values: JsonNode.Parse("""{ "email": "a@b.co" }"""));
        Assert.True(result.Ok);
        Assert.Equal("a@b.co", result.Canonical!["email"]!.GetValue<string>());
    }

    // ---- Raise-arg parser (port of Rust parse_fields) ----

    [Fact]
    public void ParseFields_ShorthandStringsAreRequired()
    {
        var fields = IdentityIntakeValidator.ParseFields(JsonNode.Parse("""["email", "name"]"""));
        Assert.Equal(2, fields.Count);
        Assert.Equal("email", fields[0].Key);
        Assert.True(fields[0].Required);
    }

    [Fact]
    public void ParseFields_ObjectFormHonorsRequiredAndLabel()
    {
        var fields = IdentityIntakeValidator.ParseFields(
            JsonNode.Parse("""[{ "key": "phone", "required": false, "label": "Mobile" }]"""));
        Assert.Single(fields);
        Assert.Equal("phone", fields[0].Key);
        Assert.False(fields[0].Required);
        Assert.Equal("Mobile", fields[0].Label);
    }

    [Fact]
    public void ParseFields_UnknownKeyThrows()
    {
        Assert.Throws<InteractionParseException>(() =>
            IdentityIntakeValidator.ParseFields(JsonNode.Parse("""["fax"]""")));
    }

    [Fact]
    public void ParseFields_EmptyArrayThrows()
    {
        Assert.Throws<InteractionParseException>(() =>
            IdentityIntakeValidator.ParseFields(JsonNode.Parse("[]")));
    }

    // ---- Host effect: identity_intake stamps the session contact; choices does not ----

    private sealed class RecordingEffectContext : IInteractionEffectContext
    {
        public string SessionId => "s-1";
        public (string? Name, string? Email, string? Phone)? Attached { get; private set; }
        public void AttachSessionIdentity(string? name, string? email, string? phone) => Attached = (name, email, phone);
    }

    [Fact]
    public void ApplyEffect_StampsCapturedContact()
    {
        var ctx = new RecordingEffectContext();
        new IdentityIntakeKind().ApplyEffect(ctx, JsonNode.Parse("""{ "name": "Alice", "email": "a@b.co", "phone": "+15551234567" }""")!);
        Assert.Equal(("Alice", "a@b.co", "+15551234567"), ctx.Attached);
    }

    [Fact]
    public void ChoicesHasNoEffect()
    {
        var ctx = new RecordingEffectContext();
        // choices inherits the interface's no-op default — nothing is stamped.
        ((IInteractionKind)new ChoicesKind()).ApplyEffect(ctx, JsonNode.Parse("""{ "answers": [] }""")!);
        Assert.Null(ctx.Attached);
    }

    [Fact]
    public void SessionIdentityRegistry_MergesWithoutClobbering()
    {
        var registry = new SessionIdentityRegistry();
        registry.Attach("s", name: "Alice", email: "a@b.co", phone: null);
        // A later intake that collected only a phone must not wipe the known name/email.
        registry.Attach("s", name: null, email: null, phone: "+15551234567");
        var identity = registry.Get("s");
        Assert.Equal("Alice", identity!.Name);
        Assert.Equal("a@b.co", identity.Email);
        Assert.Equal("+15551234567", identity.Phone);
    }

    // ---- Shared conformance fixtures: the C# validator must agree with the shared spec ----

    [Fact]
    public void ValidatesTheSharedIdentityIntakeFixtures()
    {
        var fixtures = LoadFixtures();

        // identity_intake_spec parses into the kind's fields (name optional, email required, phone optional).
        var spec = fixtures["identity_intake_spec"];
        var fields = IdentityIntakeValidator.FieldsFromSpec(spec);
        Assert.Equal(3, fields.Count);
        Assert.Equal("email", fields[1].Key);
        Assert.True(fields[1].Required);
        Assert.Equal("Work email", fields[1].Label);

        // identity_intake_values, validated against the spec, yields exactly identity_intake_payload.values.
        var values = fixtures["identity_intake_values"];
        var validation = new IdentityIntakeKind().Validate(spec, values);
        Assert.True(validation.Ok);

        var expected = fixtures["identity_intake_payload"]!["values"]!;
        var opts = IdentityIntakeValidator.SerializerOptions;
        var expectedNorm = JsonSerializer.Serialize(expected.Deserialize<IntakeValues>(opts), opts);
        var actualNorm = JsonSerializer.Serialize(validation.Canonical!.Deserialize<IntakeValues>(opts), opts);
        Assert.Equal(expectedNorm, actualNorm);
    }

    /// <summary>Load the <c>instance</c> of every fixture in the shared <c>spec/conformance/fixtures.json</c>.</summary>
    private static Dictionary<string, JsonNode> LoadFixtures()
    {
        var path = FindUp(Path.Combine("spec", "conformance", "fixtures.json"))
            ?? throw new FileNotFoundException("could not locate spec/conformance/fixtures.json above " + AppContext.BaseDirectory);
        var root = JsonNode.Parse(File.ReadAllText(path))!.AsObject();
        var fixtures = new Dictionary<string, JsonNode>();
        foreach (var (name, node) in root)
        {
            if (name.StartsWith('$') || node?["instance"] is not JsonNode instance)
            {
                continue;
            }
            fixtures[name] = instance;
        }
        return fixtures;
    }

    private static string? FindUp(string relative)
    {
        var dir = AppContext.BaseDirectory;
        while (dir is not null)
        {
            var candidate = Path.Combine(dir, relative);
            if (File.Exists(candidate))
            {
                return candidate;
            }
            dir = Path.GetDirectoryName(dir.TrimEnd(Path.DirectorySeparatorChar));
        }
        return null;
    }
}
