/**
 * Persistence for sessions + conversation message logs.
 *
 * The TypeScript port of the C# `SessionStore.cs` (and the Rust server's
 * `StorageAdapter` session/conversation/message surface). The bundled
 * {@link InMemorySessionStore} is the reference store; a durable adapter (Postgres,
 * DynamoDB, …) implements the same {@link SessionStore} interface.
 *
 * The interface is async so a network-backed store drops in without touching the
 * dispatcher/runner that consume it.
 */
import { randomUUID } from 'node:crypto';

/** A conversation session: the unit the protocol's create/get operate on. */
export interface StoredSession {
    sessionId: string;
    conversationId: string;
    agentId: string;
    agentName: string;
    userParticipantId: string;
    agentParticipantId: string;
}

/** Whether a stored message came from the user (`inbound`) or the agent (`outbound`). */
export type MessageDirection = 'inbound' | 'outbound';

export interface StoredMessage {
    id: string;
    conversationId: string;
    direction: MessageDirection;
    text: string;
}

export interface SessionStore {
    createSession(agentId: string, userName?: string, userEmail?: string): Promise<StoredSession>;
    getSession(sessionId: string): Promise<StoredSession | null>;
    appendMessage(conversationId: string, direction: MessageDirection, text: string): Promise<StoredMessage>;
    /** The most recent `limit` messages for a conversation, oldest first. */
    listMessages(conversationId: string, limit: number): Promise<StoredMessage[]>;
}

/** In-process {@link SessionStore}. The TS analog of the Rust in-memory adapter. */
export class InMemorySessionStore implements SessionStore {
    private readonly sessions = new Map<string, StoredSession>();
    private readonly messages = new Map<string, StoredMessage[]>();

    async createSession(agentId: string, _userName?: string, _userEmail?: string): Promise<StoredSession> {
        const session: StoredSession = {
            sessionId: randomUUID(),
            conversationId: randomUUID(),
            agentId: agentId && agentId.length > 0 ? agentId : randomUUID(),
            agentName: 'smooth-agent',
            userParticipantId: randomUUID(),
            agentParticipantId: randomUUID(),
        };
        this.sessions.set(session.sessionId, session);
        this.messages.set(session.conversationId, []);
        return session;
    }

    async getSession(sessionId: string): Promise<StoredSession | null> {
        return this.sessions.get(sessionId) ?? null;
    }

    async appendMessage(conversationId: string, direction: MessageDirection, text: string): Promise<StoredMessage> {
        const message: StoredMessage = { id: randomUUID(), conversationId, direction, text };
        let list = this.messages.get(conversationId);
        if (!list) {
            list = [];
            this.messages.set(conversationId, list);
        }
        list.push(message);
        return message;
    }

    async listMessages(conversationId: string, limit: number): Promise<StoredMessage[]> {
        const list = this.messages.get(conversationId);
        if (!list) return [];
        return limit >= list.length ? [...list] : list.slice(list.length - limit);
    }
}
