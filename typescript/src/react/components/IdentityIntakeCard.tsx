/**
 * IdentityIntakeCard — the web SDK card renderer for the `identity_intake` Rich
 * Interaction (structured name/email/phone lead capture). Modeled on
 * {@link ChoicesCard}.
 *
 * On an `identity_intake` `interaction_required` event the client looks the card
 * up by `kind` in `interactionCards` and renders it in the overlay slot above the
 * composer. It renders one labelled input per `field` in `spec.fields` order
 * (name → text, email → email, phone → tel), marking required fields. Submit
 * builds the canonical {@link IdentityIntakeValues} (`{ name?, email?, phone? }`,
 * only the fields the visitor filled in) and hands it to `onSubmit`; Decline calls
 * `onDecline` (the caller sends `declined: true`).
 *
 * The SERVER is the validator (email shape, phone → E.164, required present); the
 * client gates Submit on required-field presence as a nicety, but surfaces the
 * server's `interaction_invalid` field errors per input, keeping the turn parked.
 *
 * Styling: semantic markup with `smooth-chat__*` class names driven by the
 * `--smooth-*` CSS variables in `react/styles.css` (same convention as
 * {@link ChoicesCard}); `className` is forwarded so you can layer utilities on top.
 */
import { useEffect, useId, useMemo, useRef, useState, type FormEvent } from 'react';
import type { IdentityIntakeSpec, IdentityIntakeValues } from '../../generated/types.js';

/** Which identity fields the card knows how to render. */
export type IdentityFieldKey = 'name' | 'email' | 'phone';

/** A single field in an `identity_intake` spec (the ergonomic, array-friendly view
 * of the generated tuple type). */
export interface IdentityField {
    key: IdentityFieldKey;
    required: boolean;
    label?: string;
}

/** A per-field validation error surfaced from an `interaction_invalid` event.
 * `field` is the field `key` (`name` / `email` / `phone`). */
export interface IdentityError {
    field: string;
    message: string;
}

export interface IdentityIntakeCardProps {
    /** The `identity_intake` spec carried on the `interaction_required` event. */
    spec: IdentityIntakeSpec;
    /** Human-readable reason the agent raised the ask (card header). */
    reason?: string;
    /** Called with the canonical values (only the filled-in fields) on submit. */
    onSubmit: (values: IdentityIntakeValues) => void;
    /** Called when the visitor declines the interaction. */
    onDecline: () => void;
    /** Per-field server validation errors (keyed by field `key`). */
    errors?: IdentityError[];
    /** Disable all controls (e.g. while a submit is in flight). */
    busy?: boolean;
    className?: string;
}

/** Default label + input type per field. `label` on the spec field overrides the
 * label; the input type is fixed by the field kind. */
const FIELD_META: Record<IdentityFieldKey, { label: string; type: 'text' | 'email' | 'tel'; autoComplete: string; inputMode?: 'email' | 'tel' }> = {
    name: { label: 'Name', type: 'text', autoComplete: 'name' },
    email: { label: 'Email', type: 'email', autoComplete: 'email', inputMode: 'email' },
    phone: { label: 'Phone', type: 'tel', autoComplete: 'tel', inputMode: 'tel' },
};

function cx(...parts: (string | false | undefined)[]): string {
    return parts.filter(Boolean).join(' ');
}

/** Coerce the generated tuple `spec.fields` into a plain array. */
function fieldsOf(spec: IdentityIntakeSpec): IdentityField[] {
    return (spec.fields as unknown as IdentityField[]) ?? [];
}

/**
 * Build the canonical {@link IdentityIntakeValues} from the card's working state.
 *
 * Pure + exported so the payload shape is unit-testable without a DOM. Only fields
 * present in the spec with a non-blank (trimmed) value are emitted — an empty
 * optional field is omitted entirely rather than sent as `""`.
 */
export function buildIdentityValues(fields: IdentityField[], state: Record<string, string>): IdentityIntakeValues {
    const values: IdentityIntakeValues = {};
    for (const f of fields) {
        const v = (state[f.key] ?? '').trim();
        if (v) values[f.key] = v;
    }
    return values;
}

/** Whether every required field has a non-blank value (client-side submit gate;
 * the server remains the authoritative validator). */
function isComplete(fields: IdentityField[], state: Record<string, string>): boolean {
    return fields.every((f) => !f.required || (state[f.key] ?? '').trim().length > 0);
}

export function IdentityIntakeCard({ spec, reason, onSubmit, onDecline, errors, busy, className }: IdentityIntakeCardProps) {
    const fields = useMemo(() => fieldsOf(spec), [spec]);
    const groupId = useId();
    const firstInputRef = useRef<HTMLInputElement>(null);

    const [state, setState] = useState<Record<string, string>>(() => Object.fromEntries(fields.map((f) => [f.key, ''])));

    // Move focus into the card when it appears so a keyboard visitor lands on the
    // first field without a manual tab into the overlay.
    useEffect(() => {
        firstInputRef.current?.focus();
    }, []);

    const errorFor = (key: string) => errors?.find((e) => e.field === key)?.message;
    const setField = (key: string, value: string) => setState((prev) => ({ ...prev, [key]: value }));

    const complete = isComplete(fields, state);

    const handleSubmit = (e: FormEvent) => {
        e.preventDefault();
        if (!complete || busy) return;
        onSubmit(buildIdentityValues(fields, state));
    };

    return (
        <form className={cx('smooth-chat__interaction', 'smooth-chat__interaction--identity', className)} onSubmit={handleSubmit} aria-label="Share your contact details">
            {reason ? <p className="smooth-chat__interaction-reason">{reason}</p> : null}

            {fields.map((f, fi) => {
                const meta = FIELD_META[f.key];
                const id = `${groupId}-${fi}`;
                const err = errorFor(f.key);
                const errId = err ? `${id}-err` : undefined;
                const label = f.label ?? meta.label;
                return (
                    <div key={f.key} className="smooth-chat__interaction-field">
                        <label htmlFor={id} className="smooth-chat__interaction-field-label">
                            <span>{label}</span>
                            {f.required ? (
                                <span className="smooth-chat__interaction-required" aria-hidden="true">
                                    *
                                </span>
                            ) : null}
                        </label>
                        <input
                            ref={fi === 0 ? firstInputRef : undefined}
                            id={id}
                            type={meta.type}
                            name={f.key}
                            value={state[f.key] ?? ''}
                            required={f.required}
                            aria-required={f.required}
                            aria-invalid={err ? true : undefined}
                            aria-describedby={errId}
                            autoComplete={meta.autoComplete}
                            inputMode={meta.inputMode}
                            disabled={busy}
                            className={cx('smooth-chat__interaction-input', err && 'smooth-chat__interaction-input--invalid')}
                            onChange={(e) => setField(f.key, e.target.value)}
                        />
                        {err ? (
                            <p id={errId} className="smooth-chat__interaction-error" role="alert">
                                {err}
                            </p>
                        ) : null}
                    </div>
                );
            })}

            <div className="smooth-chat__interaction-actions">
                <button type="submit" className="smooth-chat__interaction-submit" disabled={!complete || busy}>
                    Submit
                </button>
                <button type="button" className="smooth-chat__interaction-decline" disabled={busy} onClick={onDecline}>
                    Not now
                </button>
            </div>
        </form>
    );
}
