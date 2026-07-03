/**
 * AUTO-GENERATED — do not edit by hand.
 *
 * Generated from the JSON Schemas in ../spec by scripts/generate.ts
 * Run `pnpm generate` to regenerate after a schema change.
 */
/* eslint-disable */

// ── from actions/confirm-tool-action.schema.json ──
/**
 * Fields sent by the client to approve or reject a pending tool write.
 */
export interface ConfirmToolActionRequest {
    /**
     * Action discriminator.
     */
    action: 'confirm_tool_action';
    /**
     * Must match the `requestId` from the `write_confirmation_required` event being responded to. This is how the server correlates the confirmation back to the paused workflow.
     */
    requestId: string;
    /**
     * Session ID of the paused session.
     */
    sessionId: string;
    /**
     * True to allow the tool call to proceed; false to reject it. On rejection the agent workflow resumes with an informational context message.
     */
    approved: boolean;
}

// ── from actions/confirm-tool-action.schema.json ──
/**
 * No dedicated response — the workflow continuation is signalled by resumed streaming events.
 */
export interface ConfirmToolActionResponse {
    [k: string]: unknown;
}

// ── from actions/create-conversation-session.schema.json ──
/**
 * Fields sent by the client to open a new session.
 */
export interface CreateConversationSessionRequest {
    /**
     * Action discriminator.
     */
    action: 'create_conversation_session';
    /**
     * Client-generated correlation ID echoed back on all related events.
     */
    requestId?: string;
    /**
     * UUID of the agent to start a session with.
     */
    agentId: string;
    /**
     * Optional display name for the user participant.
     */
    userName?: string;
    /**
     * Optional email address for the user participant.
     */
    userEmail?: string;
    /**
     * Browser fingerprint string (e.g. from ThumbmarkJS) used for anonymous user correlation across sessions.
     */
    browserFingerprint?: string;
    /**
     * Client render capabilities for this session. Known value: `identity_form` — the client can render a structured identity-intake form, so the server may emit `identity_intake_required` mid-turn. Text-only channels (SMS, voice) omit this and the server degrades intake to conversational turn-by-turn collection. Unknown values are ignored (forward-compatible).
     */
    supports?: string[];
    /**
     * Arbitrary key/value metadata to attach to the session.
     */
    metadata?: {
        [k: string]: unknown;
    };
    /**
     * Pre-auth context for HMAC-based identity verification. When provided, the server can skip OTP flows by verifying the HMAC signature instead.
     */
    authContext?: {
        /**
         * The user's identity claim.
         */
        userId: string;
        /**
         * HMAC-SHA256 signature over `userId + timestamp` using a shared secret.
         */
        signature: string;
        /**
         * Unix epoch seconds when the HMAC was signed. Used for replay protection (typical max age: 60s).
         */
        timestamp: number;
    };
}

// ── from actions/create-conversation-session.schema.json ──
/**
 * Data payload carried in the `immediate_response` event that acknowledges session creation.
 */
export interface CreateConversationSessionResponse {
    /**
     * Newly created session ID. Use this in all subsequent actions.
     */
    sessionId: string;
    /**
     * ID of the conversation created for this session.
     */
    conversationId: string;
    /**
     * ID of the agent handling this session.
     */
    agentId: string;
    /**
     * Display name of the agent.
     */
    agentName: string;
    /**
     * Participant ID representing the user in this conversation.
     */
    userParticipantId: string;
    /**
     * Participant ID representing the AI agent in this conversation.
     */
    agentParticipantId: string;
}

// ── from actions/get-messages.schema.json ──
/**
 * Fields sent by the client to page through conversation history.
 */
export interface GetMessagesRequest {
    /**
     * Action discriminator.
     */
    action: 'get_conversation_messages';
    /**
     * Client-generated correlation ID echoed back on all related events.
     */
    requestId?: string;
    /**
     * Session ID whose conversation messages to fetch.
     */
    sessionId: string;
    /**
     * Maximum number of messages to return per page. Must be 1–100; defaults to 50.
     */
    limit?: number;
    /**
     * ISO 8601 cursor: return only messages created strictly before this timestamp. Omit to start from the most recent message.
     */
    before?: string;
}

// ── from actions/get-messages.schema.json ──
/**
 * Data payload carried in the `immediate_response` event.
 */
export interface GetMessagesResponse {
    /**
     * Ordered list of messages (newest-first up to `limit`).
     */
    messages: ConversationMessage[];
    /**
     * True if more messages exist before the oldest message in this page. Use the oldest `createdAt` as the next `before` cursor.
     */
    hasMore: boolean;
}
/**
 * A single message as returned on the wire. This is a subset of the full `Message` domain object — it omits server-only fields (metadataJson, analyticsJson) and includes abbreviated participant descriptors.
 */
export interface ConversationMessage {
    /**
     * Unique message ID.
     */
    id: string;
    /**
     * Message direction: `inbound` = from user, `outbound` = from agent.
     */
    direction: 'inbound' | 'outbound';
    /**
     * Message payload. `text` contains the plain-text form; `structuredResponse` contains parsed agent output when present.
     */
    content: {
        /**
         * Plain-text content.
         */
        text?: string;
        /**
         * Structured agent response (shape depends on agent template).
         */
        structuredResponse?: {
            [k: string]: unknown;
        } | null;
    };
    /**
     * Abbreviated sender descriptor.
     */
    from?: {
        id: string;
        type: string;
        name?: string;
    };
    /**
     * Abbreviated recipient descriptor.
     */
    to?: {
        id: string;
        type: string;
        name?: string;
    };
    /**
     * ISO 8601 timestamp when the message was created.
     */
    createdAt: string;
}

// ── from actions/get-session.schema.json ──
/**
 * Fields sent by the client to retrieve a session.
 */
export interface GetSessionRequest {
    /**
     * Action discriminator.
     */
    action: 'get_session';
    /**
     * Client-generated correlation ID echoed back on all related events.
     */
    requestId?: string;
    /**
     * ID of the session to retrieve.
     */
    sessionId: string;
}

// ── from actions/get-session.schema.json ──
/**
 * Data payload carried in the `immediate_response` event. Field set matches `Session` in `domain/session.schema.json` (wire-subset; full domain object may include additional server-only fields).
 */
export interface GetSessionResponse {
    /**
     * Session ID.
     */
    sessionId: string;
    /**
     * Conversation ID.
     */
    conversationId: string;
    /**
     * Agent ID.
     */
    agentId: string;
    /**
     * Display name of the agent.
     */
    agentName: string;
    /**
     * Participant ID for the user in this session.
     */
    userParticipantId: string;
    /**
     * Participant ID for the agent in this session.
     */
    agentParticipantId: string;
    /**
     * smooth-operator thread identifier. Present when the session has an associated workflow thread.
     */
    threadId?: string;
    /**
     * Current lifecycle status of the session.
     */
    status?: 'active' | 'idle' | 'ended';
}

// ── from actions/ping.schema.json ──
/**
 * A ping frame. Only `action` and the optional `requestId` are required.
 */
export interface PingRequest {
    /**
     * Action discriminator.
     */
    action: 'ping';
    /**
     * Client-generated correlation ID echoed back in the `pong` event.
     */
    requestId?: string;
}

// ── from actions/ping.schema.json ──
/**
 * Data payload carried in the `pong` event.
 */
export interface PongResponse {
    /**
     * Server-side Unix epoch milliseconds when the pong was sent.
     */
    timestamp: number;
}

// ── from actions/send-message.schema.json ──
/**
 * Fields sent by the client to submit a message.
 */
export interface SendMessageRequest {
    /**
     * Action discriminator.
     */
    action: 'send_message';
    /**
     * Client-generated correlation ID echoed back on all related events.
     */
    requestId?: string;
    /**
     * Session ID returned by `create_conversation_session`.
     */
    sessionId: string;
    /**
     * The user's message text. Between 1 and 10 000 characters.
     */
    message: string;
    /**
     * Whether to receive incremental `stream_chunk` and `stream_token` events. Defaults to `true`. Set to `false` to receive only the final `eventual_response`.
     */
    stream?: boolean;
    /**
     * Optional gateway model id to run THIS turn on (e.g. a /smooth-mode preset). Absent → the server's configured default model.
     */
    model?: string;
}

// ── from actions/send-message.schema.json ──
/**
 * Data payload carried in the terminal `eventual_response` event for this action. Also see `GeneralAgentResponse` for the structured response shape.
 */
export interface SendMessageResponse {
    /**
     * ID of the agent's response message as persisted to the database.
     */
    messageId: string;
    response: GeneralAgentResponse;
    /**
     * True if the agent flagged this turn for human escalation.
     */
    needsEscalation: boolean;
    /**
     * Human-readable reason when `needsEscalation` is true.
     */
    escalationReason?: string;
}
/**
 * Structured agent response.
 */
export interface GeneralAgentResponse {
    /**
     * Ordered text segments that together form the agent's reply. Clients may join them or render each separately.
     */
    responseParts: string[];
    /**
     * Estimated customer happiness for this turn (0 = very unhappy, 1 = very happy).
     */
    customerHappinessScore: number;
    /**
     * Estimated degree to which the customer's needs were met (0 = not met, 1 = fully met).
     */
    needsSatisfactionScore: number;
    /**
     * One-sentence summary of what the user requested.
     */
    requestSummary: string;
    /**
     * The agent's assessment of where the conversation stands after this turn.
     */
    resolutionStatus: 'resolved' | 'in_progress' | 'requires_escalation' | 'needs_more_info' | 'blocked';
    /**
     * Ordered list of suggested follow-up actions the client UI may surface to the user.
     */
    suggestedNextActions: string[];
}

// ── from actions/submit-identity-intake.schema.json ──
/**
 * Fields sent by the client to submit (or decline) identity intake.
 */
export interface SubmitIdentityIntakeRequest {
    /**
     * Action discriminator.
     */
    action: 'submit_identity_intake';
    /**
     * Must match the `requestId` from the `identity_intake_required` event being responded to.
     */
    requestId: string;
    /**
     * Session ID of the parked session.
     */
    sessionId: string;
    /**
     * The visitor's identity values. Required unless `declined` is true. Only the fields requested by the `identity_intake_required` event are meaningful; the server validates required-ness against that request.
     */
    values?: {
        /**
         * The visitor's display name.
         */
        name?: string;
        /**
         * The visitor's email address (validated server-side).
         */
        email?: string;
        /**
         * The visitor's phone number (normalized server-side to E.164).
         */
        phone?: string;
    };
    /**
     * True when the visitor refused to share their details. The turn resumes with a declined payload so the agent can proceed gracefully. When true, `values` is ignored.
     */
    declined?: boolean;
}

// ── from actions/submit-identity-intake.schema.json ──
/**
 * No dedicated response event for `submit_identity_intake`. Valid values (or a decline) are acked with an `immediate_response` and the parked turn resumes its normal streaming sequence. Invalid values emit `identity_intake_invalid` (the turn stays parked). This schema is provided for documentation completeness only.
 */
export interface SubmitIdentityIntakeResponse {
    [k: string]: unknown;
}

// ── from actions/verify-otp.schema.json ──
/**
 * Fields sent by the client to submit an OTP code.
 */
export interface VerifyOtpRequest {
    /**
     * Action discriminator.
     */
    action: 'verify_otp';
    /**
     * Must match the `requestId` from the `otp_verification_required` event being responded to.
     */
    requestId: string;
    /**
     * Session ID of the paused session.
     */
    sessionId: string;
    /**
     * The one-time password code entered by the user.
     */
    code: string;
}

// ── from actions/verify-otp.schema.json ──
/**
 * No dedicated response — outcome signalled by `otp_verified` or `otp_invalid` events.
 */
export interface VerifyOtpResponse {
    [k: string]: unknown;
}

// ── from domain/checkpoint.schema.json ──
/**
 * A point-in-time snapshot of a smooth-operator agent's state. Checkpoints are written by the agent runtime and are used to resume execution after interruptions (Lambda cold starts, HITL pauses, network errors). Corresponds to the `Checkpoint` struct in the smooth-operator Rust crate.
 */
export interface Checkpoint {
    /**
     * Unique checkpoint identifier (UUID v4).
     */
    id: string;
    /**
     * smooth-operator workflow thread identifier this checkpoint belongs to. Matches `Session.threadId` for the associated session.
     */
    threadId: string;
    /**
     * The agent whose state is captured in this checkpoint.
     */
    agentId: string;
    /**
     * Agent loop iteration counter at the time the checkpoint was taken.
     */
    iteration: number;
    /**
     * Arbitrary string key/value metadata attached to this checkpoint (e.g. phase name, bead ID).
     */
    metadata?: {
        [k: string]: string;
    };
    /**
     * ISO 8601 timestamp when the checkpoint was created.
     */
    createdAt: string;
}

// ── from domain/citation.schema.json ──
/**
 * A source the agent used to ground its answer. Each citation points back at one retrieved knowledge-base document — the chunk the model read, plus enough metadata to render an attribution link. Citations are collected by the runtime from the documents that actually grounded a turn (the auto-injected `[Relevant knowledge]` context and any `knowledge_search` tool results) and attached to the terminal `eventual_response`. For GitHub-sourced documents `url` is the blob/issue URL; documents without a web source omit it.
 */
export interface Citation {
    /**
     * Stable identifier of the cited source document (the knowledge-base `document_id`). Used to deduplicate citations within a turn.
     */
    id: string;
    /**
     * Human-readable label for the source — typically the document's source path or, for web-sourced docs, the URL/title.
     */
    title: string;
    /**
     * Canonical link to the source, when one exists. For GitHub-sourced documents this is the blob/issue URL stamped onto the document's `source` at ingest (see CONNECTORS.md). Absent for sources with no web location (e.g. uploaded files).
     */
    url?: string;
    /**
     * The retrieved chunk text that grounded the answer, truncated to a bounded length for display.
     */
    snippet: string;
    /**
     * Relevance score of this source for the turn's query (the knowledge-base similarity score). Higher is more relevant.
     */
    score: number;
}

// ── from domain/conversation.schema.json ──
/**
 * A conversation thread between participants (users, AI agents, or human agents). Corresponds to a row in the `conversations` table. Platform indicates the channel on which the conversation takes place.
 */
export interface Conversation {
    /**
     * Unique conversation identifier.
     */
    id: string;
    /**
     * The channel on which this conversation takes place.
     */
    platform:
        | 'web'
        | 'messenger'
        | 'instagram'
        | 'email'
        | 'discord'
        | 'phone'
        | 'sms'
        | 'slack'
        | 'whatsapp'
        | 'tiktok';
    /**
     * Human-readable display name for the conversation.
     */
    name: string;
    /**
     * The organization that owns this conversation.
     */
    organizationId: string;
    /**
     * Client-provided key that prevents duplicate conversations from being created for the same logical thread.
     */
    idempotencyKey: string;
    /**
     * Arbitrary key/value metadata attached to the conversation (e.g. campaign source, CRM fields).
     */
    metadataJson?: {
        [k: string]: unknown;
    };
    /**
     * Analytics and scoring data aggregated from messages in this conversation.
     */
    analyticsJson?: {
        [k: string]: unknown;
    };
    /**
     * ISO 8601 timestamp when the conversation was created.
     */
    createdAt: string;
    /**
     * ISO 8601 timestamp when the conversation was last updated.
     */
    updatedAt: string;
}

// ── from domain/message.schema.json ──
/**
 * A single message within a conversation. Direction is from the conversation's perspective: `inbound` = arriving from the user/external party; `outbound` = sent by the agent or platform. Corresponds to a row in the `conversation_messages` table.
 */
export interface Message {
    /**
     * Unique message identifier.
     */
    id: string;
    /**
     * ID assigned by an external platform (e.g. a Twilio SID or Messenger message id).
     */
    externalId?: string | null;
    /**
     * The organization that owns this message.
     */
    organizationId?: string | null;
    /**
     * The conversation this message belongs to.
     */
    conversationId?: string | null;
    /**
     * Message direction relative to the platform: `inbound` = from user/external, `outbound` = from agent/platform.
     */
    direction: 'inbound' | 'outbound';
    content: MessageContent;
    /**
     * Abbreviated sender descriptor (wire shape used in API responses; full participant data lives in domain/participant.schema.json).
     */
    from?: {
        /**
         * Participant ID of the sender.
         */
        id: string;
        /**
         * Participant type of the sender.
         */
        type: string;
        /**
         * Display name of the sender.
         */
        name?: string | null;
    } | null;
    /**
     * Abbreviated recipient descriptor.
     */
    to?: {
        /**
         * Participant ID of the recipient.
         */
        id: string;
        /**
         * Participant type of the recipient.
         */
        type: string;
        /**
         * Display name of the recipient.
         */
        name?: string | null;
    } | null;
    /**
     * Arbitrary key/value metadata attached to this message.
     */
    metadataJson?: {
        [k: string]: unknown;
    } | null;
    /**
     * Analytics data associated with this message (e.g. sentiment scores, token counts).
     */
    analyticsJson?: {
        [k: string]: unknown;
    } | null;
    /**
     * ISO 8601 timestamp when the message was created.
     */
    createdAt: string;
    /**
     * ISO 8601 timestamp when the message was last updated.
     */
    updatedAt?: string | null;
}
/**
 * The message payload.
 */
export interface MessageContent {
    /**
     * Ordered list of content items that make up this message.
     */
    items?: ContentItem[];
    /**
     * Convenience flat-text representation of the message (populated for simple text-only messages).
     */
    text?: string | null;
    /**
     * Structured agent response payload. Shape varies by agent template.
     */
    structuredResponse?: {
        [k: string]: unknown;
    } | null;
}
/**
 * A single content element within a message. Currently only `text` items are defined; additional types (image, file, tool_result) may be added in future protocol versions.
 */
export interface ContentItem {
    /**
     * Content item type discriminator.
     */
    type: 'text';
    /**
     * The text content (required when type = `text`).
     */
    text?: string;
}

// ── from domain/participant.schema.json ──
/**
 * A participant in a conversation. Participants may be end users, AI agents, or human support agents. Corresponds to a row in the `conversation_participants` table.
 */
export interface Participant {
    /**
     * Unique participant identifier.
     */
    id: string;
    /**
     * The conversation this participant belongs to.
     */
    conversationId: string;
    /**
     * The organization that owns this participant record.
     */
    organizationId: string;
    /**
     * Participant role: `user` = end-user, `ai-agent` = smooth-operator agent, `human-agent` = live support agent.
     */
    type: 'user' | 'ai-agent' | 'human-agent';
    /**
     * External identity (e.g. Supabase auth user UUID) for authenticated participants.
     */
    externalId?: string | null;
    /**
     * Internal system identifier (e.g. agent UUID from the agents table).
     */
    internalId?: string | null;
    /**
     * Browser fingerprint (ThumbmarkJS) for anonymous user identification.
     */
    browserFingerprint?: string | null;
    /**
     * Parsed browser / device metadata collected at session start.
     */
    browserInfo?: {
        [k: string]: unknown;
    } | null;
    /**
     * Display name for this participant.
     */
    name: string;
    /**
     * Email address if known.
     */
    email?: string | null;
    /**
     * Phone number in E.164 format if known.
     */
    phone?: string | null;
    /**
     * Foreign key into the CRM contacts table if this participant has been matched.
     */
    crmContactId?: string | null;
    /**
     * Arbitrary key/value metadata attached to this participant.
     */
    metadataJson?: {
        [k: string]: unknown;
    };
    /**
     * ISO 8601 timestamp when the participant record was created.
     */
    createdAt: string;
    /**
     * ISO 8601 timestamp when the participant record was last updated.
     */
    updatedAt: string;
}

// ── from domain/session.schema.json ──
/**
 * An AI conversation session. Ties together a conversation, an agent, the user and agent participants, and the smooth-operator workflow thread. Corresponds to a row in the `conversation_sessions` table. The `threadId` field is the smooth-operator thread identifier (stored as `langgraph_thread_id` in the DB for historical reasons; renamed to `threadId` in the protocol).
 */
export interface Session {
    /**
     * Unique session identifier.
     */
    sessionId: string;
    /**
     * The conversation this session is attached to.
     */
    conversationId: string;
    /**
     * The organization that owns this session. Mirrors `organizationId` on the conversation, participants, and messages so org-scoping is uniform across every domain type and storage backends can write the session's org directly.
     */
    organizationId: string;
    /**
     * The agent handling this session.
     */
    agentId: string;
    /**
     * Human-readable display name of the agent.
     */
    agentName: string;
    /**
     * The participant record representing the end user in this session.
     */
    userParticipantId: string;
    /**
     * The participant record representing the AI agent in this session.
     */
    agentParticipantId: string;
    /**
     * smooth-operator workflow thread identifier. Used to resume agent state across turns and process restarts. Stored as `langgraph_thread_id` in the database for historical reasons.
     */
    threadId: string;
    /**
     * Lifecycle status of the session.
     */
    status?: 'active' | 'idle' | 'ended';
    /**
     * Cumulative token count consumed in this session.
     */
    tokenCount?: number;
    /**
     * Number of messages exchanged in this session.
     */
    messageCount?: number;
    /**
     * Arbitrary key/value metadata attached to this session (e.g. browser info, campaign source).
     */
    metadata?: {
        [k: string]: unknown;
    };
    /**
     * ISO 8601 timestamp when the session was created.
     */
    createdAt?: string;
    /**
     * ISO 8601 timestamp when the session was last updated.
     */
    updatedAt?: string;
    /**
     * ISO 8601 timestamp when the session ended, or null if still active.
     */
    endedAt?: string | null;
    /**
     * ISO 8601 timestamp of the most recent activity (message, keepalive, etc.).
     */
    lastActivityAt?: string;
}

// ── from envelope.schema.json ──
/**
 * A structured error descriptor. Used in error events and nested inside event data payloads.
 */
export interface ErrorObject {
    /**
     * Machine-readable error code (e.g. `SESSION_NOT_FOUND`, `RATE_LIMITED`, `VALIDATION_ERROR`).
     */
    code: string;
    /**
     * Human-readable error description.
     */
    message: string;
}

// ── from envelope.schema.json ──
/**
 * Base shape for every client-to-server WebSocket frame. The `action` field is a snake_case verb that selects the handler. `requestId` is chosen by the client and echoed back on all related server events so the client can correlate responses to pending requests.
 */
export interface ActionEnvelope {
    /**
     * The action to perform.
     */
    action:
        | 'create_conversation_session'
        | 'send_message'
        | 'get_session'
        | 'get_conversation_messages'
        | 'confirm_tool_action'
        | 'verify_otp'
        | 'submit_identity_intake'
        | 'ping';
    /**
     * Client-generated correlation ID. Will be echoed back on all related server events. Should be unique per in-flight request. If omitted the server may generate one, but correlating responses becomes the client's problem.
     */
    requestId?: string;
    [k: string]: unknown;
}

// ── from events/error.schema.json ──
/**
 * Event: `error`. Emitted when an unrecoverable error occurs during request processing. The nested `error` object shape (`{ code, message }`) is preserved for wire compatibility with clients that destructure `message.error.code`. `details` carries additional structured context when available.
 */
export interface Error {
    /**
     * Event type discriminator.
     */
    type: 'error';
    /**
     * Echoes the `requestId` from the originating action, if applicable. Absent for server-initiated errors with no associated request.
     */
    requestId?: string;
    /**
     * Top-level error object (duplicate of `data.error`; kept for clients that pattern-match on the envelope-level `error` field).
     */
    error?: {
        /**
         * Machine-readable error code.
         */
        code: string;
        /**
         * Human-readable error description.
         */
        message: string;
    };
    /**
     * Full error payload.
     */
    data: {
        /**
         * The request ID this error belongs to, if any.
         */
        requestId?: string;
        /**
         * The error descriptor (nested for wire backward-compatibility).
         */
        error: {
            /**
             * Machine-readable error code (e.g. `SESSION_NOT_FOUND`, `RATE_LIMITED`, `VALIDATION_ERROR`, `INTERNAL_ERROR`).
             */
            code: string;
            /**
             * Human-readable error description.
             */
            message: string;
        };
        /**
         * Optional additional structured error context (e.g. validation issues, partial results).
         */
        details?: {} | unknown[] | string | number | boolean | null;
    };
    /**
     * Unix epoch milliseconds when the event was emitted.
     */
    timestamp?: number;
}

// ── from events/eventual-response.schema.json ──
/**
 * Event: `eventual_response`. The terminal event of a streaming turn. Emitted after the agent workflow completes and its output has been persisted. Clients should treat this as the authoritative final state for the turn and may discard intermediate `stream_chunk` / `stream_token` data. Status is always 200 on success.
 */
export interface EventualResponse {
    /**
     * Event type discriminator.
     */
    type: 'eventual_response';
    /**
     * Echoes the `requestId` from the originating `send_message` action.
     */
    requestId?: string;
    /**
     * HTTP-like status. Always 200 for a successful eventual response.
     */
    status?: number;
    /**
     * The terminal response payload.
     */
    data: {
        /**
         * The request ID this response belongs to.
         */
        requestId: string;
        /**
         * HTTP-like status. Typically 200.
         */
        status: number;
        /**
         * The final agent output.
         */
        data: {
            /**
             * ID of the agent message as persisted to the `conversation_messages` table.
             */
            messageId: string;
            /**
             * The structured agent response payload. Shape depends on the agent template. Clients should validate against a template-specific schema before rendering.
             */
            response: {} | unknown[] | string | number | boolean | null;
            /**
             * True if the agent flagged this conversation for human escalation.
             */
            needsEscalation?: boolean;
            /**
             * Human-readable escalation reason when `needsEscalation` is true.
             */
            escalationReason?: string;
            /**
             * Per-turn token accounting and cost, captured from the engine's terminal completion event. Lets a client accumulate live session cost. Optional and back-compatible: absent when the engine reported no usage for the turn (e.g. an offline/mock turn).
             */
            usage?: {
                /**
                 * Accumulated cost in USD across every LLM call in this turn (gateway-priced).
                 */
                costUsd?: number;
                /**
                 * Accumulated prompt (input) tokens across every LLM call in this turn.
                 */
                promptTokens?: number;
                /**
                 * Accumulated completion (output) tokens across every LLM call in this turn.
                 */
                completionTokens?: number;
            };
            /**
             * The sources that grounded this answer, when any were retrieved. Collected by the runtime from the documents that actually grounded the turn — the auto-injected `[Relevant knowledge]` context and any `knowledge_search` tool results — deduplicated by source id and capped. Optional and back-compatible: absent when the turn used no knowledge sources. Each item is a `Citation` (see `domain/citation.schema.json`).
             */
            citations?: {
                /**
                 * Stable identifier of the cited source document (the knowledge-base `document_id`). Used to deduplicate citations within a turn.
                 */
                id: string;
                /**
                 * Human-readable label for the source — typically the document's source path or, for web-sourced docs, the URL/title.
                 */
                title: string;
                /**
                 * Canonical link to the source, when one exists. For GitHub-sourced documents this is the blob/issue URL stamped onto the document's `source` at ingest. Absent for sources with no web location.
                 */
                url?: string;
                /**
                 * The retrieved chunk text that grounded the answer, truncated to a bounded length for display.
                 */
                snippet: string;
                /**
                 * Relevance score of this source for the turn's query (the knowledge-base similarity score). Higher is more relevant.
                 */
                score: number;
            }[];
        };
    };
    /**
     * Unix epoch milliseconds when the event was emitted.
     */
    timestamp?: number;
}

// ── from events/identity-intake-invalid.schema.json ──
/**
 * Event: `identity_intake_invalid`. Emitted when a `submit_identity_intake` action carried values that failed server-side validation (missing required field, malformed email, unparseable phone). The turn REMAINS parked — the client should re-render the intake form with the per-field errors and let the visitor resubmit. Mirrors `otp_invalid`: invalid input is a retryable state, never a terminal `error` event.
 */
export interface IdentityIntakeInvalid {
    /**
     * Event type discriminator.
     */
    type: 'identity_intake_invalid';
    /**
     * Echoes the `requestId` of the parked turn (same correlation as the `identity_intake_required` event).
     */
    requestId?: string;
    /**
     * Validation failure details.
     */
    data: {
        /**
         * The request ID this intake belongs to.
         */
        requestId: string;
        /**
         * Per-field validation errors.
         */
        data: {
            /**
             * One entry per failed field.
             *
             * @minItems 1
             */
            errors: [
                {
                    /**
                     * The field that failed validation.
                     */
                    field: 'name' | 'email' | 'phone';
                    /**
                     * Human-readable validation message for this field.
                     */
                    message: string;
                },
                ...{
                    /**
                     * The field that failed validation.
                     */
                    field: 'name' | 'email' | 'phone';
                    /**
                     * Human-readable validation message for this field.
                     */
                    message: string;
                }[],
            ];
            /**
             * Human-readable summary suitable for a form-level error line.
             */
            message: string;
        };
    };
    /**
     * Unix epoch milliseconds when the event was emitted.
     */
    timestamp?: number;
}

// ── from events/identity-intake-required.schema.json ──
/**
 * Event: `identity_intake_required`. Emitted mid-turn when the agent requests structured identity/lead intake (name / email / phone) and the session declared the `identity_form` capability at `create_conversation_session`. The turn is parked until the client replies with a `submit_identity_intake` action carrying the same `requestId` (values or `declined: true`). Sessions that did not declare `identity_form` never receive this event — the server degrades to conversational, turn-by-turn collection instead.
 */
export interface IdentityIntakeRequired {
    /**
     * Event type discriminator.
     */
    type: 'identity_intake_required';
    /**
     * Echoes the `requestId` from the originating `send_message` action. Must be included in the `submit_identity_intake` reply.
     */
    requestId?: string;
    /**
     * Intake prompt details.
     */
    data: {
        /**
         * The request ID this intake belongs to.
         */
        requestId: string;
        /**
         * The fields the agent wants and why.
         */
        data: {
            /**
             * The identity fields to collect, in display order.
             *
             * @minItems 1
             */
            fields: [
                {
                    /**
                     * Which identity field to collect.
                     */
                    key: 'name' | 'email' | 'phone';
                    /**
                     * Whether the visitor must provide this field to submit.
                     */
                    required: boolean;
                    /**
                     * Optional display label overriding the client's default for this field.
                     */
                    label?: string;
                },
                ...{
                    /**
                     * Which identity field to collect.
                     */
                    key: 'name' | 'email' | 'phone';
                    /**
                     * Whether the visitor must provide this field to submit.
                     */
                    required: boolean;
                    /**
                     * Optional display label overriding the client's default for this field.
                     */
                    label?: string;
                }[],
            ];
            /**
             * Human-readable reason the agent needs these details, suitable for the form header (e.g. `to send you the quote`).
             */
            reason: string;
        };
    };
    /**
     * Unix epoch milliseconds when the event was emitted.
     */
    timestamp?: number;
}

// ── from events/immediate-response.schema.json ──
/**
 * Event: `immediate_response`. Sent by the server synchronously upon receiving any action, to acknowledge that the request was accepted and processing has begun. For streaming actions (`send_message`) this always precedes `stream_chunk` / `stream_token` events and the final `eventual_response`. For non-streaming actions (e.g. `get_session`, `create_conversation_session`) this also carries the complete response payload in `data`.
 */
export interface ImmediateResponse {
    /**
     * Event type discriminator.
     */
    type: 'immediate_response';
    /**
     * Echoes the `requestId` from the originating action.
     */
    requestId?: string;
    /**
     * HTTP-like status. 202 = accepted and processing; 200 = synchronous success (non-streaming responses).
     */
    status?: number;
    /**
     * Human-readable status description (e.g. `Processing your request...`).
     */
    message?: string;
    /**
     * Action-specific response payload. For `create_conversation_session` and `get_session`, this is the session descriptor. For `get_conversation_messages`, this is the message page. For streaming `send_message`, this is typically empty or contains only a minimal ack.
     */
    data: {
        [k: string]: unknown;
    };
    /**
     * Unix epoch milliseconds when the event was emitted.
     */
    timestamp?: number;
}

// ── from events/keepalive.schema.json ──
/**
 * Event: `keepalive`. Sent periodically by the server during long-running agent turns (typically every 30 seconds) to prevent AWS API Gateway's 10-minute idle connection timeout from closing the WebSocket while the backend is still computing. Clients should acknowledge receipt by updating their last-seen timestamp, but no reply action is needed. Distinct from `ping`/`pong` which are client-initiated.
 */
export interface Keepalive {
    /**
     * Event type discriminator.
     */
    type: 'keepalive';
    /**
     * The `requestId` of the in-flight request this keepalive is associated with.
     */
    requestId?: string;
    /**
     * Keepalive payload.
     */
    data: {
        /**
         * The request ID of the in-flight request this keepalive belongs to.
         */
        requestId: string;
    };
    /**
     * Unix epoch milliseconds when the keepalive was sent.
     */
    timestamp?: number;
}

// ── from events/otp-invalid.schema.json ──
/**
 * Event: `otp_invalid`. Emitted when the caller's OTP attempt is rejected — wrong code, expired, max attempts reached, or record not found. When `attemptsRemaining` is 0 the session is locked; the client must restart the OTP flow. When greater than 0 the client may prompt the user to try again.
 */
export interface OtpInvalid {
    /**
     * Event type discriminator.
     */
    type: 'otp_invalid';
    /**
     * Echoes the `requestId` from the originating `verify_otp` action.
     */
    requestId?: string;
    /**
     * Failure details.
     */
    data: {
        /**
         * The request ID this verification belongs to.
         */
        requestId: string;
        /**
         * OTP invalid payload.
         */
        data: {
            /**
             * Machine-readable failure reason. Absent if the server cannot determine a specific cause.
             */
            error?: 'INVALID_CODE' | 'MAX_ATTEMPTS' | 'NOT_FOUND' | 'EXPIRED';
            /**
             * How many more attempts the caller has before the OTP is locked. Zero means the session is locked and a new OTP must be requested.
             */
            attemptsRemaining: number;
            /**
             * Human-readable failure message suitable for display in the verification UI.
             */
            message: string;
        };
    };
    /**
     * Unix epoch milliseconds when the event was emitted.
     */
    timestamp?: number;
}

// ── from events/otp-sent.schema.json ──
/**
 * Event: `otp_sent`. Acknowledgement that an OTP code has been dispatched to the user via the chosen delivery channel. The client should update the UI to prompt the user to enter the code they received.
 */
export interface OtpSent {
    /**
     * Event type discriminator.
     */
    type: 'otp_sent';
    /**
     * Echoes the `requestId` from the originating action.
     */
    requestId?: string;
    /**
     * OTP send acknowledgement details.
     */
    data: {
        /**
         * The request ID this OTP delivery belongs to.
         */
        requestId: string;
        /**
         * Delivery details.
         */
        data: {
            /**
             * The channel through which the OTP was delivered.
             */
            channel: 'email' | 'sms';
            /**
             * Partially masked destination address for display in the UI (e.g. `j***@example.com` or `+1 ***-***-4567`). Sufficient for the user to recognize their own address without exposing it fully.
             */
            maskedDestination: string;
        };
    };
    /**
     * Unix epoch milliseconds when the event was emitted.
     */
    timestamp?: number;
}

// ── from events/otp-verification-required.schema.json ──
/**
 * Event: `otp_verification_required`. Emitted when the agent workflow pauses because it needs the caller to complete OTP verification before proceeding with an authenticated action. Corresponds to smooth-operator's `AgentEvent::HumanInputRequired { Input }` for auth gates. The client should surface a channel-selection and OTP input UI. After the user selects a channel, the client may trigger OTP delivery via a separate flow; the `verify_otp` action submits the received code.
 */
export interface OtpVerificationRequired {
    /**
     * Event type discriminator.
     */
    type: 'otp_verification_required';
    /**
     * Echoes the `requestId` from the originating `send_message` action. Must be included in the `verify_otp` reply.
     */
    requestId?: string;
    /**
     * Verification prompt details.
     */
    data: {
        /**
         * The request ID this verification belongs to.
         */
        requestId: string;
        /**
         * Details about the authentication requirement.
         */
        data: {
            /**
             * Opaque identifier of the tool invocation awaiting verification.
             */
            toolId: string;
            /**
             * Human-readable description of the action requiring verification, suitable for the consent UI.
             */
            actionDescription: string;
            /**
             * Delivery channels available to send the OTP. The client should let the user choose.
             *
             * @minItems 1
             */
            availableChannels: ['email' | 'sms', ...('email' | 'sms')[]];
            /**
             * Required authentication level. Common values: `email`, `sms`, `mfa`, `end_user`, `admin`.
             */
            authLevel: string;
        };
    };
    /**
     * Unix epoch milliseconds when the event was emitted.
     */
    timestamp?: number;
}

// ── from events/otp-verified.schema.json ──
/**
 * Event: `otp_verified`. Emitted when the caller's OTP attempt succeeds, or when a pre-auth HMAC makes OTP unnecessary. The session is now authenticated at the required level and the paused agent workflow resumes. A streaming sequence (`stream_chunk` / `stream_token` → `eventual_response`) follows.
 */
export interface OtpVerified {
    /**
     * Event type discriminator.
     */
    type: 'otp_verified';
    /**
     * Echoes the `requestId` from the originating `verify_otp` action.
     */
    requestId?: string;
    /**
     * Verification success details.
     */
    data: {
        /**
         * The request ID this verification belongs to.
         */
        requestId: string;
        /**
         * Success payload.
         */
        data: {
            /**
             * Short human-readable confirmation message (e.g. `Identity verified successfully.`).
             */
            message: string;
        };
    };
    /**
     * Unix epoch milliseconds when the event was emitted.
     */
    timestamp?: number;
}

// ── from events/pong.schema.json ──
/**
 * Event: `pong`. The server's reply to a `ping` action. Carries the server's current Unix epoch timestamp in milliseconds. Clients use the round-trip time to detect zombie connections.
 */
export interface Pong {
    /**
     * Event type discriminator.
     */
    type: 'pong';
    /**
     * Echoes the `requestId` from the originating `ping` action.
     */
    requestId?: string;
    /**
     * Server-side Unix epoch milliseconds when the pong was emitted.
     */
    timestamp?: number;
    /**
     * Pong payload (mirrors top-level `timestamp` for clients that only inspect `data`).
     */
    data?: {
        /**
         * Server-side Unix epoch milliseconds.
         */
        timestamp: number;
    };
}

// ── from events/stream-chunk.schema.json ──
/**
 * Event: `stream_chunk`. Emitted each time a node in the smooth-operator workflow completes. Carries the node name and a filtered state snapshot. Clients use this to show per-node progress (e.g. `knowledge_search completed`, tool activity) in an agent team view. Distinct from `stream_token` which carries raw token deltas; a `stream_chunk` typically fires once per node while multiple `stream_token` events may fire within a single node's LLM call.
 */
export interface StreamChunk {
    /**
     * Event type discriminator.
     */
    type: 'stream_chunk';
    /**
     * Echoes the `requestId` from the originating `send_message` action.
     */
    requestId?: string;
    /**
     * Name of the workflow node that just completed and produced this chunk.
     */
    node?: string;
    /**
     * The per-node state snapshot.
     */
    data: {
        /**
         * The request ID this chunk belongs to.
         */
        requestId: string;
        /**
         * Name of the workflow node that produced this state snapshot.
         */
        node: string;
        /**
         * Filtered subset of the node's output state exposed to the client. Only safe-to-expose fields are included; server-internal state is stripped.
         */
        state: {
            /**
             * Unstructured LLM output from this node, if any.
             */
            rawResponse?: {} | unknown[] | string | number | boolean | null;
            /**
             * Parsed structured response from this node, if any.
             */
            structuredResponse?: {} | unknown[] | string | number | boolean | null;
            /**
             * Non-null when the workflow has paused awaiting user confirmation of a write operation. Clients should surface a confirmation UI.
             */
            pendingWriteConfirmation?: {
                [k: string]: unknown;
            } | null;
            /**
             * Non-null when the workflow has paused awaiting OTP verification. Clients should surface an OTP input UI.
             */
            pendingOtpVerification?: {
                [k: string]: unknown;
            } | null;
        };
        /**
         * Reserved for future use. When true, this is the last `stream_chunk` for the request. Currently clients should use `eventual_response` to detect stream completion.
         */
        done?: boolean;
    };
    /**
     * Unix epoch milliseconds when the event was emitted.
     */
    timestamp?: number;
}

// ── from events/stream-reasoning.schema.json ──
/**
 * Event: `stream_reasoning`. A single *reasoning* token from a reasoning-model's separate thinking channel (`reasoning_content`/`reasoning` deltas — e.g. DeepSeek, gpt-oss/harmony, MiniMax, GLM), forwarded to the client in real time. Corresponds to smooth-operator's `AgentEvent::ReasoningDelta`. Shaped identically to `stream_token` so clients can render it the same way, but on a distinct `type` so reasoning is shown as collapsible "thinking" and is NEVER folded into the answer. The final response (carried by `eventual_response`) already excludes reasoning. Clients that do not recognize this event MUST ignore it — the answer still streams via `stream_token`, so reasoning simply isn't shown.
 */
export interface StreamReasoning {
    /**
     * Event type discriminator.
     */
    type: 'stream_reasoning';
    /**
     * Echoes the `requestId` from the originating `send_message` action.
     */
    requestId?: string;
    /**
     * The raw reasoning token text. Also present inside `data.token` for consumers that only inspect `data`.
     */
    token?: string;
    /**
     * Reasoning token event payload.
     */
    data: {
        /**
         * The request ID this reasoning token belongs to.
         */
        requestId: string;
        /**
         * The raw reasoning token text.
         */
        token: string;
    };
    /**
     * Unix epoch milliseconds when the event was emitted.
     */
    timestamp?: number;
}

// ── from events/stream-token.schema.json ──
/**
 * Event: `stream_token`. A single LLM output token forwarded to the client in real time. Corresponds to smooth-operator's `AgentEvent::TokenDelta`. Clients accumulate tokens to display a live typing animation. After the node finishes, a `stream_chunk` event carries the complete state snapshot for that node.
 */
export interface StreamToken {
    /**
     * Event type discriminator.
     */
    type: 'stream_token';
    /**
     * Echoes the `requestId` from the originating `send_message` action.
     */
    requestId?: string;
    /**
     * The raw token text. Also present inside `data.token` for consumers that only inspect `data`.
     */
    token?: string;
    /**
     * Token event payload.
     */
    data: {
        /**
         * The request ID this token belongs to.
         */
        requestId: string;
        /**
         * The raw token text.
         */
        token: string;
    };
    /**
     * Unix epoch milliseconds when the event was emitted.
     */
    timestamp?: number;
}

// ── from events/write-confirmation-required.schema.json ──
/**
 * Event: `write_confirmation_required`. Emitted when the agent workflow pauses before running a state-mutating tool call that requires explicit user approval. Corresponds to smooth-operator's `AgentEvent::HumanInputRequired { Confirm }`. The client must surface a confirmation dialog and reply with a `confirm_tool_action` action using the same `requestId`. Until the client responds, the workflow remains paused.
 */
export interface WriteConfirmationRequired {
    /**
     * Event type discriminator.
     */
    type: 'write_confirmation_required';
    /**
     * Echoes the `requestId` from the originating `send_message` action. Must be included in the `confirm_tool_action` reply.
     */
    requestId?: string;
    /**
     * Confirmation prompt details.
     */
    data: {
        /**
         * The request ID this confirmation belongs to.
         */
        requestId: string;
        /**
         * Details about the pending write that the user must approve or reject.
         */
        data: {
            /**
             * Opaque identifier of the tool invocation awaiting confirmation. Provided for correlation; clients do not need to parse or display this.
             */
            toolId: string;
            /**
             * Human-readable description of the action the agent wants to perform, suitable for displaying in a confirmation dialog (e.g. `Delete contact John Doe (john@example.com)`).
             */
            actionDescription: string;
        };
    };
    /**
     * Unix epoch milliseconds when the event was emitted.
     */
    timestamp?: number;
}
