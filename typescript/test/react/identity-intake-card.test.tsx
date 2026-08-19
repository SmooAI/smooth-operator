/**
 * IdentityIntakeCard tests — the web SDK card renderer for the `identity_intake`
 * Rich Interaction (name/email/phone lead capture). We assert the things that
 * matter: the payload the card builds is the schema `Values` subset
 * (`spec/interactions/identity-intake.schema.json` — only the filled-in fields),
 * required-field submit gating, the decline path, and that a server
 * `interaction_invalid` field error renders in place with the turn still parked.
 */
import { IdentityIntakeCard, buildIdentityValues, type IdentityField } from '../../src/react/components/IdentityIntakeCard.js';
import type { IdentityIntakeSpec } from '../../src/generated/types.js';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';

// No globals in this vitest config, so @testing-library's auto-cleanup never
// registers — unmount between tests ourselves or rendered cards accumulate.
afterEach(cleanup);

const SPEC: IdentityIntakeSpec = {
    fields: [
        { key: 'name', required: true },
        { key: 'email', required: true },
        { key: 'phone', required: false, label: 'Mobile' },
    ],
} as unknown as IdentityIntakeSpec;

const FIELDS = SPEC.fields as unknown as IdentityField[];

describe('buildIdentityValues', () => {
    it('emits only the filled-in fields, trimmed', () => {
        const values = buildIdentityValues(FIELDS, { name: '  Ada Lovelace  ', email: 'ada@example.com', phone: '' });
        expect(values).toEqual({ name: 'Ada Lovelace', email: 'ada@example.com' });
    });

    it('omits a blank optional field entirely (never sends "")', () => {
        const values = buildIdentityValues(FIELDS, { name: 'Ada', email: 'ada@example.com', phone: '   ' });
        expect(values).not.toHaveProperty('phone');
    });
});

describe('<IdentityIntakeCard>', () => {
    it('renders a labelled input per field in spec order (label override honored)', () => {
        render(<IdentityIntakeCard spec={SPEC} reason="so we can follow up" onSubmit={vi.fn()} onDecline={vi.fn()} />);
        expect(screen.getByText('so we can follow up')).toBeTruthy();
        expect((screen.getByLabelText(/Name/) as HTMLInputElement).type).toBe('text');
        expect((screen.getByLabelText(/Email/) as HTMLInputElement).type).toBe('email');
        // `phone` field carries a `label` override → "Mobile", tel input.
        expect((screen.getByLabelText(/Mobile/) as HTMLInputElement).type).toBe('tel');
    });

    it('gates Submit until every required field is filled, then emits the Values subset', () => {
        const onSubmit = vi.fn();
        render(<IdentityIntakeCard spec={SPEC} onSubmit={onSubmit} onDecline={vi.fn()} />);

        const submit = screen.getByRole('button', { name: 'Submit' }) as HTMLButtonElement;
        expect(submit.disabled).toBe(true); // both required fields empty

        fireEvent.change(screen.getByLabelText(/Name/), { target: { value: 'Ada Lovelace' } });
        expect(submit.disabled).toBe(true); // email (required) still empty

        fireEvent.change(screen.getByLabelText(/Email/), { target: { value: 'ada@example.com' } });
        expect(submit.disabled).toBe(false); // phone is optional

        fireEvent.click(submit);
        expect(onSubmit).toHaveBeenCalledTimes(1);
        expect(onSubmit).toHaveBeenCalledWith({ name: 'Ada Lovelace', email: 'ada@example.com' });
    });

    it('fires onDecline from the "Not now" button', () => {
        const onDecline = vi.fn();
        render(<IdentityIntakeCard spec={SPEC} onSubmit={vi.fn()} onDecline={onDecline} />);
        fireEvent.click(screen.getByRole('button', { name: 'Not now' }));
        expect(onDecline).toHaveBeenCalledTimes(1);
    });

    it('renders a server `interaction_invalid` error against its field, turn still parked', () => {
        const onSubmit = vi.fn();
        render(
            <IdentityIntakeCard
                spec={SPEC}
                onSubmit={onSubmit}
                onDecline={vi.fn()}
                errors={[{ field: 'email', message: 'Enter a valid email address.' }]}
            />,
        );
        const err = screen.getByRole('alert');
        expect(err.textContent).toContain('Enter a valid email address.');
        // The email input is flagged invalid and wired to the error via aria-describedby.
        const email = screen.getByLabelText(/Email/) as HTMLInputElement;
        expect(email.getAttribute('aria-invalid')).toBe('true');
        expect(email.getAttribute('aria-describedby')).toBe(err.id);
    });
});
