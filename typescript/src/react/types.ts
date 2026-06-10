/**
 * Shared types for the React bindings.
 *
 * The message + citation shapes mirror `@smooai/chat-widget`'s
 * `ConversationController` exactly, so the two presentation layers stay
 * interchangeable over the same protocol client.
 */
import type { Citation } from '../types.js';

export type { Citation };

export type Role = 'user' | 'assistant';

/** A single rendered chat message. Assistant messages grow as tokens stream in. */
export interface ChatMessage {
    id: string;
    role: Role;
    /** Accumulated text (assistant messages grow as `stream_token` events arrive). */
    text: string;
    /** True while an assistant message is still streaming. */
    streaming: boolean;
    /** Grounding sources, present only when the terminal `eventual_response` carried any. */
    citations?: Citation[];
}

/** Connection lifecycle of a conversation. */
export type ConnectionStatus = 'idle' | 'connecting' | 'ready' | 'error' | 'closed';
