/**
 * The `identity_intake` Rich Interaction kind — validator + kind wiring + the
 * shared conformance fixtures.
 *
 * The TS parity of the Rust `identity_intake.rs` unit tests: email/phone
 * normalization, the one-pass required/optional + per-field validation, the
 * raise-tool argument contract (`parseFields`), the kind's
 * `parseRequest`/`validate`/`fallbackDirective`, and the shared `identity_intake`
 * fixtures in `spec/conformance/fixtures.json` (so a drift in any one server is
 * caught here).
 */
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { describe, expect, it } from 'vitest';

import { IdentityIntakeKind, normalizeEmail, normalizePhoneE164, parseFields, validateIntake, type IntakeField, type IntakeValues } from '../src/identityIntake.js';

const __dirname = dirname(fileURLToPath(import.meta.url));
const SPEC_DIR = join(__dirname, '..', '..', '..', 'spec');
const fixtures = JSON.parse(readFileSync(join(SPEC_DIR, 'conformance', 'fixtures.json'), 'utf8')) as Record<string, { instance: Record<string, unknown> } | string>;
function fixture(name: string): Record<string, unknown> {
    const entry = fixtures[name];
    if (!entry || typeof entry === 'string') throw new Error(`missing fixture ${name}`);
    return entry.instance;
}

function field(key: IntakeField['key'], required: boolean): IntakeField {
    return { key, required };
}

describe('normalizeEmail', () => {
    it('lowercases the domain, preserves the local case', () => {
        expect(normalizeEmail('Alice@Example.COM')).toBe('Alice@example.com');
    });
    it('rejects malformed addresses', () => {
        for (const bad of ['', 'no-at', '@x.com', 'a@b', 'a@.com', 'a@b.', 'a b@c.com', 'a@b@c.com']) {
            expect(normalizeEmail(bad), `${bad} should be rejected`).toBeUndefined();
        }
    });
});

describe('normalizePhoneE164', () => {
    it('normalizes NANP and keeps country-coded numbers', () => {
        expect(normalizePhoneE164('+1 (555) 123-4567')).toBe('+15551234567');
        expect(normalizePhoneE164('555.123.4567')).toBe('+15551234567'); // bare 10-digit
        expect(normalizePhoneE164('1 555 123 4567')).toBe('+15551234567'); // 1-prefixed 11-digit
        expect(normalizePhoneE164('+447911123456')).toBe('+447911123456'); // non-NANP with country code
    });
    it('rejects unparseable numbers', () => {
        for (const bad of ['', 'abc', '+0123456789', '12345', '+1234567890123456']) {
            expect(normalizePhoneE164(bad), `${bad} should be rejected`).toBeUndefined();
        }
    });
});

describe('validateIntake', () => {
    it('a required field missing (or blank) is an error', () => {
        const fields = [field('email', true), field('name', false)];
        const err = validateIntake(fields, {});
        expect(err.ok).toBe(false);
        if (!err.ok) {
            expect(err.errors).toHaveLength(1);
            expect(err.errors[0]!.field).toBe('email');
        }
        // Blank counts as missing.
        expect(validateIntake(fields, { email: '   ' }).ok).toBe(false);
    });

    it('an optional field left absent is fine', () => {
        const out = validateIntake([field('name', false), field('email', false)], { name: 'Bob' });
        expect(out.ok).toBe(true);
        if (out.ok) expect(out.values).toEqual({ name: 'Bob' });
    });

    it('a valid submit trims + normalizes every field', () => {
        const fields = [field('email', true), field('phone', false)];
        const out = validateIntake(fields, { name: '  Alice Example  ', email: 'alice@Example.com', phone: '(555) 123-4567' });
        expect(out.ok).toBe(true);
        if (out.ok) {
            expect(out.values.name).toBe('Alice Example');
            expect(out.values.email).toBe('alice@example.com');
            expect(out.values.phone).toBe('+15551234567');
        }
    });

    it('a bad email → a per-field email error', () => {
        const out = validateIntake([field('email', true)], { email: 'not-an-email' });
        expect(out.ok).toBe(false);
        if (!out.ok) {
            expect(out.errors).toHaveLength(1);
            expect(out.errors[0]!.field).toBe('email');
            expect(out.errors[0]!.message).toContain('valid email');
        }
    });

    it('reports missing name + bad email + bad phone in one pass', () => {
        const out = validateIntake([field('name', true)], { email: 'not-an-email', phone: 'nope' });
        expect(out.ok).toBe(false);
        if (!out.ok) expect(out.errors).toHaveLength(3);
    });

    it('keeps a volunteered field that was not requested', () => {
        const out = validateIntake([field('email', true)], { email: 'a@b.co', phone: '+15551234567' });
        expect(out.ok).toBe(true);
        if (out.ok) expect(out.values.phone).toBe('+15551234567');
    });
});

describe('parseFields — the raise-tool contract', () => {
    it('accepts the shorthand string form (required: true) and the object form', () => {
        const shorthand = parseFields(['email', 'name']);
        expect(shorthand).toEqual([{ key: 'email', required: true }, { key: 'name', required: true }]);
        const structured = parseFields([{ key: 'phone', required: false, label: 'Mobile' }]);
        expect(structured).toEqual([{ key: 'phone', required: false, label: 'Mobile' }]);
    });
    it('rejects an empty array, a non-array, and an unknown key', () => {
        expect(() => parseFields([])).toThrow(/at least one/);
        expect(() => parseFields('nope')).toThrow(/must be an array/);
        expect(() => parseFields(['address'])).toThrow(/unknown intake field/);
    });
});

describe('IdentityIntakeKind', () => {
    it('exposes the reference identity + raise-tool surface', () => {
        const kind = new IdentityIntakeKind();
        expect(kind.kind).toBe('identity_intake');
        expect(kind.capability).toBe('identity_form');
        expect(kind.toolSchema().name).toBe('request_identity_intake');
    });

    it('parseRequest canonicalizes fields + reason', () => {
        const req = new IdentityIntakeKind().parseRequest({ fields: ['email', { key: 'phone', required: false }], reason: 'to send you the quote' });
        expect(req.kind).toBe('identity_intake');
        expect(req.reason).toBe('to send you the quote');
        expect((req.spec as { fields: IntakeField[] }).fields).toEqual([{ key: 'email', required: true }, { key: 'phone', required: false }]);
    });

    it('fallbackDirective lists the fields and points at submit_interaction', () => {
        const kind = new IdentityIntakeKind();
        const req = kind.parseRequest({ fields: ['name', 'email'], reason: 'to follow up' });
        const directive = kind.fallbackDirective(req.spec, 'to follow up');
        expect(directive).toContain('name, email');
        expect(directive).toContain('submit_interaction');
    });

    it('validate rejects an all-empty submit', () => {
        const out = new IdentityIntakeKind().validate({ fields: [{ key: 'email', required: true }] }, {});
        expect(out.ok).toBe(false);
        if (!out.ok) expect(out.errors[0]!.field).toBe('values');
    });
});

describe('the shared identity_intake fixtures', () => {
    it('validate(identity_intake_spec, identity_intake_values) produces identity_intake_payload.values', () => {
        const spec = fixture('identity_intake_spec');
        const values = fixture('identity_intake_values') as IntakeValues;
        const payload = fixture('identity_intake_payload');
        const out = new IdentityIntakeKind().validate(spec, values);
        expect(out.ok).toBe(true);
        if (out.ok) expect(out.values).toEqual((payload as { values: unknown }).values);
    });
});
