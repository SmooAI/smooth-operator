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
import { approve, deny, SmoothAgent } from '@smooai/smooth-operator-core';
import type { AgentOptions, ChatClientLike, HumanApprovalRequest, HumanApprovalResponse, Knowledge, StreamEvent, Tool } from '@smooai/smooth-operator-core';

import type { ConfirmationRegistry } from './confirmation.js';
import type { ModelCeilingResolver } from './modelCeiling.js';
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

/**
 * Default model when the host doesn't set one. Matches the engine's own default so
 * behaviour is unchanged; also the model the per-turn output ceiling is looked up for.
 */
export const DEFAULT_MODEL = 'claude-haiku-4-5';
/**
 * Default agent-loop iteration cap. Was the engine's chat-widget-sized 6 — too tight
 * for any multi-step turn. Raised to 20 for agentic use (EPIC th-1cc9fa).
 */
export const DEFAULT_MAX_ITERATIONS = 20;
/**
 * Default `max_tokens` per LLM call. Was 512 (chat-widget sizing), which STARVES
 * reasoning models — they spend the whole budget on `reasoning_content` and return
 * empty `content`. Raised to 8192 (EPIC th-1cc9fa). Safe now that the per-model output
 * ceiling clamps this down per request: a cap only bounds runaway output, it doesn't
 * lengthen concise answers.
 */
export const DEFAULT_MAX_TOKENS = 8192;

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
    /**
     * Tool-name substrings gated behind write-confirmation HITL (default empty → no
     * gating, behavior unchanged). A tool whose name contains one of these parks the
     * turn (emits `write_confirmation_required`) until the client confirms.
     */
    confirmTools?: string[];
    /** The session-keyed pending-confirmation registry the gate parks on (shared with the dispatcher). */
    confirmations?: ConfirmationRegistry;
    /** The session id a parked confirmation is keyed by (so a `confirm_tool_action` frame routes here). */
    sessionId?: string;
    /** Model id for the turn (default {@link DEFAULT_MODEL}); also the model whose output ceiling is looked up. */
    model?: string;
    /**
     * Best-effort per-model output-ceiling resolver (from the gateway's `/model/info`).
     * When set, each turn clamps `max_tokens` to `min(DEFAULT_MAX_TOKENS, ceiling)` via
     * the engine's `modelMaxOutput`. Absent (tests, keyless local) ⇒ unclamped (EPIC th-1cc9fa).
     */
    modelCeiling?: ModelCeilingResolver;
}

export class TurnRunner {
    private readonly chatClient: ChatClientLike;
    private readonly store: SessionStore;
    private readonly knowledge?: Knowledge;
    private readonly systemPrompt: string;
    private readonly tools: Tool[];
    private readonly confirmTools: string[];
    private readonly confirmations?: ConfirmationRegistry;
    private readonly sessionId?: string;
    private readonly model: string;
    private readonly modelCeiling?: ModelCeilingResolver;

    constructor(options: TurnRunnerOptions) {
        this.chatClient = options.chatClient;
        this.store = options.store;
        this.knowledge = options.knowledge;
        this.systemPrompt = options.systemPrompt ?? DEFAULT_SYSTEM_PROMPT;
        this.tools = options.tools ?? [];
        this.confirmTools = options.confirmTools ?? [];
        this.confirmations = options.confirmations;
        this.sessionId = options.sessionId;
        this.model = options.model ?? DEFAULT_MODEL;
        this.modelCeiling = options.modelCeiling;
    }

    /** True when `name` matches a confirmation-gated pattern (substring, like the Rust hook). */
    private isGated(name: string): boolean {
        if (!this.confirmations) return false;
        return this.confirmTools.some((pattern) => name.includes(pattern));
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
        const agentOptions: AgentOptions = {
            instructions: this.systemPrompt,
            model: this.model,
            maxTokens: DEFAULT_MAX_TOKENS,
            maxIterations: DEFAULT_MAX_ITERATIONS,
        };
        if (this.knowledge) agentOptions.knowledge = this.knowledge;
        if (this.tools.length > 0) agentOptions.tools = this.tools;

        // Clamp max_tokens to the resolved model's output ceiling (best-effort; a
        // missing/unknown ceiling ⇒ unclamped). Reuses the cached /model/info fetch.
        // EPIC th-1cc9fa — the consumer half of the engine's model-output clamp.
        if (this.modelCeiling) {
            const ceiling = await this.modelCeiling(this.model);
            if (ceiling !== undefined) agentOptions.modelMaxOutput = ceiling;
        }

        // Write-confirmation HITL: when configured with tool patterns AND a registry
        // is present, install a HumanGate that parks the turn before a gated tool runs
        // (emit `write_confirmation_required`, await the client's verdict via the
        // session-keyed registry). With no patterns (the default) no gate is installed
        // → no tool ever parks → behavior identical to before HITL. The gate keys its
        // pending deferred by `sessionId`, so a `confirm_tool_action` frame (also keyed
        // by sessionId) routes back here.
        const confirmSession = this.sessionId ?? conversationId;
        if (this.confirmTools.length > 0 && this.confirmations) {
            const patterns = this.confirmTools;
            const registry = this.confirmations;
            agentOptions.requiresApproval = (name: string): boolean => patterns.some((pattern) => name.includes(pattern));
            agentOptions.humanGate = async (req: HumanApprovalRequest): Promise<HumanApprovalResponse> => {
                // Park: register a fresh deferred, emit the confirmation event, then
                // await the client's `confirm_tool_action`. The toolId is the tool name
                // (one tool parks at a time — a stable correlation key).
                //
                // Event ORDER matters for cross-language parity: the reference (Rust)
                // server emits `write_confirmation_required` BEFORE the gated tool's
                // `stream_chunk(toolCall)`. The engine, however, yields the tool_call
                // event before consulting the gate — so the stream loop DEFERS a gated
                // tool's `stream_chunk` (see `isGated`) and we emit it HERE, right after
                // the confirmation prompt, to match.
                const verdict = registry.register(confirmSession);
                sink(protocol.writeConfirmationRequired(requestId, req.toolName, req.prompt));
                sink(protocol.streamChunk(requestId, req.toolName, toolCallStateFrom(req.toolName, req.arguments)));
                const approved = await verdict;
                return approved ? approve() : deny('user rejected the action');
            };
        }

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
        try {
            for await (const event of agent.runStream(userMessage, history)) {
                if (signal?.aborted) break;
                // DEFER a confirmation-gated tool's toolCall chunk: it is emitted from
                // the gate AFTER `write_confirmation_required`, so the wire order matches
                // the reference (Rust) server. Non-gated tools emit their chunk inline.
                if (event.type === 'tool_call' && this.isGated(event.name)) continue;
                this.emit(requestId, event, sink);
                if (event.type === 'text') reply += event.text;
            }
        } finally {
            // Turn over: drop any lingering pending confirmation so a stale entry can't
            // mis-route a later `confirm_tool_action` (mirrors the Rust `(cfg.clear)`
            // at turn end). No-op when HITL is off.
            this.confirmations?.clear(confirmSession);
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

/**
 * The `stream_chunk` toolCall state built from an already-parsed `arguments` object
 * (the shape the engine's {@link HumanApprovalRequest} carries). Used to emit a gated
 * tool's deferred toolCall chunk from the HumanGate — the TS analog of the Python
 * `_tool_call_state_from`.
 */
function toolCallStateFrom(name: string, args: Record<string, unknown>): Record<string, unknown> {
    return { rawResponse: { toolCall: { name, arguments: args } } };
}

function toolResultState(name: string, result: string): Record<string, unknown> {
    // The engine folds tool failures into the result string; detect that convention
    // so the chunk's isError flag matches the Rust ToolCallComplete signal.
    const isError = result.startsWith('Error:') || result.startsWith('Denied by human:');
    return { rawResponse: { toolResult: { name, isError, result } } };
}
