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

import { ANONYMOUS_ACCESS, type AccessContext } from './auth.js';
import * as protocol from './protocol.js';
import type { Frame } from './protocol.js';
import type { SessionStore } from './sessionStore.js';
import type { Sink } from './turnRunner.js';
import { TurnRunner } from './turnRunner.js';

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
}

export class FrameDispatcher {
    private readonly store: SessionStore;
    private readonly chatClient: ChatClientLike;
    private readonly knowledge?: AccessKnowledge;
    private readonly access: AccessContext;
    private readonly systemPrompt?: string;
    private readonly tools: Tool[];

    constructor(options: FrameDispatcherOptions) {
        this.store = options.store;
        this.chatClient = options.chatClient;
        this.knowledge = options.knowledge;
        this.access = options.access ?? ANONYMOUS_ACCESS;
        this.systemPrompt = options.systemPrompt;
        this.tools = options.tools ?? [];
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
                case undefined:
                    sink(protocol.error(requestId, 'VALIDATION_ERROR', "Missing 'action'"));
                    break;
                default:
                    sink(protocol.error(requestId, 'UNSUPPORTED_ACTION', `Unsupported action '${action}'`));
                    break;
            }
        } catch {
            // A handler failed mid-turn (retrieval / model / store error, or a bug).
            // Emit a clean error and KEEP the connection alive — never drop the socket
            // with no signal to the client. (Detail stays server-side, not on the wire.)
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

        // 2. Stream the turn, retrieving through knowledge SCOPED to this connection's
        //    access — so a user only ever sees documents their groups grant.
        const scopedKnowledge = this.knowledge?.forAccess(this.access);
        const runner = new TurnRunner({
            chatClient: this.chatClient,
            store: this.store,
            knowledge: scopedKnowledge,
            systemPrompt: this.systemPrompt,
            tools: this.tools,
        });
        const result = await runner.run(session.conversationId, reqId, message, sink, signal);

        // 3. Terminal eventual_response.
        sink(
            protocol.eventualResponse(reqId, 200, result.messageId, protocol.generalResponse(result.reply), false, result.citations),
        );
    }
}
