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
import * as protocol from './protocol.js';
import type { Frame } from './protocol.js';
import type { SessionStore } from './sessionStore.js';
import type { Sink } from './turnRunner.js';
import { DEFAULT_SYSTEM_PROMPT, TurnRunner } from './turnRunner.js';

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
    /** In-flight spawned `send_message` turns, tracked so teardown can await them. */
    private readonly turns = new Set<Promise<void>>();

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
                case 'send_message':
                    await this.handleSendMessage(frame, requestId, sink, signal);
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

    private async handleCreateSession(frame: Record<string, unknown>, requestId: string | undefined, sink: Sink): Promise<void> {
        const session = await this.store.createSession(
            typeof frame.agentId === 'string' ? frame.agentId : '',
            typeof frame.userName === 'string' ? frame.userName : undefined,
            typeof frame.userEmail === 'string' ? frame.userEmail : undefined,
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
        const turn = (async (): Promise<void> => {
            try {
                const result = await runner.run(session.conversationId, reqId, message, sink, signal);
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
                // Mirror the dispatcher's outer guard: a turn failure surfaces a clean
                // error and keeps the connection alive (detail stays server-side).
                console.error('[frameDispatcher] turn failed:', err);
                sink(protocol.error(reqId, 'INTERNAL_ERROR', 'Internal error processing the request.'));
            } finally {
                // SEP — kill this turn's extension subprocesses and drop any `ui/confirm`
                // responder it left parked (mirrors the Rust `(ext.clear)` + host drop).
                if (extHost) {
                    this.confirmations?.clear(sessionId);
                    await extHost.shutdownAll();
                }
            }
        })();
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
