/**
 * Rich Interactions — the kind-agnostic park/resume framework.
 *
 * One pattern, many kinds: an agent raises a **structured interaction** (choice
 * chips, an identity form, a date picker, …). On a channel whose client declared
 * the kind's render capability (`supports` at `create_conversation_session`), the
 * turn parks and the client renders a rich card (`interaction_required` →
 * `submit_interaction`). On a text-only channel the same raise degrades to the
 * kind's **conversational fallback**: a directive the model follows turn by turn,
 * submitting through the generic `submit_interaction` *tool*. Both paths run the
 * kind's **server-side validator** and resume the turn with the same canonical
 * payload.
 *
 * The TypeScript port of the Rust reference `smooth-operator/src/interaction.rs`
 * (the {@link InteractionKind} seam + {@link InteractionRegistry}) and
 * `smooth-operator/src/tools/interaction.rs` (the raise/submit tools). The park
 * registry ({@link InteractionParkRegistry}) is the interaction analog of the
 * write-confirmation {@link ./confirmation.js} registry: a raise tool parks by
 * awaiting a promise the `submit_interaction` action resolves.
 *
 * Adding a kind = implementing {@link InteractionKind} (one module, e.g.
 * {@link ./choices.js}) and registering it in an {@link InteractionRegistry}. No
 * new protocol events, no new client verbs.
 */
import { randomUUID } from 'node:crypto';

import type { Tool } from '@smooai/smooth-operator-core';

import * as protocol from './protocol.js';
import { toolCallStateFrom, type Sink } from './turnRunner.js';

/** Wire name of the generic conversational submit tool (same verb as the resume action). */
export const SUBMIT_INTERACTION_TOOL = 'submit_interaction';

/**
 * How long a parked interaction waits for a `submit_interaction` action before the
 * raise tool gives up and lets the turn continue without the details. Generous — a
 * human is filling a card.
 */
export const INTERACTION_TIMEOUT_MS = 300_000;

/** A single per-field validation failure, carried on `interaction_invalid` and the fallback tool error. */
export interface InteractionFieldError {
    /** The kind-specific field that failed (choices: the question `header`). */
    field: string;
    /** Human-readable validation message. */
    message: string;
}

/** A parsed raise: what the agent asked the visitor for. */
export interface InteractionRequest {
    /** The interaction kind (e.g. `choices`). */
    kind: string;
    /** The kind-specific render spec (shape per `spec/interactions/<kind>.schema.json#/$defs/Spec`). */
    spec: unknown;
    /** Why the agent raised it (card header / woven into the conversational ask). */
    reason: string;
}

/** The result of a kind's server-side validator: canonical values or per-field errors. */
export type InteractionValidation = { ok: true; values: unknown } | { ok: false; errors: InteractionFieldError[] };

/** How a parked interaction resolved. `no_response` covers a timeout / teardown / decline-to-answer. */
export type InteractionOutcome = { status: 'submitted'; values: unknown } | { status: 'declined' } | { status: 'no_response' };

/**
 * The kind-routed **host effect** of an accepted interaction — the framework's
 * kind-agnostic side-effect seam. Runs on a valid submit (both the rich WS path
 * and the conversational-fallback tool) with the parked `kind` and its canonical
 * `values`; the host routes by kind (e.g. `identity_intake` → stamp the session's
 * contact metadata) and no-ops for kinds with no effect (e.g. `choices`). Mirrors
 * the Rust reference's `attach_interaction_effect`.
 */
export type InteractionAttach = (kind: string, values: unknown) => void | Promise<void>;

/**
 * One interaction kind — the extension seam. A kind supplies exactly the pieces
 * that differ per interaction; all park / resume / event / registry machinery is
 * shared and kind-agnostic. Mirrors the Rust `InteractionKind` trait.
 */
export interface InteractionKind {
    /** The wire kind id (e.g. `choices`). Selects the client card and the validator. */
    readonly kind: string;
    /** The client render capability that gates the rich path (e.g. `choice_chips`). */
    readonly capability: string;
    /** The raise tool's LLM-facing schema (per-kind). Convention: name it `request_<kind>`. */
    toolSchema(): { name: string; description: string; parameters: Record<string, unknown> };
    /** Parse + canonicalize the raise tool's arguments into the kind's `spec` + reason. Throws on malformed args. */
    parseRequest(args: Record<string, unknown>): InteractionRequest;
    /**
     * Validate submitted values against `spec`, returning the canonical values or
     * every per-field error. `spec` may be `null` on the conversational path when
     * the raise happened in an earlier turn — the kind then applies format-only
     * validation.
     */
    validate(spec: unknown, values: unknown): InteractionValidation;
    /** The conversational-degradation directive for text-only channels. */
    fallbackDirective(spec: unknown, reason: string): string;
}

/**
 * The set of interaction kinds a server hosts. A host builds one with the kinds it
 * offers (see `server.ts` wiring the reference `choices` kind).
 */
export class InteractionRegistry {
    private readonly kinds = new Map<string, InteractionKind>();

    constructor(kinds: InteractionKind[] = []) {
        for (const kind of kinds) this.kinds.set(kind.kind, kind);
    }

    /** Look up a kind by its wire id. */
    get(kind: string): InteractionKind | undefined {
        return this.kinds.get(kind);
    }

    /** Every registered kind, in registration order. */
    all(): InteractionKind[] {
        return [...this.kinds.values()];
    }
}

/** A parked interaction: the metadata the `submit_interaction` action validates against + the resolver. */
interface Pending {
    interactionId: string;
    kind: string;
    spec: unknown;
    settled: boolean;
    resolveFn(outcome: InteractionOutcome): void;
}

/** The peekable identity of a pending interaction (the resolver stays private). */
export interface PendingInteraction {
    interactionId: string;
    kind: string;
    spec: unknown;
}

/**
 * Tracks the in-flight Rich Interaction each parked turn is waiting on, keyed by
 * `sessionId` (at most one per session). One registry per connection — a
 * `submit_interaction` frame and the parked turn it resumes are always on the same
 * connection. The interaction analog of the write-confirmation registry.
 *
 * Single-threaded under the Node event loop, so no locking. `peek` reads without
 * consuming (an invalid submit must leave the turn parked for a resubmit); `resolve`
 * takes the pending out so a duplicate submit is a clean no-op.
 */
export class InteractionParkRegistry {
    private readonly pending = new Map<string, Pending>();

    /**
     * Register (and return) a fresh outcome promise for `sessionId`. Any prior pending
     * interaction for the session is resolved `no_response` first, so a stale parked
     * turn can never be left dangling and the newest raise always wins.
     */
    park(sessionId: string, meta: PendingInteraction): Promise<InteractionOutcome> {
        const prior = this.pending.get(sessionId);
        if (prior && !prior.settled) {
            prior.settled = true;
            prior.resolveFn({ status: 'no_response' });
        }
        let resolveFn!: (outcome: InteractionOutcome) => void;
        const promise = new Promise<InteractionOutcome>((resolve) => {
            resolveFn = resolve;
        });
        this.pending.set(sessionId, { ...meta, settled: false, resolveFn });
        return promise;
    }

    /** The pending interaction for `sessionId` WITHOUT consuming it, or `undefined` if none is parked. */
    peek(sessionId: string): PendingInteraction | undefined {
        const p = this.pending.get(sessionId);
        if (!p || p.settled) return undefined;
        return { interactionId: p.interactionId, kind: p.kind, spec: p.spec };
    }

    /**
     * Resolve the parked turn for `sessionId` with `outcome`. Returns `true` if a
     * pending interaction was resolved, `false` if none was awaiting (a
     * duplicate/stale submit → `NO_PENDING_INTERACTION`).
     */
    resolve(sessionId: string, outcome: InteractionOutcome): boolean {
        const p = this.pending.get(sessionId);
        if (!p || p.settled) return false;
        p.settled = true;
        this.pending.delete(sessionId);
        p.resolveFn(outcome);
        return true;
    }

    /**
     * Drop the registered interaction for `sessionId` (turn ended). Idempotent.
     *
     * Settles it `no_response` on the way out, for the same reason the confirmation
     * registry's `clear` settles: a deleted-but-awaited park leaves the raise tool
     * hanging until {@link INTERACTION_TIMEOUT_MS} elapses. Same verdict
     * {@link rejectAll} uses.
     */
    clear(sessionId: string): void {
        const p = this.pending.get(sessionId);
        this.pending.delete(sessionId);
        if (p && !p.settled) {
            p.settled = true;
            p.resolveFn({ status: 'no_response' });
        }
    }

    /**
     * Resolve every outstanding interaction as `no_response` — called when a
     * connection is torn down so any turn parked on a raise unparks and finishes
     * cleanly (the visitor simply didn't answer the card).
     */
    rejectAll(): void {
        for (const p of this.pending.values()) {
            if (!p.settled) {
                p.settled = true;
                p.resolveFn({ status: 'no_response' });
            }
        }
        this.pending.clear();
    }
}

/** The per-turn stash of specs raised on the conversational path, so the generic submit tool validates with full required-ness. */
export type RaisedSpecs = Map<string, unknown>;

/**
 * Build the per-kind raise tool (`request_<kind>`). On a session that declared the
 * kind's render capability (`rich`) it **parks the turn**: it registers the outcome
 * resolver, emits `interaction_required`, and awaits the visitor's submit. Otherwise
 * it returns immediately with the kind's conversational-fallback directive (and
 * stashes the spec so a same-turn generic `submit_interaction` validates with
 * required-ness). Mirrors the Rust `RequestInteractionTool`.
 */
export function requestInteractionTool(opts: {
    kind: InteractionKind;
    rich: boolean;
    sessionId: string;
    requestId: string;
    sink: Sink;
    park: InteractionParkRegistry;
    raisedSpecs: RaisedSpecs;
    timeoutMs?: number;
}): Tool {
    const { kind, rich, sessionId, requestId, sink, park, raisedSpecs } = opts;
    const timeoutMs = opts.timeoutMs ?? INTERACTION_TIMEOUT_MS;
    const schema = kind.toolSchema();
    return {
        name: schema.name,
        description: schema.description,
        parameters: schema.parameters,
        async execute(args: Record<string, unknown>): Promise<string> {
            // The stream loop DEFERRED this tool's toolCall chunk (see
            // `TurnRunner.isInteractionRaise`) so the park path can emit it AFTER
            // `interaction_required` — the reference order, and the same order the
            // write-confirmation park already uses. Every non-park exit emits it here.
            const emitCall = (): void => sink(protocol.streamChunk(requestId, schema.name, toolCallStateFrom(schema.name, args)));

            // A malformed raise throws; the engine surfaces it to the model as a tool
            // error (never crashes the turn), so the model can correct and retry.
            let request: ReturnType<typeof kind.parseRequest>;
            try {
                request = kind.parseRequest(args);
            } catch (err) {
                emitCall();
                throw err;
            }

            if (!rich) {
                // Text-only channel: degrade to the kind's conversational directive.
                raisedSpecs.set(request.kind, request.spec);
                emitCall();
                return JSON.stringify({
                    mode: 'conversational',
                    kind: request.kind,
                    spec: request.spec,
                    reason: request.reason,
                    instructions: kind.fallbackDirective(request.spec, request.reason),
                });
            }

            // Rich channel: park the turn. Register the resolver, emit the prompt, then
            // await the client's `submit_interaction` (or a timeout / teardown).
            const interactionId = randomUUID();
            const outcome = park.park(sessionId, { interactionId, kind: request.kind, spec: request.spec });
            sink(protocol.interactionRequired(requestId, interactionId, request.kind, request.spec, request.reason));
            emitCall();

            const resolved = await withTimeout(outcome, timeoutMs, () => park.clear(sessionId));
            switch (resolved.status) {
                case 'submitted':
                    return JSON.stringify({ status: 'submitted', values: resolved.values });
                case 'declined':
                    return JSON.stringify({
                        status: 'declined',
                        message: 'The visitor declined. Continue helping them without this and do not ask again this conversation.',
                    });
                default:
                    return JSON.stringify({
                        status: 'no_response',
                        message: 'The visitor did not respond to the card. Continue without it; you may offer again later if it becomes relevant.',
                    });
            }
        },
    };
}

/**
 * Build the generic `submit_interaction` tool — the conversational fallback's submit
 * half, one instance per turn regardless of how many kinds are hosted. Routes to the
 * parked kind's server-side validator; invalid values throw a per-field tool error
 * the model relays and re-asks; valid values return the **identical** canonical
 * payload the rich path resumes with. Mirrors the Rust `SubmitInteractionTool`.
 */
export function submitInteractionTool(opts: { kinds: InteractionRegistry; raisedSpecs: RaisedSpecs; attach?: InteractionAttach }): Tool {
    const { kinds, raisedSpecs, attach } = opts;
    const kindIds = kinds.all().map((k) => k.kind);
    return {
        name: SUBMIT_INTERACTION_TOOL,
        description:
            'Submit the visitor\'s answers collected conversationally after a request_* interaction directive. Values '
            + 'are validated server-side; on a validation error, apologize, re-ask for the corrected field, and submit '
            + 'again. If the visitor declined, set declined=true.',
        parameters: {
            type: 'object',
            properties: {
                kind: { type: 'string', enum: kindIds, description: 'The interaction kind being submitted (from the directive).' },
                values: { type: 'object', description: 'The collected values, shaped per the interaction kind.' },
                declined: { type: 'boolean', description: 'True when the visitor declined the interaction.' },
            },
            required: ['kind'],
        },
        async execute(args: Record<string, unknown>): Promise<string> {
            const kindId = typeof args.kind === 'string' ? args.kind : '';
            if (!kindId) throw new Error("'kind' is required");
            const kind = kinds.get(kindId);
            if (!kind) throw new Error(`unknown interaction kind '${kindId}'`);

            if (args.declined === true) {
                return JSON.stringify({
                    status: 'declined',
                    message: 'Noted. Continue helping the visitor without this and do not ask again this conversation.',
                });
            }

            const values = args.values ?? null;
            // The spec raised earlier this turn (full required-ness) — or null (a
            // prior-turn raise): the kind then validates format-only.
            const spec = raisedSpecs.get(kindId) ?? null;
            const result = kind.validate(spec, values);
            if (result.ok) {
                // Kind-routed host effect on the conversational-fallback submit — the
                // rich path fires the same effect in the WS handler, which owns
                // validation there. Mirrors the Rust runner's `(cfg.attach)` call.
                await attach?.(kindId, result.values);
                return JSON.stringify({ status: 'submitted', values: result.values });
            }
            const detail = result.errors.map((e) => `${e.field}: ${e.message}`).join('; ');
            throw new Error(`validation failed — ${detail}. Re-ask the visitor for the corrected value(s) and submit again.`);
        },
    };
}

/**
 * Race a park promise against a timeout. On timeout, run `onTimeout` (clears the
 * registration) and resolve `no_response` — the visitor never answered the card.
 */
function withTimeout(promise: Promise<InteractionOutcome>, timeoutMs: number, onTimeout: () => void): Promise<InteractionOutcome> {
    return new Promise<InteractionOutcome>((resolve) => {
        const timer = setTimeout(() => {
            onTimeout();
            resolve({ status: 'no_response' });
        }, timeoutMs);
        // Don't keep the process alive on a parked card's long timer.
        if (typeof timer.unref === 'function') timer.unref();
        void promise.then((outcome) => {
            clearTimeout(timer);
            resolve(outcome);
        });
    });
}
