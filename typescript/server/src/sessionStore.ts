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
    /**
     * SMOODEV-590 — the conversation's current step id within the agent's
     * `conversationWorkflow`. `undefined` means "not started" → the first step is
     * rendered. Advanced by the post-turn workflow judge and persisted here so the
     * next turn resumes on the right step.
     */
    currentStepId?: string;
    /**
     * The OWNER of this session's conversation — the authenticated principal's email,
     * stamped at create time. Every conversation read (`get_session`, resume,
     * `get_conversation_messages`, `send_message`, `verify_otp`) is checked against it.
     * `undefined` means the conversation has no owner: created either on an auth-disabled
     * server (single-tenant, nothing to scope) or by a caller with no email claim — in
     * which case no authenticated principal can ever match it. Fail closed, by design.
     *
     * NOT the client-supplied `userEmail` frame field. That value is attacker-controlled,
     * and trusting it was the original cross-user leak: anyone could claim anyone's scope.
     */
    userEmail?: string;
    /**
     * The caller's email captured at create-session time, used as an OTP delivery
     * contact for the `end_user` auth-gate flow. The reference create path captures
     * only an email; a host store may also carry a phone. `undefined` → no channel to
     * offer OTP on.
     */
    contactEmail?: string;
    /** The caller's phone, if the store captured one — the SMS OTP delivery contact. */
    contactPhone?: string;
    /**
     * Whether the caller has completed OTP identity verification (set by a successful
     * `verify_otp`). Threaded into the `end_user` auth gate so a verified caller's
     * gated tools run. `undefined`/`false` → unverified. The TS analog of the Rust
     * reference server's `session.metadata.otpVerified`.
     */
    otpVerified?: boolean;
}

/** Whether a stored message came from the user (`inbound`) or the agent (`outbound`). */
export type MessageDirection = 'inbound' | 'outbound';

export interface StoredMessage {
    id: string;
    conversationId: string;
    direction: MessageDirection;
    text: string;
    /**
     * ISO-8601 timestamp of when the message was appended — the `createdAt` field of the
     * `get_conversation_messages` contract and its `before` paging key. Optional so
     * stores that predate the action still satisfy the interface; absent → the
     * dispatcher reports the epoch (the message sorts as oldest). th-75eda5.
     */
    createdAt?: string;
}

/**
 * A conversation's roll-up for the `list_conversations` action: enough for a
 * client to render a resumable-thread list without pulling every message. The
 * dispatcher turns `firstInboundText`/`updatedAt`/`messageCount` into the wire
 * `{conversationId, title, updatedAt, messageCount}`.
 */
export interface ConversationSummary {
    conversationId: string;
    /** ISO-8601 timestamp of the conversation's last activity (create or last appended message). */
    updatedAt: string;
    /** Total messages in the conversation. The dispatcher drops empties (`0`). */
    messageCount: number;
    /** Text of the FIRST inbound (user) message — the dispatcher's title source. Undefined when none. */
    firstInboundText?: string;
}

export interface SessionStore {
    /**
     * Create a session. When `conversationId` names an EXISTING conversation, the
     * new session binds to it (resume: reuses the id + its persisted message log,
     * so subsequent turns append and history replays). An absent or unknown id mints
     * a fresh conversation (unchanged behavior).
     */
    createSession(agentId: string, userName?: string, userEmail?: string, conversationId?: string): Promise<StoredSession>;
    getSession(sessionId: string): Promise<StoredSession | null>;
    /**
     * A conversation by id, or null if unknown — the resume-binding existence check.
     *
     * `userEmail` is the conversation's OWNER (see {@link StoredSession.userEmail}), and
     * the caller compares it against the authenticated principal before binding. It is
     * REQUIRED rather than optional so an implementation that doesn't track ownership
     * fails to compile instead of silently reporting every conversation as ownerless.
     */
    getConversation(conversationId: string): Promise<{ conversationId: string; userEmail: string | undefined } | null>;
    appendMessage(conversationId: string, direction: MessageDirection, text: string): Promise<StoredMessage>;
    /** The most recent `limit` messages for a conversation, oldest first. */
    listMessages(conversationId: string, limit: number): Promise<StoredMessage[]>;
    /**
     * Roll-up of the conversations OWNED BY `userEmail` (most-recent ordering +
     * empty-filtering is the dispatcher's job).
     *
     * `undefined` means unscoped — every conversation — and is reserved for
     * auth-disabled (single-tenant local/dev) servers. On a server with auth
     * configured the dispatcher never passes `undefined`; a principal with no email
     * gets an empty list instead.
     *
     * The parameter is REQUIRED, not optional-defaulting-to-unscoped. An optional
     * parameter is fail-OPEN: existing implementations keep compiling and keep leaking
     * every user's conversations. The compile break is deliberate — it forces each
     * implementation to make an explicit scoping decision.
     *
     * Implementations MUST apply the filter in the selection itself, not after
     * applying a limit. Limiting first and filtering after silently returns short or
     * empty pages, which reads as "no conversations" rather than as a bug.
     */
    listConversations(userEmail: string | undefined): Promise<ConversationSummary[]>;
    /**
     * SMOODEV-590 — persist the conversation's current workflow step id (set by the
     * post-turn judge). A no-op for an unknown session. Optional so existing stores
     * that predate workflows still satisfy the interface.
     */
    setCurrentStep?(sessionId: string, currentStepId: string): Promise<void>;
    /**
     * Mark a session identity-verified (or clear it) — called after a successful
     * `verify_otp`. A no-op for an unknown session. Optional so stores that predate
     * the OTP seam still satisfy the interface; absent → verification can't persist
     * (a verified caller's gated tools won't run, fail-closed).
     */
    setAuthenticated?(sessionId: string, verified: boolean): Promise<void>;
}

/** In-process {@link SessionStore}. The TS analog of the Rust in-memory adapter. */
export class InMemorySessionStore implements SessionStore {
    private readonly sessions = new Map<string, StoredSession>();
    private readonly messages = new Map<string, StoredMessage[]>();
    /** Per-conversation last-activity epoch ms — set on create + every append; the `updatedAt` source. */
    private readonly convUpdatedAt = new Map<string, number>();
    /**
     * Per-conversation owner email — the scoping key for `listConversations` and the
     * resume/read owner checks. Written once, when the conversation is minted; a resume
     * never rewrites it, or a second caller could take ownership of a conversation by
     * resuming it.
     */
    private readonly convOwner = new Map<string, string | undefined>();

    async createSession(agentId: string, _userName?: string, userEmail?: string, conversationId?: string): Promise<StoredSession> {
        // Resume: bind to an existing conversation (reuse its id + persisted log) when
        // the caller passes a known conversationId. Unknown/absent → mint a fresh one.
        const resume = conversationId && this.messages.has(conversationId);
        const convId = resume ? conversationId : randomUUID();
        const session: StoredSession = {
            sessionId: randomUUID(),
            conversationId: convId,
            agentId: agentId && agentId.length > 0 ? agentId : randomUUID(),
            agentName: 'smooth-agent',
            userParticipantId: randomUUID(),
            agentParticipantId: randomUUID(),
            // On a resume the owner is the conversation's ORIGINAL owner, not this
            // caller's email — the dispatcher has already verified they match, and
            // re-deriving it from the request would let a resume rewrite ownership.
            ...(resume ? this.ownerField(convId) : userEmail ? { userEmail } : {}),
            // Stash the caller's email as an OTP delivery contact for the end_user
            // auth-gate flow (mirrors the Rust reference capturing contactEmail).
            ...(userEmail ? { contactEmail: userEmail } : {}),
        };
        this.sessions.set(session.sessionId, session);
        // Only initialize the message log on a fresh conversation — a resume keeps its history.
        if (!resume) {
            this.messages.set(convId, []);
            this.convUpdatedAt.set(convId, Date.now());
            this.convOwner.set(convId, userEmail);
        }
        return session;
    }

    /** The stored owner of `convId` as a spreadable field (absent when ownerless). */
    private ownerField(convId: string): { userEmail?: string } {
        const owner = this.convOwner.get(convId);
        return owner ? { userEmail: owner } : {};
    }

    async getSession(sessionId: string): Promise<StoredSession | null> {
        return this.sessions.get(sessionId) ?? null;
    }

    async getConversation(conversationId: string): Promise<{ conversationId: string; userEmail: string | undefined } | null> {
        return this.messages.has(conversationId) ? { conversationId, userEmail: this.convOwner.get(conversationId) } : null;
    }

    async appendMessage(conversationId: string, direction: MessageDirection, text: string): Promise<StoredMessage> {
        const message: StoredMessage = { id: randomUUID(), conversationId, direction, text, createdAt: new Date().toISOString() };
        let list = this.messages.get(conversationId);
        if (!list) {
            list = [];
            this.messages.set(conversationId, list);
        }
        list.push(message);
        this.convUpdatedAt.set(conversationId, Date.now());
        return message;
    }

    async listMessages(conversationId: string, limit: number): Promise<StoredMessage[]> {
        const list = this.messages.get(conversationId);
        if (!list) return [];
        return limit >= list.length ? [...list] : list.slice(list.length - limit);
    }

    async listConversations(userEmail: string | undefined): Promise<ConversationSummary[]> {
        const out: ConversationSummary[] = [];
        for (const [conversationId, list] of this.messages) {
            // Scoped: skip conversations this user doesn't own. Filtering here — inside the
            // selection, before any caller-side limit — is what keeps a scoped page full.
            // Case-insensitive: OIDC providers vary on the casing they emit for one identity.
            if (userEmail !== undefined && this.convOwner.get(conversationId)?.toLowerCase() !== userEmail.toLowerCase()) continue;
            const firstInbound = list.find((m) => m.direction === 'inbound');
            out.push({
                conversationId,
                updatedAt: new Date(this.convUpdatedAt.get(conversationId) ?? 0).toISOString(),
                messageCount: list.length,
                ...(firstInbound ? { firstInboundText: firstInbound.text } : {}),
            });
        }
        return out;
    }

    async setCurrentStep(sessionId: string, currentStepId: string): Promise<void> {
        const session = this.sessions.get(sessionId);
        if (session) session.currentStepId = currentStepId;
    }

    async setAuthenticated(sessionId: string, verified: boolean): Promise<void> {
        const session = this.sessions.get(sessionId);
        if (session) session.otpVerified = verified;
    }
}
