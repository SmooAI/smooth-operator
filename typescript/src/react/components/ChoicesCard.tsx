/**
 * ChoicesCard — the web SDK card renderer for the `choices` Rich Interaction
 * (modeled on Claude Code's AskUserQuestion).
 *
 * On a `choices` `interaction_required` event the client looks the card up by
 * `kind` in {@link interactionCards} and renders it in the overlay slot above the
 * composer. Each question shows its `header`, prompt, and option chips (label +
 * gloss): radios when `multiSelect` is false, checkboxes when true — PLUS a
 * free-text **"Other"** escape hatch per question that is ALWAYS available (the
 * ever-present AskUserQuestion "Other"). Submit builds the canonical
 * {@link ChoicesValues} (`{ answers: [{ header, options?, other? }] }`) and hands
 * it to `onSubmit`; Decline calls `onDecline` (the caller sends `declined: true`).
 *
 * Styling: semantic markup with `smooth-chat__*` class names driven by the
 * `--smooth-*` CSS variables in `react/styles.css` (same convention as `parts`);
 * `className` is forwarded so you can layer utilities on top. Server-side
 * validation failures (`interaction_invalid`) can be surfaced per question via
 * `errors` (keyed by the question `header`) — the turn stays parked, resubmit.
 */
import { useEffect, useId, useMemo, useRef, useState, type FormEvent } from 'react';
import type { ChoicesSpec, ChoicesValues } from '../../generated/types.js';

/** One enumerated option within a question. */
export interface ChoiceOption {
    label: string;
    description?: string;
}

/** A single question in a `choices` spec (the ergonomic, array-friendly view of
 * the generated tuple type). */
export interface ChoiceQuestion {
    question: string;
    header: string;
    options: ChoiceOption[];
    multiSelect?: boolean;
}

/** A per-question validation error surfaced from an `interaction_invalid` event.
 * `field` is the question's `header`. */
export interface ChoiceError {
    field: string;
    message: string;
}

export interface ChoicesCardProps {
    /** The `choices` spec carried on the `interaction_required` event. */
    spec: ChoicesSpec;
    /** Human-readable reason the agent raised the ask (card header). */
    reason?: string;
    /** Called with the canonical values when the visitor submits a complete answer. */
    onSubmit: (values: ChoicesValues) => void;
    /** Called when the visitor declines the interaction. */
    onDecline: () => void;
    /** Per-question server validation errors (keyed by question `header`). */
    errors?: ChoiceError[];
    /** Disable all controls (e.g. while a submit is in flight). */
    busy?: boolean;
    className?: string;
}

/** Sentinel selection marking the single-select "Other" radio as chosen — kept
 * out of the submitted `options` (it maps to the free-text `other` instead). */
const OTHER = '__other__';

/** Per-question working state: chosen enumerated labels + the free-text `other`. */
interface QState {
    selected: string[];
    other: string;
}

function cx(...parts: (string | false | undefined)[]): string {
    return parts.filter(Boolean).join(' ');
}

/** Coerce the generated tuple `spec.questions` into a plain array. */
function questionsOf(spec: ChoicesSpec): ChoiceQuestion[] {
    return (spec.questions as unknown as ChoiceQuestion[]) ?? [];
}

/**
 * Build the canonical {@link ChoicesValues} from the card's working state.
 *
 * Pure + exported so the payload shape is unit-testable without a DOM. Per the
 * schema Values: `other` is trimmed and dropped when blank; the single-select
 * OTHER sentinel is stripped so a free-text single answer submits as `other`
 * alone (options omitted); a question with no selection and no `other` still
 * emits an entry (the server validator rejects the empty answer, keeping the
 * turn parked — the UI blocks submit before it gets that far).
 */
export function buildChoicesValues(questions: ChoiceQuestion[], state: Record<string, QState>): ChoicesValues {
    return {
        answers: questions.map((q) => {
            const s = state[q.header] ?? { selected: [], other: '' };
            const enumerated = s.selected.filter((l) => l !== OTHER);
            const other = s.other.trim();
            const answer: ChoicesValues['answers'][number] = { header: q.header };
            if (enumerated.length) answer.options = enumerated;
            if (other) answer.other = other;
            return answer;
        }),
    };
}

/** Whether every question has an answer (a selected label or non-blank `other`). */
function isComplete(questions: ChoiceQuestion[], state: Record<string, QState>): boolean {
    return questions.every((q) => {
        const s = state[q.header] ?? { selected: [], other: '' };
        return s.selected.some((l) => l !== OTHER) || s.other.trim().length > 0;
    });
}

export function ChoicesCard({ spec, reason, onSubmit, onDecline, errors, busy, className }: ChoicesCardProps) {
    const questions = useMemo(() => questionsOf(spec), [spec]);
    const groupId = useId();
    const firstControlRef = useRef<HTMLInputElement>(null);

    const [state, setState] = useState<Record<string, QState>>(() => Object.fromEntries(questions.map((q) => [q.header, { selected: [], other: '' }])));

    // Move focus into the card when it appears so a keyboard visitor lands on the
    // first option without a manual tab into the overlay.
    useEffect(() => {
        firstControlRef.current?.focus();
    }, []);

    const errorFor = (header: string) => errors?.find((e) => e.field === header)?.message;

    const setQ = (header: string, next: Partial<QState>) => setState((prev) => ({ ...prev, [header]: { ...(prev[header] ?? { selected: [], other: '' }), ...next } }));

    const pickSingle = (header: string, label: string) => setQ(header, { selected: [label] });
    const toggleMulti = (header: string, label: string) =>
        setState((prev) => {
            const cur = prev[header] ?? { selected: [], other: '' };
            const has = cur.selected.includes(label);
            return { ...prev, [header]: { ...cur, selected: has ? cur.selected.filter((l) => l !== label) : [...cur.selected, label] } };
        });

    const complete = isComplete(questions, state);

    const handleSubmit = (e: FormEvent) => {
        e.preventDefault();
        if (!complete || busy) return;
        onSubmit(buildChoicesValues(questions, state));
    };

    return (
        <form className={cx('smooth-chat__interaction', 'smooth-chat__interaction--choices', className)} onSubmit={handleSubmit} aria-label="Choose an answer">
            {reason ? <p className="smooth-chat__interaction-reason">{reason}</p> : null}

            {questions.map((q, qi) => {
                const s = state[q.header] ?? { selected: [], other: '' };
                const otherActive = s.selected.includes(OTHER);
                const err = errorFor(q.header);
                const errId = err ? `${groupId}-${qi}-err` : undefined;
                return (
                    <fieldset key={q.header} className="smooth-chat__interaction-question" aria-describedby={errId}>
                        <legend className="smooth-chat__interaction-legend">
                            <span className="smooth-chat__interaction-header">{q.header}</span>
                            <span className="smooth-chat__interaction-prompt">{q.question}</span>
                        </legend>

                        {q.options.map((opt, oi) => {
                            const id = `${groupId}-${qi}-${oi}`;
                            const checked = q.multiSelect ? s.selected.includes(opt.label) : s.selected[0] === opt.label;
                            return (
                                <label key={opt.label} htmlFor={id} className={cx('smooth-chat__interaction-option', checked && 'smooth-chat__interaction-option--checked')}>
                                    <input
                                        ref={qi === 0 && oi === 0 ? firstControlRef : undefined}
                                        id={id}
                                        type={q.multiSelect ? 'checkbox' : 'radio'}
                                        name={`${groupId}-${qi}`}
                                        checked={checked}
                                        disabled={busy}
                                        onChange={() => (q.multiSelect ? toggleMulti(q.header, opt.label) : pickSingle(q.header, opt.label))}
                                    />
                                    <span className="smooth-chat__interaction-option-body">
                                        <span className="smooth-chat__interaction-option-label">{opt.label}</span>
                                        {opt.description ? <span className="smooth-chat__interaction-option-desc">{opt.description}</span> : null}
                                    </span>
                                </label>
                            );
                        })}

                        {/* Free-text "Other" — always available (the AskUserQuestion escape hatch). */}
                        <label htmlFor={`${groupId}-${qi}-other`} className={cx('smooth-chat__interaction-option', 'smooth-chat__interaction-other')}>
                            {q.multiSelect ? null : (
                                <input
                                    type="radio"
                                    name={`${groupId}-${qi}`}
                                    checked={otherActive}
                                    disabled={busy}
                                    aria-label={`Choose other for ${q.header}`}
                                    onChange={() => setQ(q.header, { selected: [OTHER] })}
                                />
                            )}
                            <input
                                id={`${groupId}-${qi}-other`}
                                type="text"
                                className="smooth-chat__interaction-other-input"
                                placeholder="Other…"
                                value={s.other}
                                disabled={busy}
                                aria-label={`Other answer for ${q.header}`}
                                onChange={(e) => setQ(q.header, { other: e.target.value, ...(q.multiSelect ? {} : { selected: e.target.value ? [OTHER] : [] }) })}
                                onFocus={() => (q.multiSelect ? undefined : setQ(q.header, { selected: [OTHER] }))}
                            />
                        </label>

                        {err ? (
                            <p id={errId} className="smooth-chat__interaction-error" role="alert">
                                {err}
                            </p>
                        ) : null}
                    </fieldset>
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

/**
 * The widget's card registry — `kind` → card component. `interaction_required`
 * looks the card up by `kind` and renders it in the overlay slot. Registering a
 * card here IS declaring the kind's render capability; adding a kind is one card
 * component + one entry. (`identity_intake`'s card lives in the chat-widget repo;
 * `choices` is registered here.)
 */
export const interactionCards = {
    choices: ChoicesCard,
} as const;
