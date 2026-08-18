/**
 * ChoicesCard tests — the web SDK card renderer for the `choices` Rich
 * Interaction. We assert the two things that matter: the payload the card builds
 * matches the schema `Values` shape (`spec/interactions/choices.schema.json`),
 * and the card is usable (options render, Submit gates on completeness, Decline
 * fires). No DOM-less pure test AND a rendered flow, since both paths ship.
 */
import { ChoicesCard, buildChoicesValues, type ChoiceQuestion } from '../../src/react/components/ChoicesCard.js';
import type { ChoicesSpec } from '../../src/generated/types.js';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';

// No globals in this vitest config, so @testing-library's auto-cleanup never
// registers — unmount between tests ourselves or rendered cards accumulate.
afterEach(cleanup);

const SPEC: ChoicesSpec = {
    questions: [
        {
            question: 'Which plan fits you?',
            header: 'Plan',
            options: [
                { label: 'Starter', description: 'For individuals' },
                { label: 'Team', description: 'For small teams' },
            ],
        },
        {
            question: 'Which features matter?',
            header: 'Features',
            multiSelect: true,
            options: [
                { label: 'Analytics' },
                { label: 'SSO' },
                { label: 'API access' },
            ],
        },
    ],
} as unknown as ChoicesSpec;

const QUESTIONS = SPEC.questions as unknown as ChoiceQuestion[];

describe('buildChoicesValues', () => {
    it('emits one answer per question keyed by header', () => {
        const values = buildChoicesValues(QUESTIONS, {
            Plan: { selected: ['Starter'], other: '' },
            Features: { selected: ['Analytics', 'SSO'], other: '' },
        });
        expect(values).toEqual({
            answers: [
                { header: 'Plan', options: ['Starter'] },
                { header: 'Features', options: ['Analytics', 'SSO'] },
            ],
        });
    });

    it('drops blank `other` and trims non-blank', () => {
        const values = buildChoicesValues(QUESTIONS, {
            Plan: { selected: ['Team'], other: '   ' },
            Features: { selected: [], other: '  Webhooks  ' },
        });
        expect(values.answers[0]).toEqual({ header: 'Plan', options: ['Team'] });
        expect(values.answers[1]).toEqual({ header: 'Features', other: 'Webhooks' });
    });

    it('single-select "other" submits as `other` alone (sentinel stripped, options omitted)', () => {
        const values = buildChoicesValues(QUESTIONS, {
            Plan: { selected: ['__other__'], other: 'Enterprise' },
            Features: { selected: ['API access'], other: '' },
        });
        expect(values.answers[0]).toEqual({ header: 'Plan', other: 'Enterprise' });
        expect(values.answers[1]).toEqual({ header: 'Features', options: ['API access'] });
    });
});

describe('<ChoicesCard>', () => {
    it('renders every question header, prompt, and option', () => {
        render(<ChoicesCard spec={SPEC} reason="to route your request" onSubmit={vi.fn()} onDecline={vi.fn()} />);
        expect(screen.getByText('to route your request')).toBeTruthy();
        expect(screen.getByText('Plan')).toBeTruthy();
        expect(screen.getByText('Which plan fits you?')).toBeTruthy();
        expect(screen.getByText('For individuals')).toBeTruthy();
        expect(screen.getByLabelText(/Starter/)).toBeTruthy();
    });

    it('gates Submit until every question is answered, then emits the schema Values', () => {
        const onSubmit = vi.fn();
        render(<ChoicesCard spec={SPEC} onSubmit={onSubmit} onDecline={vi.fn()} />);

        const submit = screen.getByRole('button', { name: 'Submit' }) as HTMLButtonElement;
        expect(submit.disabled).toBe(true); // nothing chosen yet

        fireEvent.click(screen.getByLabelText(/Starter/)); // single-select radio
        expect(submit.disabled).toBe(true); // second question still unanswered

        fireEvent.click(screen.getByLabelText('Analytics')); // multi-select checkbox
        fireEvent.click(screen.getByLabelText('SSO'));
        expect(submit.disabled).toBe(false);

        fireEvent.click(submit);
        expect(onSubmit).toHaveBeenCalledTimes(1);
        expect(onSubmit).toHaveBeenCalledWith({
            answers: [
                { header: 'Plan', options: ['Starter'] },
                { header: 'Features', options: ['Analytics', 'SSO'] },
            ],
        });
    });

    it('accepts a free-text Other answer for a multi-select question', () => {
        const onSubmit = vi.fn();
        render(<ChoicesCard spec={SPEC} onSubmit={onSubmit} onDecline={vi.fn()} />);

        fireEvent.click(screen.getByLabelText(/Team/));
        fireEvent.change(screen.getByLabelText('Other answer for Features'), { target: { value: 'On-prem' } });

        fireEvent.click(screen.getByRole('button', { name: 'Submit' }));
        expect(onSubmit).toHaveBeenCalledWith({
            answers: [
                { header: 'Plan', options: ['Team'] },
                { header: 'Features', other: 'On-prem' },
            ],
        });
    });

    it('fires onDecline from the "Not now" button', () => {
        const onDecline = vi.fn();
        render(<ChoicesCard spec={SPEC} onSubmit={vi.fn()} onDecline={onDecline} />);
        fireEvent.click(screen.getByRole('button', { name: 'Not now' }));
        expect(onDecline).toHaveBeenCalledTimes(1);
    });
});
