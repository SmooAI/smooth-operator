using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Text.Json.Serialization;

namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// The <c>choices</c> Rich Interaction kind — a structured multiple-choice ask modeled on Claude Code's
/// <c>AskUserQuestion</c>: 1–4 short questions, each with 2–4 labeled options, that the turn parks on
/// until the visitor picks. Every question also carries an implicit free-text "Other" escape hatch.
/// On a <c>choice_chips</c> channel the client renders chips; on text-only channels the raise degrades
/// to a conversational directive. Both paths validate through <see cref="ChoicesValidator.Validate"/>.
/// A faithful C# port of the Rust reference <c>smooth-operator/src/choices.rs</c>.
/// </summary>
public sealed class ChoicesKind : IInteractionKind
{
    /// <summary>Max length of a question's short <c>header</c> label (chip/tab caption).</summary>
    public const int HeaderMaxChars = 12;

    public string Kind => "choices";
    public string Capability => "choice_chips";

    public string ToolDescription =>
        "Ask the visitor a structured multiple-choice question (1–4 questions, each with 2–4 labeled " +
        "options) and wait for their pick. On channels that can render chips/menus the visitor taps an " +
        "option; on text channels you will be told to enumerate the options and accept a natural-language " +
        "answer. An implicit free-text \"Other\" is always available, so use this whenever the answer is " +
        "likely (but not certainly) one of a small set — never free-form the menu yourself.";

    private static readonly JsonElement Schema = JsonSerializer.Deserialize<JsonElement>($$"""
        {
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 4,
                    "description": "The questions to ask, in order (1–4).",
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": { "type": "string", "description": "The question prompt shown to the visitor." },
                            "header": { "type": "string", "maxLength": {{HeaderMaxChars}}, "description": "A short label (≤12 chars), unique within the raise. Used as the answer key and the chip/tab caption." },
                            "options": {
                                "type": "array",
                                "minItems": 2,
                                "maxItems": 4,
                                "description": "The 2–4 options to offer. A free-text 'Other' is always available in addition.",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": { "type": "string", "description": "The option label (the value submitted)." },
                                        "description": { "type": "string", "description": "A short gloss for the option." }
                                    },
                                    "required": ["label"]
                                }
                            },
                            "multiSelect": { "type": "boolean", "description": "Allow selecting more than one option (default false)." }
                        },
                        "required": ["question", "header", "options"]
                    }
                },
                "reason": {
                    "type": "string",
                    "description": "Why you're asking, phrased for the visitor (e.g. \"to route you to the right team\")."
                }
            },
            "required": ["questions", "reason"]
        }
        """);

    public JsonElement ToolSchema => Schema;

    public InteractionRequest ParseRequest(JsonObject args)
    {
        var questions = ChoicesValidator.ParseQuestions(args["questions"]);
        var reason = args["reason"]?.GetValue<string>()?.Trim() is { Length: > 0 } r ? r : "to help you better";
        var spec = new JsonObject { ["questions"] = JsonSerializer.SerializeToNode(questions, ChoicesValidator.SerializerOptions) };
        return new InteractionRequest(Kind, spec, reason);
    }

    public InteractionValidation Validate(JsonNode? spec, JsonNode? values)
    {
        // Missing/garbled spec ⇒ format-only validation (a prior-turn fallback whose spec is gone).
        var questions = ChoicesValidator.QuestionsFromSpec(spec);

        ChoiceValues parsed;
        try
        {
            parsed = values is null ? throw new JsonException("values is required") : values.Deserialize<ChoiceValues>(ChoicesValidator.SerializerOptions) ?? throw new JsonException("values is null");
        }
        catch (JsonException e)
        {
            return InteractionValidation.Invalid(new[] { new InteractionFieldError("values", $"invalid values shape: {e.Message}") });
        }

        var (validated, errors) = ChoicesValidator.Validate(questions, parsed);
        if (errors is not null)
        {
            return InteractionValidation.Invalid(errors);
        }
        return InteractionValidation.Valid(JsonSerializer.SerializeToNode(validated, ChoicesValidator.SerializerOptions)!);
    }

    public string FallbackDirective(JsonNode? spec, string reason)
    {
        var lines = new List<string>();
        if (spec?["questions"] is JsonArray questions)
        {
            foreach (var q in questions)
            {
                if (q?["question"]?.GetValue<string>() is not { } question)
                {
                    continue;
                }
                var header = q["header"]?.GetValue<string>() ?? question;
                var multi = q["multiSelect"]?.GetValue<bool>() ?? false;
                var labels = q["options"] is JsonArray opts
                    ? string.Join(", ", opts.Where(o => o?["label"] is not null).Select(o => o!["label"]!.GetValue<string>()))
                    : string.Empty;
                lines.Add($"- [{header}] {question} Options: {labels}{(multi ? " (choose one or more)" : string.Empty)}.");
            }
        }
        var enumerated = string.Join("\n", lines);
        return
            "This visitor's channel cannot display choice chips. Ask the following question(s) " +
            "conversationally, naturally weaving in the reason (" + reason + "), and read out each " +
            "option so the visitor can pick:\n" + enumerated + "\nThe visitor may also answer with " +
            "something not listed (that's fine — capture it as their 'other' answer). When you " +
            "have their pick(s), call the `submit_interaction` tool with kind \"choices\" and " +
            "`values.answers` — one entry per question `{ header, options: [chosen label(s)], " +
            "other?: \"their free-text answer\" }`. It validates each answer and will tell you " +
            "if a pick isn't offered so you can re-ask. If the visitor declines to choose, call " +
            "`submit_interaction` with declined=true and continue helping them.";
    }
}

/// <summary>One selectable option in a question.</summary>
public sealed class ChoiceOption
{
    [JsonPropertyName("label")] public string Label { get; set; } = string.Empty;
    [JsonPropertyName("description")] public string Description { get; set; } = string.Empty;
}

/// <summary>One question in a <c>choices</c> raise.</summary>
public sealed class ChoiceQuestion
{
    [JsonPropertyName("question")] public string Question { get; set; } = string.Empty;
    [JsonPropertyName("header")] public string Header { get; set; } = string.Empty;
    [JsonPropertyName("options")] public List<ChoiceOption> Options { get; set; } = new();
    [JsonPropertyName("multiSelect")] public bool MultiSelect { get; set; }
}

/// <summary>The visitor's answer to one question, submitted via <c>submit_interaction</c>.</summary>
public sealed class ChoiceAnswer
{
    [JsonPropertyName("header")] public string Header { get; set; } = string.Empty;
    [JsonPropertyName("options")] public List<string> Options { get; set; } = new();

    // Blank ⇒ omitted (the free-text "Other" escape hatch), matching serde's skip_serializing_if.
    [JsonPropertyName("other")]
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Other { get; set; }

    /// <summary>Total picks: selected labels + one for a non-blank "Other".</summary>
    public int SelectionCount() => Options.Count + (Other is null ? 0 : 1);
}

/// <summary>Validated, normalized choice answers — the payload the parked turn resumes with.</summary>
public sealed class ChoiceValues
{
    [JsonPropertyName("answers")] public List<ChoiceAnswer> Answers { get; set; } = new();
}

/// <summary>
/// The <c>choices</c> validator + raise-arg parser — a faithful port of <c>validate_choices</c> and
/// <c>parse_questions</c> from the Rust reference. Kept separate from <see cref="ChoicesKind"/> so it is
/// unit-testable in isolation (like the Rust module's <c>#[cfg(test)]</c> block).
/// </summary>
public static class ChoicesValidator
{
    /// <summary>Serializer options for the choice wire types — no camelCase munging (the property names
    /// are explicit) and no null-skipping beyond <see cref="ChoiceAnswer.Other"/>'s own attribute.</summary>
    public static readonly JsonSerializerOptions SerializerOptions = new();

    /// <summary>Deserialize a spec's <c>questions</c> into the typed list; empty on a missing/garbled
    /// spec (⇒ format-only validation), mirroring Rust's <c>unwrap_or_default</c>.</summary>
    public static List<ChoiceQuestion> QuestionsFromSpec(JsonNode? spec)
    {
        if (spec?["questions"] is not JsonNode questions)
        {
            return new List<ChoiceQuestion>();
        }
        try
        {
            return questions.Deserialize<List<ChoiceQuestion>>(SerializerOptions) ?? new List<ChoiceQuestion>();
        }
        catch (JsonException)
        {
            return new List<ChoiceQuestion>();
        }
    }

    /// <summary>
    /// Validate submitted <paramref name="values"/> against the raised <paramref name="questions"/>.
    /// Returns <c>(validated, null)</c> on success or <c>(null, errors)</c> with EVERY failed question
    /// (one-pass), a line-for-line port of Rust's <c>validate_choices</c>.
    /// </summary>
    public static (ChoiceValues? Validated, IReadOnlyList<InteractionFieldError>? Errors) Validate(
        IReadOnlyList<ChoiceQuestion> questions,
        ChoiceValues values)
    {
        // Normalize first: trim headers + labels (drop blanks), trim "Other" (blank ⇒ absent).
        var normalized = values.Answers.Select(a => new ChoiceAnswer
        {
            Header = a.Header.Trim(),
            Options = a.Options.Select(o => o.Trim()).Where(o => o.Length > 0).ToList(),
            Other = a.Other?.Trim() is { Length: > 0 } t ? t : null,
        }).ToList();

        // Format-only path: no spec to check membership/required-ness against.
        if (questions.Count == 0)
        {
            var fmtErrors = new List<InteractionFieldError>();
            foreach (var answer in normalized)
            {
                if (answer.SelectionCount() == 0)
                {
                    fmtErrors.Add(new InteractionFieldError(answer.Header, "select an option or provide an 'other' answer"));
                }
            }
            if (normalized.Count == 0)
            {
                fmtErrors.Add(new InteractionFieldError("answers", "provide an answer for each question, or declined=true"));
            }
            return fmtErrors.Count == 0
                ? (new ChoiceValues { Answers = normalized }, null)
                : (null, fmtErrors);
        }

        var errors = new List<InteractionFieldError>();
        var outAnswers = new List<ChoiceAnswer>(questions.Count);

        foreach (var question in questions)
        {
            var answer = normalized.FirstOrDefault(a => a.Header == question.Header);
            if (answer is null)
            {
                errors.Add(new InteractionFieldError(question.Header, "this question must be answered"));
                continue;
            }

            // Every selected label must be one of the enumerated options.
            var badLabel = false;
            foreach (var label in answer.Options)
            {
                if (!question.Options.Any(o => o.Label == label))
                {
                    badLabel = true;
                    errors.Add(new InteractionFieldError(question.Header, $"'{label}' is not one of the offered options"));
                }
            }

            var count = answer.SelectionCount();
            if (count == 0)
            {
                errors.Add(new InteractionFieldError(question.Header, "select an option or provide an 'other' answer"));
            }
            else if (!question.MultiSelect && count > 1)
            {
                errors.Add(new InteractionFieldError(question.Header, "this question takes a single answer"));
            }

            if (!badLabel)
            {
                outAnswers.Add(answer);
            }
        }

        return errors.Count == 0 ? (new ChoiceValues { Answers = outAnswers }, null) : (null, errors);
    }

    /// <summary>
    /// Parse the raise tool's <c>questions</c> argument into validated <see cref="ChoiceQuestion"/>s,
    /// enforcing the LLM-facing contract (1–4 questions; non-empty prompt; non-empty unique header ≤12
    /// chars; 2–4 options with non-empty labels). Accepts a bare-string option shorthand. Throws
    /// <see cref="InteractionParseException"/> on a violation — a port of Rust's <c>parse_questions</c>.
    /// </summary>
    public static List<ChoiceQuestion> ParseQuestions(JsonNode? raw)
    {
        if (raw is not JsonArray items)
        {
            throw new InteractionParseException("'questions' must be an array");
        }
        if (items.Count is < 1 or > 4)
        {
            throw new InteractionParseException("'questions' must contain between 1 and 4 questions");
        }

        var questions = new List<ChoiceQuestion>(items.Count);
        var seenHeaders = new List<string>(items.Count);
        foreach (var item in items)
        {
            if (item is not JsonObject obj)
            {
                throw new InteractionParseException("each question must be an object");
            }

            var question = obj["question"]?.GetValue<string>()?.Trim() is { Length: > 0 } q
                ? q
                : throw new InteractionParseException("each question needs a non-empty 'question'");

            var header = obj["header"]?.GetValue<string>()?.Trim() is { Length: > 0 } h
                ? h
                : throw new InteractionParseException("each question needs a non-empty 'header'");
            if (header.Length > ChoicesKind.HeaderMaxChars)
            {
                throw new InteractionParseException($"header '{header}' is too long (max {ChoicesKind.HeaderMaxChars} characters)");
            }
            if (seenHeaders.Contains(header))
            {
                throw new InteractionParseException($"duplicate question header '{header}'");
            }
            seenHeaders.Add(header);

            if (obj["options"] is not JsonArray rawOptions)
            {
                throw new InteractionParseException($"question '{header}' needs an 'options' array");
            }
            if (rawOptions.Count is < 2 or > 4)
            {
                throw new InteractionParseException($"question '{header}' must offer between 2 and 4 options");
            }

            var options = new List<ChoiceOption>(rawOptions.Count);
            foreach (var opt in rawOptions)
            {
                var option = opt switch
                {
                    JsonValue v when v.TryGetValue<string>(out var s) => new ChoiceOption { Label = s.Trim(), Description = string.Empty },
                    JsonObject o => new ChoiceOption
                    {
                        Label = (o["label"]?.GetValue<string>() ?? string.Empty).Trim(),
                        Description = (o["description"]?.GetValue<string>() ?? string.Empty).Trim(),
                    },
                    _ => throw new InteractionParseException($"invalid option entry in '{header}'"),
                };
                if (option.Label.Length == 0)
                {
                    throw new InteractionParseException($"an option in '{header}' has an empty label");
                }
                options.Add(option);
            }

            questions.Add(new ChoiceQuestion
            {
                Question = question,
                Header = header,
                Options = options,
                MultiSelect = obj["multiSelect"]?.GetValue<bool>() ?? false,
            });
        }
        return questions;
    }
}
