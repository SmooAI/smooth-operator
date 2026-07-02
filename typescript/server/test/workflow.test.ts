/**
 * Unit tests for the conversation-workflow primitives (SMOODEV-590).
 *
 * Pure helpers (parse / resolve / next / render / advance) plus the LLM-touching
 * judge, driven by MockLlmProvider so every verdict path — yes / no / maybe /
 * unparseable / thrown — is exercised deterministically.
 */
import { MockLlmProvider } from '@smooai/smooth-operator-core';
import { describe, expect, it } from 'vitest';

import {
    advanceStep,
    judgeStep,
    nextStep,
    parseWorkflow,
    renderWorkflowPromptSection,
    resolveCurrentStep,
    type ConversationWorkflow,
} from '../src/workflow.js';

const WORKFLOW: ConversationWorkflow = {
    goal: 'Book a demo',
    steps: [
        { id: 'greet', intent: 'Greet the visitor', criteria: 'Visitor greeted by name' },
        { id: 'qualify', intent: 'Qualify the lead', criteria: 'Company size captured', next: 'book' },
        { id: 'book', intent: 'Book the demo', criteria: 'Meeting scheduled' },
    ],
};

describe('parseWorkflow — tolerant', () => {
    it('accepts a well-formed workflow', () => {
        expect(parseWorkflow(WORKFLOW)).toEqual(WORKFLOW);
    });

    it('keeps an optional next only when it is a non-empty string', () => {
        const parsed = parseWorkflow(WORKFLOW)!;
        expect(parsed.steps[0]!.next).toBeUndefined();
        expect(parsed.steps[1]!.next).toBe('book');
    });

    it.each([
        ['null', null],
        ['a string', 'nope'],
        ['an array', [{ id: 'a' }]],
        ['a missing goal', { steps: WORKFLOW.steps }],
        ['an empty goal', { goal: '   ', steps: WORKFLOW.steps }],
        ['no steps', { goal: 'g', steps: [] }],
        ['non-array steps', { goal: 'g', steps: 'x' }],
        ['a step missing id', { goal: 'g', steps: [{ intent: 'i', criteria: 'c' }] }],
        ['a step missing intent', { goal: 'g', steps: [{ id: 'a', criteria: 'c' }] }],
        ['a step missing criteria', { goal: 'g', steps: [{ id: 'a', intent: 'i' }] }],
        ['duplicate step ids', { goal: 'g', steps: [{ id: 'a', intent: 'i', criteria: 'c' }, { id: 'a', intent: 'i2', criteria: 'c2' }] }],
    ])('degrades to undefined for %s', (_label, raw) => {
        expect(parseWorkflow(raw)).toBeUndefined();
    });

    it('rejects more than 20 steps', () => {
        const steps = Array.from({ length: 21 }, (_v, i) => ({ id: `s${i}`, intent: 'i', criteria: 'c' }));
        expect(parseWorkflow({ goal: 'g', steps })).toBeUndefined();
    });

    it('never throws on hostile input', () => {
        expect(() => parseWorkflow(undefined)).not.toThrow();
        expect(() => parseWorkflow({ goal: 'g', steps: [null] })).not.toThrow();
    });
});

describe('resolveCurrentStep', () => {
    it('returns the first step for an empty pointer (fresh start)', () => {
        expect(resolveCurrentStep(WORKFLOW, undefined)?.id).toBe('greet');
        expect(resolveCurrentStep(WORKFLOW, '')?.id).toBe('greet');
    });

    it('returns the matching step by id', () => {
        expect(resolveCurrentStep(WORKFLOW, 'qualify')?.id).toBe('qualify');
    });

    it('falls back to the first step for an unknown pointer', () => {
        expect(resolveCurrentStep(WORKFLOW, 'ghost')?.id).toBe('greet');
    });

    it('returns null when there is no workflow', () => {
        expect(resolveCurrentStep(undefined, 'x')).toBeNull();
    });
});

describe('nextStep', () => {
    it('follows an explicit next', () => {
        expect(nextStep(WORKFLOW, WORKFLOW.steps[1]!)?.id).toBe('book');
    });

    it('falls through to the array-order next when no explicit next', () => {
        expect(nextStep(WORKFLOW, WORKFLOW.steps[0]!)?.id).toBe('qualify');
    });

    it('returns null on the terminal step', () => {
        expect(nextStep(WORKFLOW, WORKFLOW.steps[2]!)).toBeNull();
    });

    it('ignores a dangling explicit next and uses array order', () => {
        const wf: ConversationWorkflow = { goal: 'g', steps: [{ id: 'a', intent: 'i', criteria: 'c', next: 'gone' }, { id: 'b', intent: 'i', criteria: 'c' }] };
        expect(nextStep(wf, wf.steps[0]!)?.id).toBe('b');
    });
});

describe('renderWorkflowPromptSection', () => {
    it('renders the current step with a 1-based number + total', () => {
        const section = renderWorkflowPromptSection(WORKFLOW, 'qualify');
        expect(section).toContain('CURRENT STEP (2/3): qualify');
        expect(section).toContain('INTENT: Qualify the lead');
        expect(section).toContain('CRITERIA: Company size captured');
        expect(section).toContain('GOAL: Book a demo');
    });

    it('renders the first step for an empty pointer', () => {
        expect(renderWorkflowPromptSection(WORKFLOW, undefined)).toContain('CURRENT STEP (1/3): greet');
    });

    it('returns an empty string when no workflow is configured', () => {
        expect(renderWorkflowPromptSection(undefined, 'x')).toBe('');
    });
});

describe('advanceStep', () => {
    it('advances on a yes verdict', () => {
        expect(advanceStep(WORKFLOW, WORKFLOW.steps[0]!, 'yes')).toBe('qualify');
    });

    it('stays on the terminal step even on yes', () => {
        expect(advanceStep(WORKFLOW, WORKFLOW.steps[2]!, 'yes')).toBe('book');
    });

    it.each(['no', 'maybe', 'skipped'] as const)('stays put on a %s verdict', (verdict) => {
        expect(advanceStep(WORKFLOW, WORKFLOW.steps[0]!, verdict)).toBe('greet');
    });
});

describe('judgeStep — LLM verdict', () => {
    const base = { workflow: WORKFLOW, current: WORKFLOW.steps[0]!, userMessage: 'hi', reply: 'Hello Sam!' };

    it('returns the JSON verdict', async () => {
        const mock = new MockLlmProvider().pushText('{"verdict":"yes"}');
        expect(await judgeStep(mock, base)).toBe('yes');
    });

    it('falls back to a bare-word scan when the model ignores the JSON instruction', async () => {
        const mock = new MockLlmProvider().pushText('Yes, the visitor was greeted.');
        expect(await judgeStep(mock, base)).toBe('yes');
    });

    it('returns maybe / no verbatim', async () => {
        expect(await judgeStep(new MockLlmProvider().pushText('{"verdict":"maybe"}'), base)).toBe('maybe');
        expect(await judgeStep(new MockLlmProvider().pushText('{"verdict":"no"}'), base)).toBe('no');
    });

    it('skips (stays put) on an unparseable verdict', async () => {
        expect(await judgeStep(new MockLlmProvider().pushText('the model rambled without a verdict token'), base)).toBe('skipped');
    });

    it('skips on a judge error rather than freezing the conversation', async () => {
        expect(await judgeStep(new MockLlmProvider().pushError('gateway 500'), base)).toBe('skipped');
    });

    it('skips without an LLM call when the reply is empty', async () => {
        const mock = new MockLlmProvider().pushText('{"verdict":"yes"}');
        expect(await judgeStep(mock, { ...base, reply: '   ' })).toBe('skipped');
        expect(mock.callCount).toBe(0);
    });

    it('judges with the configured cheap model', async () => {
        const mock = new MockLlmProvider().pushText('{"verdict":"no"}');
        await judgeStep(mock, { ...base, model: 'cheap-judge-1' });
        expect(mock.lastCall?.body.model).toBe('cheap-judge-1');
    });
});
