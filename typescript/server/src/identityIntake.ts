/**
 * Identity intake — channel-normalized lead/identity capture: a **Rich
 * Interaction kind** (see {@link ./interaction.js}), the sibling of {@link ./choices.js}.
 *
 * - On a channel that declared the `identity_form` capability, the agent's
 *   `request_identity_intake` tool parks the turn and the server emits
 *   `interaction_required { kind: "identity_intake" }`; the client's form
 *   resumes with a `submit_interaction` action.
 * - On a **text-only** channel, the same raise degrades to a conversational
 *   directive and the model submits collected values through the generic
 *   `submit_interaction` *tool*.
 *
 * Both paths validate through {@link validateIntake} — one implementation, one
 * behavior — and resume the turn with the same structured payload. On a valid
 * submit the server's kind-routed **host effect** stamps the captured contacts
 * onto the session (`userName` / `contactEmail` / `contactPhone` — the same keys
 * the pre-chat create path stashes and the OTP contact seam reads), so a
 * captured contact is immediately OTP-verifiable.
 *
 * The TypeScript port of the Rust reference `smooth-operator/src/identity_intake.rs`.
 */
import type { InteractionKind, InteractionRequest, InteractionValidation } from './interaction.js';

/** The closed set of identity fields intake can collect. */
export type IntakeFieldKey = 'name' | 'email' | 'phone';

const INTAKE_FIELD_KEYS: readonly IntakeFieldKey[] = ['name', 'email', 'phone'];

/** One requested identity field, as raised by the agent's `request_identity_intake` tool. */
export interface IntakeField {
    /** Which identity field to collect. */
    key: IntakeFieldKey;
    /** Whether the visitor must provide this field to submit. */
    required: boolean;
    /** Optional display label overriding the client's default. */
    label?: string;
}

/**
 * Validated, normalized identity values — the structured payload the parked turn
 * resumes with (identical on the form and conversational paths).
 */
export interface IntakeValues {
    name?: string;
    email?: string;
    /** E.164-normalized (`+15551234567`). */
    phone?: string;
}

/** A single per-field validation failure. `field` is the {@link IntakeFieldKey}. */
export interface IntakeFieldError {
    field: string;
    message: string;
}

/** True when no field carries a value. */
function isEmpty(values: IntakeValues): boolean {
    return values.name === undefined && values.email === undefined && values.phone === undefined;
}

/**
 * Minimal email-shape validation: exactly one `@`, non-empty local part, a
 * dot-containing domain, no whitespace. Returns the trimmed address with a
 * lowercased domain, or `undefined` when malformed.
 *
 * ponytail: shape check, not RFC 5322 — a deliverability check belongs to the
 * host's email service, not the protocol boundary.
 */
export function normalizeEmail(raw: string): string | undefined {
    const s = raw.trim();
    if (/\s/.test(s)) return undefined;
    const at = s.indexOf('@');
    if (at <= 0) return undefined; // no `@`, or empty local part
    const local = s.slice(0, at);
    const domain = s.slice(at + 1);
    if (domain.includes('@')) return undefined; // more than one `@`
    // Domain needs an interior dot: `a.b`, not `.b`, `a.`, or `ab`.
    const domainLc = domain.toLowerCase();
    const parts = domainLc.split('.');
    if (parts.length < 2 || parts.some((p) => p.length === 0)) return undefined;
    return `${local}@${domainLc}`;
}

/**
 * Normalize a phone number to E.164, or `undefined` when unparseable.
 *
 * Strips common separators (space, `-`, `.`, `(`, `)`), then accepts:
 * - `+` + 8–15 digits (already E.164),
 * - a bare 10-digit number or a 1-prefixed 11-digit number → `+1…` (NANP).
 *
 * ponytail: NANP default for bare national numbers; swap in a phone library if
 * non-NANP national formats ever need to parse.
 */
export function normalizePhoneE164(raw: string): string | undefined {
    const s = raw
        .trim()
        .split('')
        .filter((c) => !(c === ' ' || c === '-' || c === '.' || c === '(' || c === ')'))
        .join('');
    const plus = s.startsWith('+');
    const digits = plus ? s.slice(1) : s;
    if (digits.length === 0 || !/^\d+$/.test(digits)) return undefined;
    if (plus) {
        // E.164: country code can't start with 0; total 8–15 digits.
        if (digits.length >= 8 && digits.length <= 15 && !digits.startsWith('0')) return `+${digits}`;
        return undefined;
    }
    if (digits.length === 10) return `+1${digits}`;
    if (digits.length === 11 && digits.startsWith('1')) return `+${digits}`;
    return undefined;
}

/**
 * Validate raw submitted `values` against the requested `fields`, returning the
 * normalized {@link IntakeValues} or the full list of per-field errors.
 *
 * Rules (mirrors `identity_intake.rs`):
 * - every `required` field must be present and non-blank;
 * - `name`: non-empty after trim;
 * - `email`: `local@domain.tld` shape (single `@`, dot in the domain, no whitespace); domain lowercased;
 * - `phone`: E.164 after stripping separators; bare 10-digit / 1-prefixed 11-digit NANP → `+1…`.
 *
 * Fields that were *not* requested but are present are still validated and kept —
 * a visitor volunteering their phone is a gift, not an error. Returns every
 * failed field (not just the first) so a form can annotate all of them at once.
 */
export function validateIntake(fields: IntakeField[], values: IntakeValues): { ok: true; values: IntakeValues } | { ok: false; errors: IntakeFieldError[] } {
    const errors: IntakeFieldError[] = [];
    const out: IntakeValues = {};

    const raw = (key: IntakeFieldKey): string | undefined => values[key];

    // Required-ness: every required requested field must be present + non-blank.
    for (const field of fields) {
        if (field.required) {
            const v = raw(field.key);
            if (v === undefined || v.trim().length === 0) {
                errors.push({ field: field.key, message: 'this field is required' });
            }
        }
    }

    // Format validation + normalization for whatever was provided.
    if (values.name !== undefined) {
        const trimmed = values.name.trim();
        if (trimmed.length > 0) out.name = trimmed;
    }
    if (values.email !== undefined) {
        const trimmed = values.email.trim();
        if (trimmed.length > 0) {
            const normalized = normalizeEmail(trimmed);
            if (normalized !== undefined) out.email = normalized;
            else errors.push({ field: 'email', message: 'must be a valid email address' });
        }
    }
    if (values.phone !== undefined) {
        const trimmed = values.phone.trim();
        if (trimmed.length > 0) {
            const normalized = normalizePhoneE164(trimmed);
            if (normalized !== undefined) out.phone = normalized;
            else errors.push({ field: 'phone', message: 'must be a valid phone number (include your country code, e.g. +1 555 123 4567)' });
        }
    }

    return errors.length === 0 ? { ok: true, values: out } : { ok: false, errors };
}

/**
 * Parse the raise tool's `fields` argument into validated {@link IntakeField}s.
 * Accepts both the structured form (`[{ key, required?, label? }]`) and the
 * shorthand models like to emit (`["email", "name"]` — shorthand fields are
 * `required: true`). Unknown keys are an error (closed set).
 */
export function parseFields(raw: unknown): IntakeField[] {
    if (!Array.isArray(raw)) throw new Error("'fields' must be an array");
    if (raw.length === 0) throw new Error("'fields' must contain at least one field");

    const parseKey = (s: string): IntakeFieldKey => {
        if ((INTAKE_FIELD_KEYS as readonly string[]).includes(s)) return s as IntakeFieldKey;
        throw new Error(`unknown intake field '${s}' (expected name, email, or phone)`);
    };

    const fields: IntakeField[] = [];
    for (const item of raw) {
        if (typeof item === 'string') {
            fields.push({ key: parseKey(item), required: true });
        } else if (typeof item === 'object' && item !== null && !Array.isArray(item)) {
            const obj = item as Record<string, unknown>;
            const key = typeof obj.key === 'string' ? obj.key : undefined;
            if (key === undefined) throw new Error("each field object needs a string 'key'");
            const field: IntakeField = { key: parseKey(key), required: typeof obj.required === 'boolean' ? obj.required : true };
            if (typeof obj.label === 'string') field.label = obj.label;
            fields.push(field);
        } else {
            throw new Error(`invalid field entry: ${JSON.stringify(item)}`);
        }
    }
    return fields;
}

/** Coerce the submitted `values` into {@link IntakeValues}, keeping only string fields. */
function parseValues(values: unknown): IntakeValues {
    if (typeof values !== 'object' || values === null || Array.isArray(values)) throw new Error('values must be an object');
    const o = values as Record<string, unknown>;
    const out: IntakeValues = {};
    if (typeof o.name === 'string') out.name = o.name;
    if (typeof o.email === 'string') out.email = o.email;
    if (typeof o.phone === 'string') out.phone = o.phone;
    return out;
}

/** Read the requested `fields` out of a raise spec (best-effort; malformed/absent ⇒ format-only []). */
function specFields(spec: unknown): IntakeField[] {
    if (typeof spec !== 'object' || spec === null) return [];
    const raw = (spec as Record<string, unknown>).fields;
    if (!Array.isArray(raw)) return [];
    const fields: IntakeField[] = [];
    for (const f of raw) {
        if (typeof f !== 'object' || f === null) continue;
        const o = f as Record<string, unknown>;
        if (typeof o.key !== 'string' || !(INTAKE_FIELD_KEYS as readonly string[]).includes(o.key)) continue;
        const field: IntakeField = { key: o.key as IntakeFieldKey, required: o.required === true };
        if (typeof o.label === 'string') field.label = o.label;
        fields.push(field);
    }
    return fields;
}

/**
 * The `identity_intake` Rich Interaction kind — structured name/email/phone lead
 * capture (see the module docs and `spec/interactions/identity-intake.schema.json`).
 */
export class IdentityIntakeKind implements InteractionKind {
    readonly kind = 'identity_intake';
    readonly capability = 'identity_form';

    toolSchema(): { name: string; description: string; parameters: Record<string, unknown> } {
        return {
            name: 'request_identity_intake',
            description:
                'Ask the visitor for their contact details (name, email, and/or phone) in a channel-appropriate way. '
                + 'On channels that can render a form the visitor fills a structured form; on text channels you will be '
                + 'told to collect the fields conversationally. Always use this tool instead of free-forming a request '
                + 'for contact details.',
            parameters: {
                type: 'object',
                properties: {
                    fields: {
                        type: 'array',
                        minItems: 1,
                        description: 'Which fields to collect, in order. Each entry is either a string ("name" | "email" | "phone") or an object { key, required?, label? }.',
                        items: {
                            anyOf: [
                                { type: 'string', enum: ['name', 'email', 'phone'] },
                                {
                                    type: 'object',
                                    properties: {
                                        key: { type: 'string', enum: ['name', 'email', 'phone'] },
                                        required: { type: 'boolean' },
                                        label: { type: 'string' },
                                    },
                                    required: ['key'],
                                },
                            ],
                        },
                    },
                    reason: {
                        type: 'string',
                        description: 'Why you need these details, phrased for the visitor (e.g. "to send you the quote").',
                    },
                },
                required: ['fields', 'reason'],
            },
        };
    }

    parseRequest(args: Record<string, unknown>): InteractionRequest {
        const fields = parseFields(args.fields ?? null);
        const reason = typeof args.reason === 'string' && args.reason.trim().length > 0 ? args.reason.trim() : 'to help you better';
        return { kind: this.kind, spec: { fields }, reason };
    }

    validate(spec: unknown, values: unknown): InteractionValidation {
        // The spec's fields drive required-ness; an absent spec (fallback raise from
        // an earlier turn) degrades to format-only validation.
        const fields = specFields(spec);
        let parsed: IntakeValues;
        try {
            parsed = parseValues(values);
        } catch (err) {
            return { ok: false, errors: [{ field: 'values', message: `invalid values shape: ${err instanceof Error ? err.message : String(err)}` }] };
        }
        if (isEmpty(parsed)) {
            return { ok: false, errors: [{ field: 'values', message: 'provide at least one of name/email/phone, or declined=true' }] };
        }
        const result = validateIntake(fields, parsed);
        return result.ok ? { ok: true, values: result.values } : { ok: false, errors: result.errors };
    }

    fallbackDirective(spec: unknown, reason: string): string {
        const fieldList = specFields(spec)
            .map((f) => f.key)
            .join(', ');
        return (
            "This visitor's channel cannot display a form. Collect the requested details "
            + `(${fieldList}) conversationally: ask for ONE field at a time, in the order given, naturally weaving in the `
            + `reason (${reason}). When you have the values, call the \`submit_interaction\` tool with kind "identity_intake" `
            + 'and the values — it validates each field and will tell you if something looks wrong so you can re-ask. '
            + 'If the visitor declines to share, call `submit_interaction` with declined=true and continue helping them '
            + 'without the details.'
        );
    }
}
