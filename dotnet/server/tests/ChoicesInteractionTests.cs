using System.Text.Json;
using System.Text.Json.Nodes;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// Unit tests for the <c>choices</c> Rich Interaction kind — the validator, the raise-arg parser, and
/// the fallback directive. Ports the Rust reference's <c>choices.rs</c> <c>#[cfg(test)]</c> block, and
/// additionally ties the C# validator to the SHARED conformance fixtures (<c>choices_spec</c> /
/// <c>choices_values</c> / <c>choices_payload</c>) so the same inputs the Rust server validates against
/// produce the same canonical payload here.
/// </summary>
public sealed class ChoicesInteractionTests
{
    private static ChoiceQuestion Question(string header, string[] labels, bool multi) => new()
    {
        Question = $"{header}?",
        Header = header,
        Options = labels.Select(l => new ChoiceOption { Label = l }).ToList(),
        MultiSelect = multi,
    };

    private static ChoiceValues Values(params ChoiceAnswer[] answers) => new() { Answers = answers.ToList() };

    private static ChoiceAnswer Answer(string header, string[] options, string? other = null) => new()
    {
        Header = header,
        Options = options.ToList(),
        Other = other,
    };

    [Fact]
    public void ValidSingleSelectNormalizes()
    {
        var (validated, errors) = ChoicesValidator.Validate(
            new[] { Question("Plan", new[] { "Basic", "Pro" }, false) },
            Values(Answer("Plan", new[] { "  Pro  " })));

        Assert.Null(errors);
        Assert.Single(validated!.Answers);
        Assert.Equal(new[] { "Pro" }, validated.Answers[0].Options);
        Assert.Null(validated.Answers[0].Other);
    }

    [Fact]
    public void ValidMultiSelectKeepsAllPicks()
    {
        var (validated, errors) = ChoicesValidator.Validate(
            new[] { Question("Topics", new[] { "Sales", "Support", "Billing" }, true) },
            Values(Answer("Topics", new[] { "Sales", "Billing" })));

        Assert.Null(errors);
        Assert.Equal(new[] { "Sales", "Billing" }, validated!.Answers[0].Options);
    }

    [Fact]
    public void OtherEscapeHatchIsAccepted()
    {
        var (validated, errors) = ChoicesValidator.Validate(
            new[] { Question("Plan", new[] { "Basic", "Pro" }, false) },
            Values(Answer("Plan", Array.Empty<string>(), "  Enterprise, actually  ")));

        Assert.Null(errors);
        Assert.Empty(validated!.Answers[0].Options);
        Assert.Equal("Enterprise, actually", validated.Answers[0].Other);
    }

    [Fact]
    public void UnknownLabelIsAFieldError()
    {
        var (validated, errors) = ChoicesValidator.Validate(
            new[] { Question("Plan", new[] { "Basic", "Pro" }, false) },
            Values(Answer("Plan", new[] { "Platinum" })));

        Assert.Null(validated);
        Assert.Single(errors!);
        Assert.Equal("Plan", errors![0].Field);
        Assert.Contains("not one of the offered", errors[0].Message);
    }

    [Fact]
    public void SingleSelectRejectsMultiplePicks()
    {
        var (_, errors) = ChoicesValidator.Validate(
            new[] { Question("Plan", new[] { "Basic", "Pro" }, false) },
            Values(Answer("Plan", new[] { "Basic", "Pro" })));

        Assert.NotNull(errors);
        Assert.Contains(errors!, e => e.Message.Contains("single answer"));
    }

    [Fact]
    public void UnansweredQuestionIsRequired()
    {
        var (_, errors) = ChoicesValidator.Validate(
            new[] { Question("Plan", new[] { "Basic", "Pro" }, false), Question("Size", new[] { "S", "M" }, false) },
            Values(Answer("Plan", new[] { "Pro" })));

        Assert.NotNull(errors);
        Assert.Single(errors!);
        Assert.Equal("Size", errors![0].Field);
        Assert.Contains("must be answered", errors[0].Message);
    }

    [Fact]
    public void EmptyAnswerNeedsAPickOrOther()
    {
        var (_, errors) = ChoicesValidator.Validate(
            new[] { Question("Plan", new[] { "Basic", "Pro" }, false) },
            Values(Answer("Plan", Array.Empty<string>())));

        Assert.NotNull(errors);
        Assert.Contains(errors!, e => e.Message.Contains("select an option"));
    }

    [Fact]
    public void AllErrorsAccumulateInOnePass()
    {
        // Two questions both wrong: one unanswered, one with a bad label — both must be reported together.
        var (_, errors) = ChoicesValidator.Validate(
            new[] { Question("Plan", new[] { "Basic", "Pro" }, false), Question("Size", new[] { "S", "M" }, false) },
            Values(Answer("Plan", new[] { "Platinum" })));

        Assert.NotNull(errors);
        Assert.Equal(2, errors!.Count);
        Assert.Contains(errors, e => e.Field == "Plan");
        Assert.Contains(errors, e => e.Field == "Size");
    }

    [Fact]
    public void FormatOnlyPathAcceptsAnswersWithoutASpec()
    {
        // Prior-turn fallback: no questions to membership-check against, so any answer with a pick passes.
        var (validated, errors) = ChoicesValidator.Validate(
            Array.Empty<ChoiceQuestion>(),
            Values(Answer("Plan", new[] { "anything the visitor typed" })));

        Assert.Null(errors);
        Assert.Single(validated!.Answers);
    }

    [Fact]
    public void FormatOnlyPathRejectsNoAnswers()
    {
        var (_, errors) = ChoicesValidator.Validate(Array.Empty<ChoiceQuestion>(), Values());

        Assert.NotNull(errors);
        Assert.Contains(errors!, e => e.Field == "answers");
    }

    [Fact]
    public void ParseQuestionsEnforcesTheContract()
    {
        // Happy path with shorthand string options.
        var qs = ChoicesValidator.ParseQuestions(JsonNode.Parse("""
            [ { "question": "Which plan?", "header": "Plan", "options": ["Basic", "Pro"] } ]
            """));
        Assert.Single(qs);
        Assert.Equal("Basic", qs[0].Options[0].Label);
        Assert.False(qs[0].MultiSelect);

        // Too many questions.
        Assert.Throws<InteractionParseException>(() => ChoicesValidator.ParseQuestions(JsonNode.Parse("""
            [ {"question":"q","header":"H0","options":["a","b"]},
              {"question":"q","header":"H1","options":["a","b"]},
              {"question":"q","header":"H2","options":["a","b"]},
              {"question":"q","header":"H3","options":["a","b"]},
              {"question":"q","header":"H4","options":["a","b"]} ]
            """)));

        // Too few options.
        Assert.Throws<InteractionParseException>(() => ChoicesValidator.ParseQuestions(JsonNode.Parse("""
            [ { "question": "q", "header": "H", "options": ["only"] } ]
            """)));

        // Header too long.
        Assert.Throws<InteractionParseException>(() => ChoicesValidator.ParseQuestions(JsonNode.Parse("""
            [ { "question": "q", "header": "ThisHeaderIsWayTooLong", "options": ["a", "b"] } ]
            """)));

        // Duplicate headers.
        Assert.Throws<InteractionParseException>(() => ChoicesValidator.ParseQuestions(JsonNode.Parse("""
            [ { "question": "q1", "header": "H", "options": ["a","b"] },
              { "question": "q2", "header": "H", "options": ["a","b"] } ]
            """)));
    }

    [Fact]
    public void KindWiresTheReferenceSurface()
    {
        IInteractionKind kind = new ChoicesKind();
        Assert.Equal("choices", kind.Kind);
        Assert.Equal("choice_chips", kind.Capability);
        Assert.Equal("request_choices", kind.ToolName);

        var request = kind.ParseRequest(JsonNode.Parse("""
            {
                "questions": [ { "question": "Which plan interests you?", "header": "Plan",
                    "options": [ { "label": "Basic" }, { "label": "Pro" } ] } ],
                "reason": "to route you"
            }
            """)!.AsObject());
        Assert.Equal("choices", request.Kind);
        Assert.Equal("to route you", request.Reason);
        Assert.Equal("Plan", request.Spec["questions"]![0]!["header"]!.GetValue<string>());

        // The validator, through the kind, produces the canonical values.
        var validation = kind.Validate(request.Spec, JsonNode.Parse("""{ "answers": [ { "header": "Plan", "options": ["Pro"] } ] }"""));
        Assert.True(validation.Ok);
        Assert.Equal("Pro", validation.Canonical!["answers"]![0]!["options"]![0]!.GetValue<string>());

        // The fallback directive enumerates the options + names submit_interaction.
        var directive = kind.FallbackDirective(request.Spec, "to route you");
        Assert.Contains("Basic, Pro", directive);
        Assert.Contains("submit_interaction", directive);
    }

    [Fact]
    public void ParseRequestDefaultsBlankReason()
    {
        var request = new ChoicesKind().ParseRequest(JsonNode.Parse("""
            { "questions": [ { "question": "q", "header": "H", "options": ["a","b"] } ] }
            """)!.AsObject());
        Assert.Equal("to help you better", request.Reason);
    }

    // ---- Shared conformance fixtures: the C# validator must agree with the shared spec ----

    [Fact]
    public void ValidatesTheSharedChoicesFixtures()
    {
        var fixtures = LoadFixtures();

        // choices_spec parses into the kind's questions (2 questions, second is multiSelect).
        var spec = fixtures["choices_spec"];
        var questions = ChoicesValidator.QuestionsFromSpec(spec);
        Assert.Equal(2, questions.Count);
        Assert.Equal("Plan", questions[0].Header);
        Assert.False(questions[0].MultiSelect);
        Assert.Equal("Topics", questions[1].Header);
        Assert.True(questions[1].MultiSelect);

        // choices_values, validated against choices_spec, yields exactly choices_payload.values.
        var values = fixtures["choices_values"];
        var validation = new ChoicesKind().Validate(spec, values);
        Assert.True(validation.Ok);

        // Normalize both through the typed model so key order can't cause a spurious mismatch.
        var expected = fixtures["choices_payload"]!["values"]!;
        var opts = ChoicesValidator.SerializerOptions;
        var expectedNorm = JsonSerializer.Serialize(expected.Deserialize<ChoiceValues>(opts), opts);
        var actualNorm = JsonSerializer.Serialize(validation.Canonical!.Deserialize<ChoiceValues>(opts), opts);
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
