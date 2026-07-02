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
        const enabledTools = agentConfig?.enabledTools;
        const filteredTools = enabledTools?.length
            ? this.tools.filter((t) => enabledTools.some((e) => e.enabled && e.toolId === t.name))
            : this.tools;
        // Enforce authLevel + deliver per-tool config at execution (mirrors the
        // monorepo tool-execution gate). No-op for tools without a gated entry / config.
        const effectiveTools = gateTools(filteredTools, agentConfig, session.conversationId, this.sessionAuthenticator);

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
                sink(protocol.eventualResponse(reqId, 200, result.messageId, protocol.generalResponse(result.reply), false, result.citations));
            } catch (err) {
                // Mirror the dispatcher's outer guard: a turn failure surfaces a clean
                // error and keeps the connection alive (detail stays server-side).
                console.error('[frameDispatcher] turn failed:', err);
                sink(protocol.error(reqId, 'INTERNAL_ERROR', 'Internal error processing the request.'));
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
}
