/**
 * Routes an inbound protocol frame (by its `action` discriminator) to the right
 * handler and emits the response event(s) to a sink.
 *
 * The TypeScript port of the C# `FrameDispatcher.cs` and the Rust server's
 * `handle_frame`. Transport-agnostic: the WebSocket host calls {@link dispatch}
 * per inbound text frame and writes the sink's events back over the socket.
 *
 * One dispatcher is bound to one connection's {@link AccessContext} (resolved from
 * the `?token=` slot), so retrieval for each turn is scoped to it — ACL is enforced
 * on the live chat path, not just at ingest.
 */
import type { ChatClientLike, Knowledge, Tool } from '@smooai/smooth-operator-core';
import { randomUUID } from 'node:crypto';

import { type AgentConfigResolver, assembleSystemPrompt } from './agentConfig.js';
import { gateTools, type SessionAuthenticator } from './toolGating.js';
import { ANONYMOUS_ACCESS, type AccessContext } from './auth.js';
import { ConfirmationRegistry } from './confirmation.js';
import { buildExtensionHost } from './extensions.js';
import { availableChannels, isContactEmpty, type OtpContact, type OtpRefusal, type OtpService } from './otp.js';
import type { ModelCeilingResolver } from './modelCeiling.js';
import * as protocol from './protocol.js';
import type { Frame } from './protocol.js';
import type { SessionStore } from './sessionStore.js';
import type { Sink } from './turnRunner.js';
import { DEFAULT_SYSTEM_PROMPT, TurnCancelledError, TurnRunner } from './turnRunner.js';

/**
 * A knowledge provider that yields a retriever SCOPED to a given access context —
 * the seam where ACL filtering plugs in. The MVP ships an unscoped provider
 * (returns the same store for everyone); a real one returns a view filtered to the
 * principal's groups, mirroring the Rust/C# `IAccessKnowledge.ForAccess`.
 */
export interface AccessKnowledge {
    forAccess(access: AccessContext): Knowledge | undefined;
}

export interface FrameDispatcherOptions {
    store: SessionStore;
    chatClient: ChatClientLike;
    knowledge?: AccessKnowledge;
    access?: AccessContext;
    systemPrompt?: string;
    /** Tools the agent may call during a turn (default none); forwarded to the {@link TurnRunner}. */
    tools?: Tool[];
    /**
     * SMOODEV-590 — resolves a session's `agentId` into its per-agent config
     * (instructions, conversationWorkflow, greeting, personality, tool allow-list).
     * Undefined → every turn uses the server/org default prompt + tools (behavior
     * unchanged).
     */
    agentConfig?: AgentConfigResolver;
    /** The cheap model id the workflow judge uses (forwarded to the {@link TurnRunner}). */
    judgeModel?: string;
    /**
     * SMOODEV-590 — resolves whether a conversation's session is identity-verified,
     * for `end_user` tool-auth gating on public agents. Absent → fail-closed (tools
     * requiring end_user auth on public agents are blocked).
     */
    sessionAuthenticator?: SessionAuthenticator;
    /**
     * End-user OTP identity-verification seam. When set, a turn whose auth gate
     * refuses an `end_user` tool on an unverified session (and the session has a
     * contact) triggers the OTP flow: emit `otp_verification_required`, call
     * {@link OtpService.sendOtp}, emit `otp_sent`; a later `verify_otp` action calls
     * {@link OtpService.verifyOtp} and, on success, marks the session authenticated.
     * Undefined → the fail-closed default (refuse, no OTP offered). The server never
     * holds a code; the host owns generation/expiry.
     */
    otpService?: OtpService;
    /**
     * Tool-name patterns gated behind write-confirmation HITL (default empty → no
     * gating, behavior unchanged). When a turn calls a tool whose name contains one of
     * these, the dispatcher parks the turn and emits `write_confirmation_required`
     * until the client replies with `confirm_tool_action`.
     */
    confirmTools?: string[];
    /**
     * The session-keyed pending-confirmation registry. One per connection (a
     * `confirm_tool_action` frame and the parked turn it resumes are always on the same
     * connection). Created on demand if not supplied.
     */
    confirmations?: ConfirmationRegistry;
    /** Model id for turns (default {@link DEFAULT_MODEL}); forwarded to the {@link TurnRunner}. */
    model?: string;
    /** Best-effort per-model output-ceiling resolver; forwarded to the {@link TurnRunner} (EPIC th-1cc9fa). */
    modelCeiling?: ModelCeilingResolver;
}

export class FrameDispatcher {
    private readonly store: SessionStore;
    private readonly chatClient: ChatClientLike;
    private readonly knowledge?: AccessKnowledge;
    private readonly access: AccessContext;
    private readonly systemPrompt?: string;
    private readonly tools: Tool[];
    private readonly confirmTools: string[];
    private readonly confirmations: ConfirmationRegistry;
    private readonly agentConfig?: AgentConfigResolver;
    private readonly judgeModel?: string;
    private readonly sessionAuthenticator?: SessionAuthenticator;
    private readonly otpService?: OtpService;
    private readonly model?: string;
    private readonly modelCeiling?: ModelCeilingResolver;
    /** In-flight spawned `send_message` turns, tracked so teardown can await them. */
    private readonly turns = new Set<Promise<void>>();
    /**
     * The connection's SINGLE active agent turn, if one is running. A connection runs at
     * most one turn at a time: a second `send_message` mid-turn is rejected with
     * `TURN_IN_PROGRESS` rather than run concurrently (interleaved streams + racing
     * storage writes). A `cancel` action — or a disconnect — aborts this turn.
     */
    private activeTurn: { requestId: string; controller: AbortController; promise: Promise<void> } | undefined;

    constructor(options: FrameDispatcherOptions) {
        this.store = options.store;
        this.chatClient = options.chatClient;
        this.knowledge = options.knowledge;
        this.access = options.access ?? ANONYMOUS_ACCESS;
        this.systemPrompt = options.systemPrompt;
        this.tools = options.tools ?? [];
        this.confirmTools = options.confirmTools ?? [];
        this.confirmations = options.confirmations ?? new ConfirmationRegistry();
        this.agentConfig = options.agentConfig;
        this.judgeModel = options.judgeModel;
        this.sessionAuthenticator = options.sessionAuthenticator;
        this.otpService = options.otpService;
        this.model = options.model;
        this.modelCeiling = options.modelCeiling;
    }

    /**
     * Await every in-flight spawned `send_message` turn to completion.
     *
     * `send_message` runs its turn as a background task (so the read loop stays free to
     * receive a `confirm_tool_action` while a turn is parked). The connection loop calls
     * this in its teardown so an in-flight turn finishes — and its `eventual_response`
     * is flushed — before the writer stops (preserves the graceful-drain contract).
     */
    async waitForTurns(): Promise<void> {
        if (this.turns.size > 0) await Promise.allSettled([...this.turns]);
    }

    /**
     * Reject every outstanding write-confirmation (fail closed — a write is never
     * auto-approved on disconnect), unparking any turn waiting on one so it can finish.
     * Called by the connection loop before {@link waitForTurns} on teardown.
     */
    rejectPendingConfirmations(): void {
        this.confirmations.rejectAll();
    }

    /**
     * Abort the connection's in-flight turn, if any, WITHOUT emitting anything. Returns
     * whether a turn was actually aborted.
     *
     * The turn's own {@link AbortController} is fired, so the runner throws
     * {@link TurnCancelledError} at its next stream event — no further protocol events,
     * and the partial assistant reply is never persisted. Any confirmation the turn is
     * parked on is rejected so it unparks and reaches that check. The turn is also
     * dropped from the drain set: {@link waitForTurns} must not block teardown on a turn
     * nobody is waiting for.
     *
     * Used by the `cancel` action (which then emits the terminal `cancelled` event) and
     * by the connection loop on a client DISCONNECT (no client remains to receive the
     * turn's output). Graceful drain deliberately does NOT call this — an in-flight turn
     * finishes on SIGTERM.
     */
    cancelActiveTurn(): boolean {
        const turn = this.activeTurn;
        if (!turn) return false;
        this.activeTurn = undefined;
        turn.controller.abort();
        this.confirmations.rejectAll();
        this.turns.delete(turn.promise);
        return true;
    }

    /**
     * Parse, validate, and route a single inbound frame, pushing response events to
     * `sink`. `signal`, when aborted, lets an in-flight `send_message` turn stop
     * streaming early (graceful drain).
     */
    async dispatch(rawFrame: string, sink: Sink, signal?: AbortSignal): Promise<void> {
        let frame: Record<string, unknown> | undefined;
        try {
            const parsed: unknown = JSON.parse(rawFrame);
            if (typeof parsed !== 'object' || parsed === null || Array.isArray(parsed)) {
                sink(protocol.error(undefined, 'VALIDATION_ERROR', 'Empty or non-object frame'));
                return;
            }
            frame = parsed as Record<string, unknown>;
        } catch {
            sink(protocol.error(undefined, 'VALIDATION_ERROR', 'Invalid JSON frame'));
            return;
        }

        const action = typeof frame.action === 'string' ? frame.action : undefined;
        const requestId = typeof frame.requestId === 'string' ? frame.requestId : undefined;

        try {
            switch (action) {
                case 'ping':
                    sink(protocol.pong(requestId));
                    break;
                case 'create_conversation_session':
                    await this.handleCreateSession(frame, requestId, sink);
                    break;
                case 'get_session':
                    await this.handleGetSession(frame, requestId, sink);
                    break;
                case 'list_conversations':
                    await this.handleListConversations(frame, requestId, sink);
                    break;
                case 'get_conversation_messages':
                    await this.handleGetConversationMessages(frame, requestId, sink);
                    break;
                case 'send_message':
                    // One active turn per connection: reject a second one rather than run
                    // two concurrently. (`confirm_tool_action` / `verify_otp` are turn
                    // RESUMES, not new turns, so they're unaffected.)
                    if (this.activeTurn) {
                        sink(
                            protocol.error(
                                requestId,
                                'TURN_IN_PROGRESS',
                                'a turn is already in progress on this connection; cancel it or wait for it to complete',
                            ),
                        );
                        break;
                    }
                    await this.handleSendMessage(frame, requestId, sink, signal);
                    break;
                case 'cancel':
                    this.handleCancel(requestId, sink);
                    break;
                case 'confirm_tool_action':
                    this.handleConfirmToolAction(frame, requestId, sink);
                    break;
                case 'verify_otp':
                    await this.handleVerifyOtp(frame, requestId, sink);
                    break;
                case undefined:
                    sink(protocol.error(requestId, 'VALIDATION_ERROR', "Missing 'action'"));
                    break;
                default:
                    sink(protocol.error(requestId, 'UNSUPPORTED_ACTION', `Unsupported action '${action}'`));
                    break;
            }
        } catch (err) {
            // A handler failed mid-turn (retrieval / model / store error, or a bug).
            // Emit a clean error and KEEP the connection alive — never drop the socket
            // with no signal to the client. (Detail stays server-side, not on the wire.)
            console.error(`[frameDispatcher] action '${action}' failed:`, err);
            sink(protocol.error(requestId, 'INTERNAL_ERROR', 'Internal error processing the request.'));
        }
    }

    /**
     * `cancel` — stop the connection's in-flight turn (the "Stop button").
     *
     * Per `spec/actions/cancel.schema.json` the client sends `{action, requestId}`,
     * reusing the `send_message`'s `requestId` as the correlation key. We abort the
     * tracked turn and emit the terminal `cancelled` event (status 499) echoing the
     * CANCELLED TURN's requestId — it replaces the `eventual_response` that turn would
     * otherwise have sent, so a turn emits exactly one terminal event. A cancel with no
     * active turn is a silent no-op: nothing is emitted and the connection stays live.
     */
    private handleCancel(requestId: string | undefined, sink: Sink): void {
        const turnRequestId = this.activeTurn?.requestId;
        // Echo the cancelled turn's requestId, falling back to the cancel frame's own.
        if (this.cancelActiveTurn()) sink(protocol.cancelled(turnRequestId ?? requestId));
    }

    private async handleCreateSession(frame: Record<string, unknown>, requestId: string | undefined, sink: Sink): Promise<void> {
        // Resume: a `conversationId` naming an existing conversation binds the new
        // session to it (reuses id + history); absent/unknown → a fresh conversation.
        // The response echoes `conversationId` either way, so a resuming client sees
        // the same id it passed. Mirrors the Rust reference's resume branch.
        const conversationId = typeof frame.conversationId === 'string' && frame.conversationId.length > 0 ? frame.conversationId : undefined;
        const session = await this.store.createSession(
            typeof frame.agentId === 'string' ? frame.agentId : '',
            typeof frame.userName === 'string' ? frame.userName : undefined,
            typeof frame.userEmail === 'string' ? frame.userEmail : undefined,
            conversationId,
        );
        sink(
            protocol.immediateResponse(requestId, 200, 'Session created', {
                sessionId: session.sessionId,
                conversationId: session.conversationId,
                agentId: session.agentId,
                agentName: session.agentName,
                userParticipantId: session.userParticipantId,
                agentParticipantId: session.agentParticipantId,
            }),
        );
    }

    private async handleGetSession(frame: Record<string, unknown>, requestId: string | undefined, sink: Sink): Promise<void> {
        const sessionId = typeof frame.sessionId === 'string' ? frame.sessionId : '';
        const session = await this.store.getSession(sessionId);
        if (!session) {
            sink(protocol.error(requestId, 'SESSION_NOT_FOUND', `session '${sessionId}' not found`));
            return;
        }
        sink(
            protocol.immediateResponse(requestId, 200, 'OK', {
                sessionId: session.sessionId,
                conversationId: session.conversationId,
                agentId: session.agentId,
                agentName: session.agentName,
            }),
        );
    }

    /**
     * `list_conversations` — return the resumable conversations, most-recent first.
     *
     * Mirrors the Rust reference: roll up every conversation, drop the empty ones
     * (`messageCount === 0`), derive a clean `title` from the FIRST inbound (user)
     * message, sort by `updatedAt` descending, and cap to `limit` (default 50). Each
     * entry is `{conversationId, title, updatedAt, messageCount}`. A client resumes one
     * by passing its `conversationId` to `create_conversation_session`.
     */
    private async handleListConversations(frame: Record<string, unknown>, requestId: string | undefined, sink: Sink): Promise<void> {
        const DEFAULT_LIMIT = 50;
        const rawLimit = typeof frame.limit === 'number' && Number.isFinite(frame.limit) ? Math.floor(frame.limit) : undefined;
        const limit = rawLimit !== undefined && rawLimit > 0 ? rawLimit : DEFAULT_LIMIT;

        const summaries = await this.store.listConversations();
        const conversations = summaries
            .filter((c) => c.messageCount > 0)
            .sort((a, b) => Date.parse(b.updatedAt) - Date.parse(a.updatedAt))
            .slice(0, limit)
            .map((c) => ({
                conversationId: c.conversationId,
                title: conversationTitle(c.firstInboundText, `Conversation ${c.conversationId}`),
                updatedAt: c.updatedAt,
                messageCount: c.messageCount,
            }));

        sink(protocol.immediateResponse(requestId, 200, 'Conversations', { conversations }));
    }

    /**
     * `get_conversation_messages` — the conversation-resume substrate. Returns the
     * session's conversation messages NEWEST-first plus a `hasMore` flag, per
     * `spec/actions/get-messages.schema.json`. Optional input: `limit` (1..100,
     * default 50) and `cursor` (an opaque prior-response `nextCursor`, a message id
     * — returns only messages older than the one it names). The response carries
     * `nextCursor` = the oldest message in the page, non-null iff `hasMore`.
     *
     * The cursor is deliberately NOT a timestamp: two messages can share a
     * `createdAt` at wire precision, so a `createdAt < cursor` filter drops or
     * repeats the collision. An id names exactly one message. th-75eda5, th-54d039.
     */
    private async handleGetConversationMessages(frame: Record<string, unknown>, requestId: string | undefined, sink: Sink): Promise<void> {
        const sessionId = typeof frame.sessionId === 'string' ? frame.sessionId : '';
        if (!sessionId) {
            sink(protocol.error(requestId, 'VALIDATION_ERROR', "Missing 'sessionId'"));
            return;
        }
        const session = await this.store.getSession(sessionId);
        if (!session) {
            sink(protocol.error(requestId, 'SESSION_NOT_FOUND', `session '${sessionId}' not found`));
            return;
        }

        const rawLimit = typeof frame.limit === 'number' && Number.isFinite(frame.limit) ? Math.floor(frame.limit) : undefined;
        const limit = Math.min(rawLimit !== undefined && rawLimit > 0 ? rawLimit : DEFAULT_MESSAGE_LIMIT, MAX_MESSAGE_LIMIT);

        const cursor = typeof frame.cursor === 'string' && frame.cursor.length > 0 ? frame.cursor : undefined;

        // No cursor: limit+1 rows is enough — the extra row is the `hasMore` probe.
        // With a cursor we must locate the named message in the log, so read it whole.
        // ponytail: whole-log read per cursored page; push the cursor into the store
        // when a conversation gets big enough to care (that breaks `SessionStore` for
        // downstream implementers, so not until it earns it).
        const stored = await this.store.listMessages(session.conversationId, cursor === undefined ? limit + 1 : Number.MAX_SAFE_INTEGER);

        // The store returns oldest-first; the contract is newest-first.
        const newestFirst = stored
            .map((m) => ({ id: m.id, direction: m.direction, content: { text: m.text }, createdAt: m.createdAt ?? EPOCH_ISO }))
            .reverse();

        let page = newestFirst;
        if (cursor !== undefined) {
            const at = newestFirst.findIndex((m) => m.id === cursor);
            if (at === -1) {
                sink(protocol.error(requestId, 'VALIDATION_ERROR', `Unknown 'cursor' '${cursor}'`));
                return;
            }
            page = newestFirst.slice(at + 1);
        }

        const hasMore = page.length > limit;
        const messages = hasMore ? page.slice(0, limit) : page;
        sink(
            protocol.immediateResponse(requestId, 200, 'ConversationMessages', {
                messages,
                nextCursor: hasMore ? (messages[messages.length - 1]?.id ?? null) : null,
                hasMore,
            }),
        );
    }

    private async handleSendMessage(frame: Record<string, unknown>, requestId: string | undefined, sink: Sink, signal?: AbortSignal): Promise<void> {
        const reqId = requestId ?? randomUUID();
        const sessionId = typeof frame.sessionId === 'string' ? frame.sessionId : '';
        const session = await this.store.getSession(sessionId);
        if (!session) {
            sink(protocol.error(reqId, 'SESSION_NOT_FOUND', `session '${sessionId}' not found`));
            return;
        }

        const message = typeof frame.message === 'string' ? frame.message : '';

        // 1. Immediate ack (202).
        sink(protocol.immediateResponse(reqId, 202, 'Processing your request...', {}));

        // SMOODEV-590 — resolve this agent's per-agent config (instructions,
        // conversationWorkflow, greeting, personality, tool allow-list) by the
        // session's agentId, and fold it into the effective system prompt + tools for
        // THIS turn. An un-configured agent (resolver undefined / returns undefined)
        // falls back to the server/org default prompt + full tool set — behavior
        // unchanged. Resolution never throws the turn: a resolver error degrades to
        // the default.
        let agentConfig;
        try {
            agentConfig = (await this.agentConfig?.resolve(session.agentId)) ?? undefined;
        } catch {
            agentConfig = undefined;
        }
        // First turn = no prior messages yet (the inbound is persisted by the runner
        // AFTER this). Gates the greeting to turn one only, matching the Python server.
        const isFirstTurn = (await this.store.listMessages(session.conversationId, 1)).length === 0;
        const baseSystemPrompt = this.systemPrompt ?? DEFAULT_SYSTEM_PROMPT;
        const effectiveSystemPrompt = assembleSystemPrompt(baseSystemPrompt, agentConfig, session.currentStepId, isFirstTurn);
        // SEP — build this turn's extension host (only when SMOOTH_EXTENSIONS_ALLOW is
        // set; undefined otherwise, zero overhead). The delegate is bound to THIS turn's
        // sink/request/session so a hosted extension's `ui/confirm` routes back over this
        // connection. Its eager tools join the base set BEFORE the enabled_tools filter,
        // so a per-agent allow-list drops them exactly like a built-in (SMOODEV-590 parity).
        const extHost = await buildExtensionHost({ confirmations: this.confirmations, sessionId, requestId: reqId, sink });
        const baseTools = extHost ? [...this.tools, ...extHost.tools()] : this.tools;
        const enabledTools = agentConfig?.enabledTools;
        const filteredTools = enabledTools?.length
            ? baseTools.filter((t) => enabledTools.some((e) => e.enabled && e.toolId === t.name))
            : baseTools;
        // Enforce authLevel + deliver per-tool config at execution (mirrors the
        // monorepo tool-execution gate). No-op for tools without a gated entry / config.
        // The session's own OTP-verified bit is threaded in so a verified caller's
        // `end_user` tools run; `otpRefusal` records an unverified `end_user` refusal so
        // we can offer OTP after the turn (the TS analog of the Rust auth-gate handle).
        const otpRefusal: OtpRefusal = {};
        const effectiveTools = gateTools(filteredTools, agentConfig, session.conversationId, this.sessionAuthenticator, session.otpVerified ?? false, otpRefusal);

        // 2. Stream the turn, retrieving through knowledge SCOPED to this connection's
        //    access — so a user only ever sees documents their groups grant.
        const scopedKnowledge = this.knowledge?.forAccess(this.access);
        const runner = new TurnRunner({
            chatClient: this.chatClient,
            store: this.store,
            knowledge: scopedKnowledge,
            systemPrompt: effectiveSystemPrompt,
            tools: effectiveTools,
            confirmTools: this.confirmTools,
            confirmations: this.confirmations,
            sessionId,
            workflow: agentConfig?.conversationWorkflow,
            currentStepId: session.currentStepId,
            judgeModel: this.judgeModel,
            model: this.model,
            modelCeiling: this.modelCeiling,
        });

        // Run the turn as a background task, NOT awaited inline. A turn that calls a
        // confirmation-gated tool **parks** awaiting a later `confirm_tool_action`
        // frame; the connection's read loop dispatches that frame, so awaiting the turn
        // here would block the reader and deadlock (the confirm could never be read).
        // Spawning frees the reader to receive the confirmation while the turn streams
        // its events through the sink. Mirrors the Python `asyncio.ensure_future` /
        // Rust `tokio::spawn`. The 202 ack above is already enqueued; the terminal
        // `eventual_response` is emitted from the task on completion. The connection
        // loop awaits all tracked turns on teardown ({@link waitForTurns}) so an
        // in-flight turn finishes before the writer stops (graceful drain).
        // This turn's own cancel handle — fired by a `cancel` action or a disconnect (NOT
        // by the server-wide drain `signal`, which lets an in-flight turn finish).
        const controller = new AbortController();
        let settled = false;

        const turn = (async (): Promise<void> => {
            try {
                const result = await runner.run(session.conversationId, reqId, message, sink, signal, controller.signal);
                // SMOODEV-590 — persist the workflow pointer the judge advanced to, so the
                // next turn on this conversation resumes on the right step. No-op for
                // freeform agents (nextStepId undefined) or a store without setCurrentStep.
                if (result.nextStepId !== undefined && result.nextStepId !== session.currentStepId) {
                    await this.store.setCurrentStep?.(sessionId, result.nextStepId);
                }
                // If the auth gate refused an `end_user` tool for lack of a verified
                // session this turn, and an OTP service is installed and the session has a
                // contact to reach, offer the OTP flow BEFORE the terminal event (mirrors
                // the Rust reference order: otp_verification_required → otp_sent →
                // eventual_response). The client verifies via `verify_otp` and re-sends its
                // message once authenticated — the server does not park/auto-resume.
                if (otpRefusal.refusedTool !== undefined && this.otpService) {
                    const contact: OtpContact = {};
                    if (session.contactEmail !== undefined) contact.email = session.contactEmail;
                    if (session.contactPhone !== undefined) contact.phone = session.contactPhone;
                    if (!isContactEmpty(contact)) {
                        await this.offerOtp(sessionId, otpRefusal.refusedTool, contact, reqId, sink);
                    }
                }
                sink(protocol.eventualResponse(reqId, 200, result.messageId, protocol.generalResponse(result.reply), false, result.citations));
            } catch (err) {
                // Cancelled: the terminal `cancelled` event was already emitted by the
                // cancel handler (or the client is gone) — emit nothing more.
                if (err instanceof TurnCancelledError) return;
                // Mirror the dispatcher's outer guard: a turn failure surfaces a clean
                // error and keeps the connection alive (detail stays server-side).
                console.error('[frameDispatcher] turn failed:', err);
                sink(protocol.error(reqId, 'INTERNAL_ERROR', 'Internal error processing the request.'));
            } finally {
                settled = true;
                // Only clear the slot if it's still OURS: a cancel already took it (and a
                // later `send_message` may have installed its own).
                if (this.activeTurn?.controller === controller) this.activeTurn = undefined;
                // SEP — kill this turn's extension subprocesses and drop any `ui/confirm`
                // responder it left parked (mirrors the Rust `(ext.clear)` + host drop).
                if (extHost) {
                    this.confirmations?.clear(sessionId);
                    await extHost.shutdownAll();
                }
            }
        })();
        // Track it as the connection's single active turn — unless it already finished
        // synchronously, in which case there is nothing to cancel.
        if (!settled) this.activeTurn = { requestId: reqId, controller, promise: turn };
        this.turns.add(turn);
        void turn.finally(() => this.turns.delete(turn));
    }

    /**
     * `confirm_tool_action` — resume a turn parked on a write-tool confirmation.
     *
     * Per `spec/actions/confirm-tool-action.schema.json` the client replies with
     * `{action, sessionId, requestId, approved}` to a `write_confirmation_required`
     * event. We resolve the session's pending confirmation with the verdict: the parked
     * `HumanGate` returns and the turn resumes (runs the tool on approve, skips it with
     * a rejection result on deny). There is no dedicated response event — continuation
     * is signalled by the resumed streaming sequence; we ack with an
     * `immediate_response`. Resolving takes the deferred out, so a duplicate confirm is
     * a clean `NO_PENDING_CONFIRMATION` no-op. Fails closed: a missing `sessionId` or
     * non-bool `approved` is rejected (never silently approve).
     */
    private handleConfirmToolAction(frame: Record<string, unknown>, requestId: string | undefined, sink: Sink): void {
        const sessionId = typeof frame.sessionId === 'string' ? frame.sessionId : '';
        if (!sessionId) {
            sink(protocol.error(requestId, 'VALIDATION_ERROR', "confirm_tool_action requires a 'sessionId'"));
            return;
        }

        if (typeof frame.approved !== 'boolean') {
            sink(protocol.error(requestId, 'VALIDATION_ERROR', "confirm_tool_action requires a boolean 'approved'"));
            return;
        }
        const approved = frame.approved;

        if (!this.confirmations.resolve(sessionId, approved)) {
            sink(protocol.error(requestId, 'NO_PENDING_CONFIRMATION', `no tool action is awaiting confirmation for session '${sessionId}'`));
            return;
        }

        sink(
            protocol.immediateResponse(requestId, 200, approved ? 'Tool action approved' : 'Tool action rejected', {
                sessionId,
                approved,
            }),
        );
    }

    /**
     * Emit the OTP-offer sequence for a turn whose `end_user` tool was refused for
     * lack of a verified session: `otp_verification_required` (prompt the client),
     * then `sendOtp` on the host service, then `otp_sent` (ack delivery) — or an
     * `error` event if delivery throws. The masked destination + channel come from
     * the host; the server never sees the code. `authLevel` is fixed `end_user` (the
     * only level this flow remedies).
     */
    private async offerOtp(sessionId: string, tool: string, contact: OtpContact, requestId: string, sink: Sink): Promise<void> {
        const otp = this.otpService;
        if (!otp) return;
        sink(protocol.otpVerificationRequired(requestId, tool, `Verify your identity to continue using '${tool}'.`, availableChannels(contact), 'end_user'));
        try {
            const delivery = await otp.sendOtp(sessionId, contact);
            sink(protocol.otpSent(requestId, delivery.channel, delivery.maskedDestination));
        } catch (err) {
            console.error('[frameDispatcher] otp send failed:', err);
            sink(protocol.error(requestId, 'OTP_SEND_FAILED', 'failed to send verification code'));
        }
    }

    /**
     * `verify_otp` — validate a submitted OTP code and, on success, mark the session
     * identity-verified. Per `spec/actions/verify-otp.schema.json` the client sends
     * `{action, sessionId, requestId, code}` in reply to an `otp_verification_required`
     * event. There is no dedicated response event: a correct code emits `otp_verified`
     * (the client re-sends its message to run the gated tool — the server does not
     * park/auto-resume the original turn), a rejected code emits `otp_invalid` carrying
     * the host's remaining-attempt count. With no {@link OtpService} installed,
     * verification is impossible, so we fail closed with `otp_invalid` (`NOT_FOUND`, 0
     * attempts). Validation order mirrors the Rust reference: requestId, sessionId,
     * code, session-exists, no-service.
     */
    private async handleVerifyOtp(frame: Record<string, unknown>, requestId: string | undefined, sink: Sink): Promise<void> {
        // requestId is load-bearing (it echoes the originating otp_verification_required); require it.
        if (requestId === undefined) {
            sink(protocol.error(undefined, 'VALIDATION_ERROR', "verify_otp requires a 'requestId'"));
            return;
        }
        const sessionId = typeof frame.sessionId === 'string' ? frame.sessionId : undefined;
        if (sessionId === undefined) {
            sink(protocol.error(requestId, 'VALIDATION_ERROR', "verify_otp requires a 'sessionId'"));
            return;
        }
        const code = typeof frame.code === 'string' ? frame.code : undefined;
        if (code === undefined) {
            sink(protocol.error(requestId, 'VALIDATION_ERROR', "verify_otp requires a 'code'"));
            return;
        }

        // The session must exist (a code can't verify a session we don't track).
        const session = await this.store.getSession(sessionId);
        if (!session) {
            sink(protocol.error(requestId, 'SESSION_NOT_FOUND', `session '${sessionId}' not found`));
            return;
        }

        // No host OTP service → verification is impossible. Fail closed on the documented
        // otp_invalid path (a client shouldn't reach here without first receiving
        // otp_verification_required, which only an installed service emits).
        if (!this.otpService) {
            sink(protocol.otpInvalid(requestId, 'NOT_FOUND', 0, 'No verification is in progress for this session.'));
            return;
        }

        const outcome = await this.otpService.verifyOtp(sessionId, code);
        if (outcome.verified) {
            await this.store.setAuthenticated?.(sessionId, true);
            sink(protocol.otpVerified(requestId, 'Identity verified successfully.'));
        } else {
            sink(protocol.otpInvalid(requestId, outcome.error, outcome.attemptsRemaining, outcome.message));
        }
    }
}

const TITLE_MAX = 60;

/** `get_conversation_messages` page size when the client sends no `limit`. */
const DEFAULT_MESSAGE_LIMIT = 50;
/** Contract ceiling on `get_conversation_messages`' `limit` (1..100). */
const MAX_MESSAGE_LIMIT = 100;
/** Reported `createdAt` for a message a pre-th-75eda5 store left untimestamped. */
const EPOCH_ISO = new Date(0).toISOString();

/**
 * Derive a `list_conversations` entry title from the first inbound message text,
 * falling back to `fallback` (the conversation name) when there is none. The
 * preview is cleaned (leading markdown/control chars stripped) and clipped to
 * {@link TITLE_MAX} characters with an ellipsis. Mirrors the Rust reference's
 * `conversation_title` + `truncate_preview`, with the contract's leading-markdown
 * strip so the client renders clean text.
 */
export function conversationTitle(firstInboundText: string | undefined, fallback: string): string {
    const cleaned = firstInboundText !== undefined ? cleanPreview(firstInboundText) : '';
    if (cleaned.length === 0) return fallback;
    return truncatePreview(cleaned, TITLE_MAX);
}

/**
 * Strip leading markdown/control noise (blockquote `>`, cursor `▎`, heading `#`,
 * emphasis `*`, list/rule dashes, backticks) and control chars so a raw message
 * body renders as plain text, then trim. Only leading chars are touched — inline
 * markdown mid-text is left alone.
 */
function cleanPreview(s: string): string {
    // eslint-disable-next-line no-control-regex
    return s.replace(/^[\s>#*`~_\-▎ -]+/u, '').trim();
}

/** Clip to `max` chars, appending an ellipsis when truncated (matches the Rust `truncate_preview`). */
function truncatePreview(s: string, max: number): string {
    const trimmed = s.trim();
    const chars = [...trimmed];
    if (chars.length <= max) return trimmed;
    return `${chars.slice(0, max).join('').trimEnd()}…`;
}
