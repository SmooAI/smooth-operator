/**
 * Drives one `send_message` turn.
 *
 * The TypeScript port of the C# `TurnRunner.cs` and the Rust server's
 * `run_streaming_turn`: load prior history, retrieve grounding knowledge as
 * citations, run the `@smooai/smooth-operator-core` engine ({@link SmoothAgent})
 * in STREAMING mode, emit a `stream_token` per text delta and a `stream_chunk` per
 * tool call / tool result, persist the reply, and return the citations.
 *
 * The engine is consumed exactly as the C# server consumes its `SmoothAgent`: one
 * agent per turn, prior messages replayed onto a fresh thread as memory, then
 * `runStream` mapped event-by-event onto protocol events.
 */
import { SmoothAgent } from '@smooai/smooth-operator-core';
import type { AgentOptions, ChatClientLike, Knowledge, StreamEvent, Tool } from '@smooai/smooth-operator-core';

import * as protocol from './protocol.js';
import type { Citation, Frame } from './protocol.js';
import type { SessionStore } from './sessionStore.js';

/** What a completed turn produced (the analog of the C#/Rust `TurnResult`). */
export interface TurnResult {
    reply: string;
    messageId: string;
    citations: Citation[];
}

const AUTO_CONTEXT_LIMIT = 3;
const MAX_PRIOR_MESSAGES = 50;
const CITATION_SNIPPET_MAX_CHARS = 280;

const DEFAULT_SYSTEM_PROMPT =
    'You are a helpful customer support agent. Answer using only the knowledge provided to you; if it is not there, say you don\'t know.';

/** A sink the runner pushes outbound protocol frames into (single-writer downstream). */
export type Sink = (frame: Frame) => void;

export interface TurnRunnerOptions {
    /** The OpenAI-compatible engine client (gateway in prod, {@link MockLlmProvider} in tests). */
    chatClient: ChatClientLike;
    store: SessionStore;
    /** Optional knowledge retriever, already SCOPED to the connection's access (ACL). */
    knowledge?: Knowledge;
    systemPrompt?: string;
    /** Tools the agent may call during the turn (default none); passed straight to the engine. */
    tools?: Tool[];
}

export class TurnRunner {
    private readonly chatClient: ChatClientLike;
    private readonly store: SessionStore;
    private readonly knowledge?: Knowledge;
    private readonly systemPrompt: string;
    private readonly tools: Tool[];

    constructor(options: TurnRunnerOptions) {
        this.chatClient = options.chatClient;
        this.store = options.store;
        this.knowledge = options.knowledge;
        this.systemPrompt = options.systemPrompt ?? DEFAULT_SYSTEM_PROMPT;
        this.tools = options.tools ?? [];
    }

    /**
     * Run the turn, streaming events to `sink`. `signal`, when aborted, cooperatively
     * stops streaming further events (an in-flight turn drains what it has). Returns
     * the final reply + citations so the caller can emit the terminal event.
     */
    async run(conversationId: string, requestId: string, userMessage: string, sink: Sink, signal?: AbortSignal): Promise<TurnResult> {
        // 1. Auto-context citations (what grounded the answer). Mirrors the Rust
        //    auto_sources / C# citation build. The engine's Knowledge.query is the
        //    same retriever the agent injects from, so the citations match the
        //    grounding the model actually saw.
        const citations: Citation[] = [];
        if (this.knowledge) {
            const hits = this.knowledge.query(userMessage, AUTO_CONTEXT_LIMIT);
            for (const hit of hits) {
                const isUrl = hit.source.startsWith('http://') || hit.source.startsWith('https://');
                citations.push({
                    id: hit.source,
                    title: hit.source,
                    url: isUrl ? hit.source : undefined,
                    snippet: truncate(hit.content, CITATION_SNIPPET_MAX_CHARS),
                    score: hit.score,
                });
            }
        }

        // 2. Build the agent + replay prior history as the thread (before persisting
        //    this turn's inbound message). The engine consumes history as OpenAI-format
        //    messages passed to runStream.
        const agentOptions: AgentOptions = { instructions: this.systemPrompt };
        if (this.knowledge) agentOptions.knowledge = this.knowledge;
        if (this.tools.length > 0) agentOptions.tools = this.tools;
        const agent = new SmoothAgent(this.chatClient, agentOptions);

        const prior = await this.store.listMessages(conversationId, MAX_PRIOR_MESSAGES);
        const history = prior.map((m) => ({
            role: m.direction === 'outbound' ? 'assistant' : 'user',
            content: m.text,
        }));

        // 3. Persist the inbound user message.
        await this.store.appendMessage(conversationId, 'inbound', userMessage);

        // 4. Stream the turn: a stream_token per text delta, a stream_chunk per tool
        //    call / tool result (the TS parity of the Rust runner translating
        //    ToolCallStart/Complete and the C# FunctionCall/FunctionResult mapping).
        let reply = '';
        for await (const event of agent.runStream(userMessage, history)) {
            if (signal?.aborted) break;
            this.emit(requestId, event, sink);
            if (event.type === 'text') reply += event.text;
        }

        // 5. Persist the outbound reply and return.
        const outbound = await this.store.appendMessage(conversationId, 'outbound', reply);
        return { reply, messageId: outbound.id, citations };
    }

    /** Map one engine {@link StreamEvent} onto its protocol event(s). */
    private emit(requestId: string, event: StreamEvent, sink: Sink): void {
        switch (event.type) {
            case 'text':
                if (event.text.length > 0) sink(protocol.streamToken(requestId, event.text));
                break;
            case 'tool_call':
                sink(protocol.streamChunk(requestId, event.name, toolCallState(event.name, event.arguments)));
                break;
            case 'tool_result':
                sink(protocol.streamChunk(requestId, event.name, toolResultState(event.name, event.result)));
                break;
            case 'done':
                // The terminal `done` carries the final AgentRunResponse; the dispatcher
                // emits the protocol's eventual_response from the returned reply, so
                // nothing is streamed for it here.
                break;
        }
    }
}

function truncate(value: string, max: number): string {
    return value.length <= max ? value : value.slice(0, max);
}

function toolCallState(name: string, args: string): Record<string, unknown> {
    let parsed: unknown = {};
    try {
        parsed = args ? JSON.parse(args) : {};
    } catch {
        // The model occasionally streams non-JSON arg fragments; surface them raw
        // rather than dropping the chunk.
        parsed = { _raw: args };
    }
    return { rawResponse: { toolCall: { name, arguments: parsed } } };
}

function toolResultState(name: string, result: string): Record<string, unknown> {
    // The engine folds tool failures into the result string; detect that convention
    // so the chunk's isError flag matches the Rust ToolCallComplete signal.
    const isError = result.startsWith('Error:') || result.startsWith('Denied by human:');
    return { rawResponse: { toolResult: { name, isError, result } } };
}
