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
import type { AgentOptions, ChatClientLike, HumanApprovalRequest, HumanApprovalResponse, Knowledge, StreamEvent, Tool, ToolHook } from '@smooai/smooth-operator-core';

import type { ConfirmationRegistry } from './confirmation.js';
import type { ModelCeilingResolver } from './modelCeiling.js';
import * as protocol from './protocol.js';
import type { Citation, Frame } from './protocol.js';
import type { SessionStore } from './sessionStore.js';
import { advanceStep, judgeStep, resolveCurrentStep, type ConversationWorkflow } from './workflow.js';

/** What a completed turn produced (the analog of the C#/Rust `TurnResult`). */
export interface TurnResult {
    reply: string;
    messageId: string;
    citations: Citation[];
    /**
     * SMOODEV-590 — the workflow step id this conversation should resume on next
     * turn, after the post-turn judge. Present only when a `conversationWorkflow`
     * was configured; the caller persists it. Undefined for freeform agents.
     */
    nextStepId?: string;
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

/** The server/org default persona, used when neither the caller nor a per-agent
 *  config supplies a system prompt. Exported so the dispatcher assembles per-agent
 *  prompts on top of the same base. */
export const DEFAULT_SYSTEM_PROMPT =
    'You are a helpful customer support agent. Answer using only the knowledge provided to you; if it is not there, say you don\'t know.';

/** A sink the runner pushes outbound protocol frames into (single-writer downstream). */
export type Sink = (frame: Frame) => void;

/** `max_tokens` for the fast-model preamble — one short sentence. Pearl th-9a5794. */
export const PREAMBLE_MAX_TOKENS = 64;

/**
 * System prompt for the fast-model preamble (see `SMOOTH_AGENT_PREAMBLE_MODEL`).
 * One short present-tense sentence describing intent — no answer (it's generated
 * WITHOUT the tool result), no greeting, no promises. Byte-for-byte the Rust
 * reference server's `PREAMBLE_SYSTEM_PROMPT`.
 */
export const PREAMBLE_SYSTEM_PROMPT =
    'You are the assistant\'s voice while it works. '
    + 'In ONE short present-tense sentence (max ~12 words), tell the user what you\'re about to do to help with their message. '
    + 'Do NOT answer the question, do NOT greet, do NOT promise a specific result or outcome. '
    + 'Example: "Let me pull up your recent conversations." '
    + 'Reply with only that sentence — no quotes, no preamble, no markdown.';

/**
 * The fast preamble model from `SMOOTH_AGENT_PREAMBLE_MODEL`, or `undefined` when
 * unset/blank — in which case the preamble is OFF: no extra LLM call, no extra
 * event, behaviour byte-for-byte unchanged. Mirrors the Rust runner's env read.
 */
export function preambleModelFromEnv(): string | undefined {
    const model = process.env.SMOOTH_AGENT_PREAMBLE_MODEL?.trim();
    return model ? model : undefined;
}

/**
 * A one-shot mutable flag flipped on the FIRST real answer token, so a slow preamble
 * is dropped rather than popping in AFTER the reply has started streaming.
 */
export interface AnswerStartedFlag {
    started: boolean;
}

/**
 * Generate + emit the ephemeral preamble on the SAME gateway/key as the turn (only the
 * model id and token cap differ), from the user's message alone — no tool results, no
 * history. Runs concurrently with the agent loop and never gates it.
 *
 * Best-effort by construction: every failure is swallowed at debug, and the emit is
 * skipped entirely once `answerStarted.started` flips. Exported so the race + failure
 * paths are testable deterministically, without sleeping.
 */
export async function runPreamble(
    chatClient: ChatClientLike,
    model: string,
    requestId: string,
    userMessage: string,
    sink: Sink,
    answerStarted: AnswerStartedFlag,
): Promise<void> {
    try {
        const response = await chatClient.chat.completions.create({
            model,
            max_tokens: PREAMBLE_MAX_TOKENS,
            messages: [
                { role: 'system', content: PREAMBLE_SYSTEM_PROMPT },
                { role: 'user', content: userMessage },
            ],
        });
        const text = response.choices[0]?.message.content?.trim() ?? '';
        // Re-check the guard AFTER the await: the answer may have started while the
        // fast model was still thinking, and then the preamble is no longer wanted.
        if (text.length > 0 && !answerStarted.started) sink(protocol.streamPreamble(requestId, text));
    } catch (err) {
        // A failed or slow preamble must never surface to the client or fail the turn.
        console.debug('[turnRunner] preamble generation failed (ignored):', err);
    }
}

/**
 * Thrown out of {@link TurnRunner.run} when the client cancelled the turn (a `cancel`
 * action, or a disconnect). The caller swallows it: the terminal `cancelled` event was
 * already emitted by the dispatcher's cancel handler, and nothing further is emitted or
 * persisted — the partial assistant reply is DISCARDED (the user's message, persisted at
 * the start of the turn, stays).
 */
export class TurnCancelledError extends Error {
    constructor() {
        super('turn cancelled by the client');
        this.name = 'TurnCancelledError';
    }
}

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
     * Consumer-supplied tool-call surveillance {@link ToolHook}s (default none). Passed
     * straight to the engine's `toolHooks` seam, so each hook's `preCall` runs before a
     * tool executes (a throw blocks it) and its `postCall` runs after with a mutable
     * result it may redact. This is where host-supplied surveillance (Narc, redaction)
     * plugs into every turn's tool registry. Empty ⇒ behaviour unchanged.
     */
    toolHooks?: ToolHook[];
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
    /**
     * SMOODEV-590 — the agent's structured workflow (already parsed). When set, the
     * runner judges the turn after it completes and returns the advanced step id in
     * {@link TurnResult.nextStepId}. The current step must already be rendered into
     * `systemPrompt` by the caller (via `assembleSystemPrompt`).
     */
    workflow?: ConversationWorkflow;
    /** The conversation's current workflow step id (the pointer the judge advances from). */
    currentStepId?: string;
    /** The cheap model id the workflow judge uses (defaults to the workflow module default). */
    judgeModel?: string;
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
    private readonly toolHooks: ToolHook[];
    private readonly confirmTools: string[];
    private readonly confirmations?: ConfirmationRegistry;
    private readonly sessionId?: string;
    private readonly workflow?: ConversationWorkflow;
    private readonly currentStepId?: string;
    private readonly judgeModel?: string;
    private readonly model: string;
    private readonly modelCeiling?: ModelCeilingResolver;

    constructor(options: TurnRunnerOptions) {
        this.chatClient = options.chatClient;
        this.store = options.store;
        this.knowledge = options.knowledge;
        this.systemPrompt = options.systemPrompt ?? DEFAULT_SYSTEM_PROMPT;
        this.tools = options.tools ?? [];
        this.toolHooks = options.toolHooks ?? [];
        this.confirmTools = options.confirmTools ?? [];
        this.confirmations = options.confirmations;
        this.sessionId = options.sessionId;
        this.workflow = options.workflow;
        this.currentStepId = options.currentStepId;
        this.judgeModel = options.judgeModel;
        this.model = options.model ?? DEFAULT_MODEL;
        this.modelCeiling = options.modelCeiling;
    }

    /** True when `name` matches a confirmation-gated pattern (substring, like the Rust hook). */
    private isGated(name: string): boolean {
        if (!this.confirmations) return false;
        return this.confirmTools.some((pattern) => name.includes(pattern));
    }

    /**
     * Run the turn, streaming events to `sink`.
     *
     * - `signal` (the server-wide DRAIN signal), when aborted, cooperatively stops
     *   streaming further events — the turn still persists its reply and returns, so a
     *   SIGTERM drain completes the in-flight turn (unchanged behaviour).
     * - `cancelSignal` (this turn's OWN signal, aborted by a client `cancel` action or a
     *   disconnect) throws {@link TurnCancelledError} instead: nothing more is emitted
     *   and the assistant reply is never persisted.
     *
     * ponytail: cancellation is COOPERATIVE — JS can't drop an in-flight `await` the way
     * Rust drops a future, and neither `@smooai/smooth-operator-core` nor the `Tool`
     * interface takes an `AbortSignal`. So a turn parked inside a long tool call stops at
     * the next stream event rather than instantly; the observable protocol contract
     * (terminal `cancelled`, no `eventual_response`, no persisted reply) holds either way.
     * Upgrade path: thread an `AbortSignal` through the engine's `runStream` + `Tool.execute`.
     */
    async run(conversationId: string, requestId: string, userMessage: string, sink: Sink, signal?: AbortSignal, cancelSignal?: AbortSignal): Promise<TurnResult> {
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
        // Thread consumer-supplied surveillance hooks into the engine's per-turn tool
        // registry. Empty ⇒ unset ⇒ behaviour unchanged.
        if (this.toolHooks.length > 0) agentOptions.toolHooks = this.toolHooks;

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

        // Optional fast-model preamble, fired in PARALLEL with the agent loop. Off unless
        // `SMOOTH_AGENT_PREAMBLE_MODEL` is set → no extra call, no extra event. Floating by
        // design (it must never delay or gate the real turn) but self-contained: `runPreamble`
        // swallows its own failures, so there is no unhandled rejection to leak.
        // Pearl th-9a5794.
        const answerStarted: AnswerStartedFlag = { started: false };
        const preambleModel = preambleModelFromEnv();
        if (preambleModel) void runPreamble(this.chatClient, preambleModel, requestId, userMessage, sink, answerStarted);

        try {
            for await (const event of agent.runStream(userMessage, history)) {
                // Cancelled: bail BEFORE emitting this event, so nothing follows the
                // terminal `cancelled` the dispatcher already sent.
                if (cancelSignal?.aborted) throw new TurnCancelledError();
                if (signal?.aborted) break;
                // DEFER a confirmation-gated tool's toolCall chunk: it is emitted from
                // the gate AFTER `write_confirmation_required`, so the wire order matches
                // the reference (Rust) server. Non-gated tools emit their chunk inline.
                if (event.type === 'tool_call' && this.isGated(event.name)) continue;
                // Mark the answer as started BEFORE emitting, so a preamble that resolves
                // in this same tick is dropped rather than landing after the reply.
                if (event.type === 'text') answerStarted.started = true;
                this.emit(requestId, event, sink);
                if (event.type === 'text') reply += event.text;
            }
        } finally {
            // Turn over: drop any lingering pending confirmation so a stale entry can't
            // mis-route a later `confirm_tool_action` (mirrors the Rust `(cfg.clear)`
            // at turn end). No-op when HITL is off.
            this.confirmations?.clear(confirmSession);
        }

        // A cancel that landed while the stream was blocked (e.g. inside a tool call)
        // is observed here: DISCARD the partial reply — never persist it, never return.
        if (cancelSignal?.aborted) throw new TurnCancelledError();

        // 5. Persist the outbound reply.
        const outbound = await this.store.appendMessage(conversationId, 'outbound', reply);

        // 6. SMOODEV-590 — post-turn workflow judge. When the agent has a structured
        //    workflow, a cheap judge call decides whether the current step's criteria
        //    were met this turn and advances the pointer. Failure-tolerant: any judge
        //    error keeps the conversation on the current step (never freezes / skips).
        //    No-op for freeform agents (`nextStepId` stays undefined).
        let nextStepId: string | undefined;
        if (this.workflow) {
            const current = resolveCurrentStep(this.workflow, this.currentStepId);
            if (current) {
                const verdict = await judgeStep(this.chatClient, {
                    workflow: this.workflow,
                    current,
                    userMessage,
                    reply,
                    model: this.judgeModel,
                });
                nextStepId = advanceStep(this.workflow, current, verdict);
            }
        }

        return { reply, messageId: outbound.id, citations, nextStepId };
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
