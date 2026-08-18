/**
 * The `choices` Rich Interaction kind — validator + kind wiring + the shared
 * conformance fixtures.
 *
 * The TS parity of the Rust `choices.rs` unit tests: the validator's one-pass
 * per-question rules, the raise-tool argument contract (`parseQuestions`), the
 * kind's `parseRequest`/`validate`/`fallbackDirective`, and the `interaction_required`
 * protocol builder — all cross-checked against the three shared `choices` fixtures
 * in `spec/conformance/fixtures.json` (so a drift in any one server is caught here).
 */
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { describe, expect, it } from 'vitest';

import { ChoicesKind, HEADER_MAX_CHARS, parseQuestions, validateChoices, type ChoiceQuestion, type ChoiceValues } from '../src/choices.js';
import * as protocol from '../src/protocol.js';

const __dirname = dirname(fileURLToPath(import.meta.url));
const SPEC_DIR = join(__dirname, '..', '..', '..', 'spec');
const fixtures = JSON.parse(readFileSync(join(SPEC_DIR, 'conformance', 'fixtures.json'), 'utf8')) as Record<string, { instance: Record<string, unknown> } | string>;

function fixture(name: string): Record<string, unknown> {
    const entry = fixtures[name];
    if (!entry || typeof entry === 'string') throw new Error(`missing fixture ${name}`);
    return entry.instance;
}

function q(header: string, labels: string[], multiSelect = false): ChoiceQuestion {
    return { question: `${header}?`, header, options: labels.map((l) => ({ label: l, description: '' })), multiSelect };
}
function vals(answers: ChoiceValues['answers']): ChoiceValues {
    return { answers };
}

describe('validateChoices', () => {
    it('single-select normalizes (trims labels)', () => {
        const out = validateChoices([q('Plan', ['Basic', 'Pro'])], vals([{ header: 'Plan', options: ['  Pro  '] }]));
        expect(out.ok).toBe(true);
        if (out.ok) {
            expect(out.values.answers).toHaveLength(1);
            expect(out.values.answers[0]!.options).toEqual(['Pro']);
            expect(out.values.answers[0]!.other).toBeUndefined();
        }
    });

    it('multi-select keeps all picks', () => {
        const out = validateChoices([q('Topics', ['Sales', 'Support', 'Billing'], true)], vals([{ header: 'Topics', options: ['Sales', 'Billing'] }]));
        expect(out.ok).toBe(true);
        if (out.ok) expect(out.values.answers[0]!.options).toEqual(['Sales', 'Billing']);
    });

    it("accepts the free-text 'Other' escape hatch and trims it; blank Other dropped", () => {
        const out = validateChoices([q('Plan', ['Basic', 'Pro'])], vals([{ header: 'Plan', options: [], other: '  Enterprise, actually  ' }]));
        expect(out.ok).toBe(true);
        if (out.ok) {
            expect(out.values.answers[0]!.options).toEqual([]);
            expect(out.values.answers[0]!.other).toBe('Enterprise, actually');
        }
        const blank = validateChoices([q('Plan', ['Basic', 'Pro'])], vals([{ header: 'Plan', options: ['Pro'], other: '   ' }]));
        expect(blank.ok).toBe(true);
        if (blank.ok) expect(blank.values.answers[0]!.other).toBeUndefined();
    });

    it('unknown label is a per-question error', () => {
        const out = validateChoices([q('Plan', ['Basic', 'Pro'])], vals([{ header: 'Plan', options: ['Platinum'] }]));
        expect(out.ok).toBe(false);
        if (!out.ok) {
            expect(out.errors).toHaveLength(1);
            expect(out.errors[0]!.field).toBe('Plan');
            expect(out.errors[0]!.message).toContain('not one of the offered');
        }
    });

    it('single-select rejects multiple picks', () => {
        const out = validateChoices([q('Plan', ['Basic', 'Pro'])], vals([{ header: 'Plan', options: ['Basic', 'Pro'] }]));
        expect(out.ok).toBe(false);
        if (!out.ok) expect(out.errors.some((e) => e.message.includes('single answer'))).toBe(true);
    });

    it('every question must be answered', () => {
        const out = validateChoices([q('Plan', ['Basic', 'Pro']), q('Size', ['S', 'M'])], vals([{ header: 'Plan', options: ['Pro'] }]));
        expect(out.ok).toBe(false);
        if (!out.ok) {
            expect(out.errors).toHaveLength(1);
            expect(out.errors[0]!.field).toBe('Size');
            expect(out.errors[0]!.message).toContain('must be answered');
        }
    });

    it('an empty answer needs a pick or an other', () => {
        const out = validateChoices([q('Plan', ['Basic', 'Pro'])], vals([{ header: 'Plan', options: [] }]));
        expect(out.ok).toBe(false);
        if (!out.ok) expect(out.errors.some((e) => e.message.includes('select an option'))).toBe(true);
    });

    it('format-only path (no questions) accepts any answer with a pick', () => {
        const out = validateChoices([], vals([{ header: 'Anything', options: ['whatever'] }]));
        expect(out.ok).toBe(true);
        const empty = validateChoices([], vals([]));
        expect(empty.ok).toBe(false);
    });
});

describe('parseQuestions — the raise-tool contract', () => {
    it('accepts shorthand string options and defaults multiSelect to false', () => {
        const qs = parseQuestions([{ question: 'Which plan?', header: 'Plan', options: ['Basic', 'Pro'] }]);
        expect(qs).toHaveLength(1);
        expect(qs[0]!.options[0]!.label).toBe('Basic');
        expect(qs[0]!.multiSelect).toBe(false);
    });

    it('rejects >4 questions, <2 options, an over-long header, and duplicate headers', () => {
        expect(() => parseQuestions(Array.from({ length: 5 }, (_, i) => ({ question: 'q', header: `H${i}`, options: ['a', 'b'] })))).toThrow(/between 1 and 4/);
        expect(() => parseQuestions([{ question: 'q', header: 'H', options: ['only'] }])).toThrow(/between 2 and 4/);
        expect(() => parseQuestions([{ question: 'q', header: 'ThisHeaderIsWayTooLong', options: ['a', 'b'] }])).toThrow(/too long/);
        expect(() => parseQuestions([{ question: 'q1', header: 'H', options: ['a', 'b'] }, { question: 'q2', header: 'H', options: ['a', 'b'] }])).toThrow(/duplicate/);
    });

    it(`caps the header at ${HEADER_MAX_CHARS} characters`, () => {
        expect(() => parseQuestions([{ question: 'q', header: 'x'.repeat(HEADER_MAX_CHARS + 1), options: ['a', 'b'] }])).toThrow(/too long/);
        expect(parseQuestions([{ question: 'q', header: 'x'.repeat(HEADER_MAX_CHARS), options: ['a', 'b'] }])).toHaveLength(1);
    });
});

describe('ChoicesKind', () => {
    it('exposes the reference identity + raise-tool surface', () => {
        const kind = new ChoicesKind();
        expect(kind.kind).toBe('choices');
        expect(kind.capability).toBe('choice_chips');
        expect(kind.toolSchema().name).toBe('request_choices');
    });

    it('parseRequest canonicalizes questions + reason', () => {
        const req = new ChoicesKind().parseRequest({
            questions: [{ question: 'Which plan interests you?', header: 'Plan', options: [{ label: 'Basic' }, { label: 'Pro' }] }],
            reason: 'to route you',
        });
        expect(req.kind).toBe('choices');
        expect(req.reason).toBe('to route you');
        expect((req.spec as { questions: ChoiceQuestion[] }).questions[0]!.header).toBe('Plan');
    });

    it('fallbackDirective enumerates the options and points at submit_interaction', () => {
        const kind = new ChoicesKind();
        const req = kind.parseRequest({ questions: [{ question: 'Which plan?', header: 'Plan', options: ['Basic', 'Pro'] }], reason: 'to route you' });
        const directive = kind.fallbackDirective(req.spec, 'to route you');
        expect(directive).toContain('Basic, Pro');
        expect(directive).toContain('submit_interaction');
    });
});

describe('the shared choices fixtures', () => {
    it('validate(choices_spec, choices_values) produces choices_payload.values', () => {
        const spec = fixture('choices_spec');
        const values = fixture('choices_values');
        const payload = fixture('choices_payload');
        const out = new ChoicesKind().validate(spec, values);
        expect(out.ok).toBe(true);
        if (out.ok) expect(out.values).toEqual((payload as { values: unknown }).values);
    });

    it('interactionRequired builder matches the interaction_required_event shape, carrying the choices spec', () => {
        const spec = fixture('choices_spec');
        const built = protocol.interactionRequired('req-a1b2c3d4-0004', '88888888-8888-8888-8888-888888888888', 'choices', spec, 'to route you');
        const rt = JSON.parse(JSON.stringify(built)) as Record<string, unknown>;
        expect(rt.type).toBe('interaction_required');
        expect(rt.requestId).toBe('req-a1b2c3d4-0004');
        const inner = (rt.data as { data: Record<string, unknown> }).data;
        expect(inner.interactionId).toBe('88888888-8888-8888-8888-888888888888');
        expect(inner.kind).toBe('choices');
        expect(inner.spec).toEqual(spec);
        expect(inner.reason).toBe('to route you');
    });

    it('interactionInvalid builder carries per-field errors and stays retryable-shaped', () => {
        const built = protocol.interactionInvalid('req-1', 'i-1', 'choices', [{ field: 'Plan', message: 'this question must be answered' }], 'Some fields need attention.');
        const inner = (built.data as { data: Record<string, unknown> }).data;
        expect(built.type).toBe('interaction_invalid');
        expect((inner.errors as unknown[])).toHaveLength(1);
        expect((inner.errors as { field: string }[])[0]!.field).toBe('Plan');
        expect(inner.message).toBe('Some fields need attention.');
    });
});
