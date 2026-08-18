/**
 * Choices — a structured multiple-choice ask (modeled on Claude Code's
 * `AskUserQuestion`): a **Rich Interaction kind** (see {@link ./interaction.js}).
 *
 * The agent asks 1–4 short questions, each with 2–4 labeled options; the turn
 * parks until the visitor picks. Every question also carries an implicit
 * free-text **"Other"** escape hatch, so the visitor can answer outside the
 * enumerated options (exactly as `AskUserQuestion` always offers "Other").
 *
 * - On a channel that declared the `choice_chips` capability, the agent's
 *   `request_choices` tool parks the turn and the server emits
 *   `interaction_required { kind: "choices" }`; the client's chip/menu card
 *   resumes with a `submit_interaction` action.
 * - On a **text-only** channel the same raise degrades to a conversational
 *   directive that enumerates the questions + options and the model submits the
 *   picks through the generic `submit_interaction` *tool*.
 *
 * Both paths validate through {@link validateChoices} — one implementation, one
 * behavior — and resume the turn with the same structured payload. The
 * TypeScript port of the Rust reference `smooth-operator/src/choices.rs`.
 */
import type { InteractionKind, InteractionRequest, InteractionValidation } from './interaction.js';

/** Max length of a question's short `header` label (chip/tab caption). */
export const HEADER_MAX_CHARS = 12;

/** One selectable option in a question. */
export interface ChoiceOption {
    /** The option's label — the value the visitor submits. */
    label: string;
    /** A short human-readable gloss shown under/next to the label. */
    description: string;
}

/** One question in a `choices` raise. */
export interface ChoiceQuestion {
    /** The question prompt shown to the visitor. */
    question: string;
    /** A short label (≤{@link HEADER_MAX_CHARS} chars) — the answer key and the chip/tab caption. Unique within a raise. */
    header: string;
    /** The 2–4 enumerated options. An implicit free-text "Other" is always available in addition. */
    options: ChoiceOption[];
    /** Whether the visitor may pick more than one option (default `false`). */
    multiSelect: boolean;
}

/** The visitor's answer to one question, submitted via `submit_interaction`. */
export interface ChoiceAnswer {
    /** Which question this answers — matches the spec question's `header`. */
    header: string;
    /** The selected option label(s). Empty when the visitor only used the free-text "Other". */
    options: string[];
    /** The free-text "Other" answer, when the visitor answered outside the enumerated options. Blank ⇒ omitted. */
    other?: string;
}

/** Validated, normalized choice answers — the structured payload the parked turn resumes with. */
export interface ChoiceValues {
    /** One entry per answered question (in submission order). */
    answers: ChoiceAnswer[];
}

/** A single per-question validation failure. `field` is the question's `header`. */
export interface ChoiceFieldError {
    field: string;
    message: string;
}

/** Total picks the visitor made (selected labels + one for a non-blank "Other"). */
function selectionCount(answer: ChoiceAnswer): number {
    return answer.options.length + (answer.other !== undefined ? 1 : 0);
}

/**
 * Validate submitted `values` against the raised `questions`, returning the
 * normalized {@link ChoiceValues} or the full list of per-question errors.
 *
 * Rules (mirrors `choices.rs`):
 * - **every** question must be answered (a selection or a non-blank "Other");
 * - each selected label must be one of that question's option labels;
 * - single-select: exactly one pick (one label XOR "Other"); multi-select: one or more picks;
 * - a blank/whitespace "Other" is treated as absent; labels are trimmed.
 *
 * When `questions` is empty (a prior-turn fallback raise whose spec is gone),
 * validation degrades to **format-only**: labels can't be checked for
 * membership, so any answer with at least one pick is accepted as-is.
 *
 * Returns every failed question (not just the first) so a card can annotate all
 * of them in one round-trip.
 */
export function validateChoices(questions: ChoiceQuestion[], values: ChoiceValues): { ok: true; values: ChoiceValues } | { ok: false; errors: ChoiceFieldError[] } {
    // Normalize the raw answers first (trim labels + "Other", drop blanks).
    const normalized: ChoiceAnswer[] = values.answers.map((a) => {
        const other = a.other?.trim();
        const answer: ChoiceAnswer = {
            header: a.header.trim(),
            options: a.options.map((o) => o.trim()).filter((o) => o.length > 0),
        };
        if (other && other.length > 0) answer.other = other;
        return answer;
    });

    // Format-only path: no spec to check membership / required-ness against.
    if (questions.length === 0) {
        const errors: ChoiceFieldError[] = [];
        for (const answer of normalized) {
            if (selectionCount(answer) === 0) {
                errors.push({ field: answer.header, message: "select an option or provide an 'other' answer" });
            }
        }
        if (normalized.length === 0) {
            errors.push({ field: 'answers', message: 'provide an answer for each question, or declined=true' });
        }
        return errors.length === 0 ? { ok: true, values: { answers: normalized } } : { ok: false, errors };
    }

    const errors: ChoiceFieldError[] = [];
    const out: ChoiceAnswer[] = [];

    for (const question of questions) {
        const answer = normalized.find((a) => a.header === question.header);
        if (!answer) {
            errors.push({ field: question.header, message: 'this question must be answered' });
            continue;
        }

        // Every selected label must be one of the enumerated options.
        let badLabel = false;
        for (const label of answer.options) {
            if (!question.options.some((o) => o.label === label)) {
                badLabel = true;
                errors.push({ field: question.header, message: `'${label}' is not one of the offered options` });
            }
        }

        const count = selectionCount(answer);
        if (count === 0) {
            errors.push({ field: question.header, message: "select an option or provide an 'other' answer" });
        } else if (!question.multiSelect && count > 1) {
            errors.push({ field: question.header, message: 'this question takes a single answer' });
        }

        if (!badLabel) out.push(answer);
    }

    return errors.length === 0 ? { ok: true, values: { answers: out } } : { ok: false, errors };
}

/**
 * Parse the raise tool's `questions` argument into validated {@link ChoiceQuestion}s.
 *
 * Enforces the LLM-facing contract so the model produces usable cards: 1–4
 * questions, each with a non-empty prompt, a non-empty header ≤12 chars (unique
 * within the raise), and 2–4 options with non-empty labels.
 */
export function parseQuestions(raw: unknown): ChoiceQuestion[] {
    if (!Array.isArray(raw)) throw new Error("'questions' must be an array");
    if (raw.length < 1 || raw.length > 4) throw new Error("'questions' must contain between 1 and 4 questions");

    const questions: ChoiceQuestion[] = [];
    const seenHeaders = new Set<string>();
    for (const item of raw) {
        if (typeof item !== 'object' || item === null || Array.isArray(item)) throw new Error('each question must be an object');
        const obj = item as Record<string, unknown>;

        const question = typeof obj.question === 'string' ? obj.question.trim() : '';
        if (!question) throw new Error("each question needs a non-empty 'question'");

        const header = typeof obj.header === 'string' ? obj.header.trim() : '';
        if (!header) throw new Error("each question needs a non-empty 'header'");
        if ([...header].length > HEADER_MAX_CHARS) throw new Error(`header '${header}' is too long (max ${HEADER_MAX_CHARS} characters)`);
        if (seenHeaders.has(header)) throw new Error(`duplicate question header '${header}'`);
        seenHeaders.add(header);

        const rawOptions = obj.options;
        if (!Array.isArray(rawOptions)) throw new Error(`question '${header}' needs an 'options' array`);
        if (rawOptions.length < 2 || rawOptions.length > 4) throw new Error(`question '${header}' must offer between 2 and 4 options`);

        const options: ChoiceOption[] = [];
        for (const opt of rawOptions) {
            // Accept the object form `{ label, description? }` and the shorthand bare string.
            let option: ChoiceOption;
            if (typeof opt === 'string') {
                option = { label: opt.trim(), description: '' };
            } else if (typeof opt === 'object' && opt !== null && !Array.isArray(opt)) {
                const o = opt as Record<string, unknown>;
                option = {
                    label: typeof o.label === 'string' ? o.label.trim() : '',
                    description: typeof o.description === 'string' ? o.description.trim() : '',
                };
            } else {
                throw new Error(`invalid option entry in '${header}'`);
            }
            if (!option.label) throw new Error(`an option in '${header}' has an empty label`);
            options.push(option);
        }

        questions.push({ question, header, options, multiSelect: obj.multiSelect === true });
    }
    return questions;
}

/** Coerce the submitted `values` into {@link ChoiceValues}, or throw a shape error. */
function parseValues(values: unknown): ChoiceValues {
    if (typeof values !== 'object' || values === null || Array.isArray(values)) throw new Error('values must be an object');
    const answersRaw = (values as Record<string, unknown>).answers;
    if (!Array.isArray(answersRaw)) throw new Error("values.answers must be an array");
    const answers: ChoiceAnswer[] = answersRaw.map((a) => {
        if (typeof a !== 'object' || a === null || Array.isArray(a)) throw new Error('each answer must be an object');
        const o = a as Record<string, unknown>;
        const answer: ChoiceAnswer = {
            header: typeof o.header === 'string' ? o.header : '',
            options: Array.isArray(o.options) ? o.options.filter((x): x is string => typeof x === 'string') : [],
        };
        if (typeof o.other === 'string') answer.other = o.other;
        return answer;
    });
    return { answers };
}

/**
 * The `choices` Rich Interaction kind — a structured multiple-choice ask modeled
 * on `AskUserQuestion` (see the module docs and `spec/interactions/choices.schema.json`).
 */
export class ChoicesKind implements InteractionKind {
    readonly kind = 'choices';
    readonly capability = 'choice_chips';

    toolSchema(): { name: string; description: string; parameters: Record<string, unknown> } {
        return {
            name: 'request_choices',
            description:
                'Ask the visitor a structured multiple-choice question (1–4 questions, each with 2–4 labeled options) '
                + 'and wait for their pick. On channels that can render chips/menus the visitor taps an option; on text '
                + 'channels you will be told to enumerate the options and accept a natural-language answer. An implicit '
                + 'free-text "Other" is always available, so use this whenever the answer is likely (but not certainly) '
                + 'one of a small set — never free-form the menu yourself.',
            parameters: {
                type: 'object',
                properties: {
                    questions: {
                        type: 'array',
                        minItems: 1,
                        maxItems: 4,
                        description: 'The questions to ask, in order (1–4).',
                        items: {
                            type: 'object',
                            properties: {
                                question: { type: 'string', description: 'The question prompt shown to the visitor.' },
                                header: { type: 'string', maxLength: HEADER_MAX_CHARS, description: 'A short label (≤12 chars), unique within the raise. Used as the answer key and the chip/tab caption.' },
                                options: {
                                    type: 'array',
                                    minItems: 2,
                                    maxItems: 4,
                                    description: "The 2–4 options to offer. A free-text 'Other' is always available in addition.",
                                    items: {
                                        type: 'object',
                                        properties: {
                                            label: { type: 'string', description: 'The option label (the value submitted).' },
                                            description: { type: 'string', description: 'A short gloss for the option.' },
                                        },
                                        required: ['label'],
                                    },
                                },
                                multiSelect: { type: 'boolean', description: 'Allow selecting more than one option (default false).' },
                            },
                            required: ['question', 'header', 'options'],
                        },
                    },
                    reason: {
                        type: 'string',
                        description: 'Why you\'re asking, phrased for the visitor (e.g. "to route you to the right team").',
                    },
                },
                required: ['questions', 'reason'],
            },
        };
    }

    parseRequest(args: Record<string, unknown>): InteractionRequest {
        const questions = parseQuestions(args.questions ?? null);
        const reason = typeof args.reason === 'string' && args.reason.trim().length > 0 ? args.reason.trim() : 'to help you better';
        return { kind: this.kind, spec: { questions }, reason };
    }

    validate(spec: unknown, values: unknown): InteractionValidation {
        const questions = specQuestions(spec);
        let parsed: ChoiceValues;
        try {
            parsed = parseValues(values);
        } catch (err) {
            return { ok: false, errors: [{ field: 'values', message: `invalid values shape: ${err instanceof Error ? err.message : String(err)}` }] };
        }
        const result = validateChoices(questions, parsed);
        return result.ok ? { ok: true, values: result.values } : { ok: false, errors: result.errors };
    }

    fallbackDirective(spec: unknown, reason: string): string {
        const questions = specQuestions(spec);
        const enumerated = questions
            .map((q) => {
                const opts = q.options.map((o) => o.label).join(', ');
                return `- [${q.header}] ${q.question} Options: ${opts}${q.multiSelect ? ' (choose one or more)' : ''}.`;
            })
            .join('\n');
        return (
            "This visitor's channel cannot display choice chips. Ask the following question(s) conversationally, "
            + `naturally weaving in the reason (${reason}), and read out each option so the visitor can pick:\n${enumerated}\n`
            + "The visitor may also answer with something not listed (that's fine — capture it as their 'other' answer). "
            + 'When you have their pick(s), call the `submit_interaction` tool with kind "choices" and `values.answers` — '
            + 'one entry per question `{ header, options: [chosen label(s)], other?: "their free-text answer" }`. It '
            + "validates each answer and will tell you if a pick isn't offered so you can re-ask. If the visitor declines "
            + 'to choose, call `submit_interaction` with declined=true and continue helping them.'
        );
    }
}

/** Read the `questions` out of a raise spec (best-effort; a malformed/absent spec ⇒ format-only []). */
function specQuestions(spec: unknown): ChoiceQuestion[] {
    if (typeof spec !== 'object' || spec === null) return [];
    const raw = (spec as Record<string, unknown>).questions;
    if (!Array.isArray(raw)) return [];
    const questions: ChoiceQuestion[] = [];
    for (const q of raw) {
        if (typeof q !== 'object' || q === null) continue;
        const o = q as Record<string, unknown>;
        const options = Array.isArray(o.options)
            ? o.options
                  .filter((op): op is Record<string, unknown> => typeof op === 'object' && op !== null)
                  .map((op) => ({ label: typeof op.label === 'string' ? op.label : '', description: typeof op.description === 'string' ? op.description : '' }))
            : [];
        questions.push({
            question: typeof o.question === 'string' ? o.question : '',
            header: typeof o.header === 'string' ? o.header : '',
            options,
            multiSelect: o.multiSelect === true,
        });
    }
    return questions;
}
