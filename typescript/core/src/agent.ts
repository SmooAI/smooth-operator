/**
 * The TypeScript smooth-operator core: a native agentic loop.
 *
 * Phase-0 sibling of the C# `SmoothAgent` (`dotnet/core`), the Python core
 * (`python/core`), and the Rust reference engine. Drives an agentic tool-calling
 * loop over any OpenAI-compatible chat client (the `openai` SDK pointed at a
 * gateway): inject retrieved knowledge, call the model, run any requested tools,
 * feed results back, and loop until the model answers without a tool call or the
 * iteration budget is hit.
 *
 * Deliberately minimal (no compaction / budget / checkpointing yet) — those layer
 * on exactly as they did when the C# core grew past Phase 0.
 */

import type { Clearance } from './cast.js';
import type { CheckpointStore } from './checkpoint.js';
import type { SmoothAgentThread } from './thread.js';
import type { Memory } from './memory.js';
import type { Reranker } from './rerank.js';
import { compact } from './compaction.js';
import { CostTracker } from './cost.js';
import type { CostBudget, ModelPricing, Usage } from './cost.js';
import type { HumanGate } from './humanGate.js';
import { isApproved } from './humanGate.js';
import type { Knowledge } from './knowledge.js';
import { ToolSearch } from './toolSearch.js';

/** A callable tool the agent may invoke. Mirrors the reference engines' tool seam. */
export interface Tool {
    name: string;
    description: string;
    /** JSON Schema for the tool's arguments. */
    parameters: Record<string, unknown>;
    execute(args: Record<string, unknown>): Promise<string>;
}

export interface AgentOptions {
    instructions?: string;
    model?: string;
    maxIterations?: number;
    maxTokens?: number;
    temperature?: number;
    knowledge?: Knowledge;
    knowledgeTopK?: number;
    /** Reranker applied to retrieved hits before injection (default: passthrough). */
    reranker?: Reranker;
    /** Candidate pool size to retrieve before reranking; when > knowledgeTopK, more docs are fetched, reranked, then trimmed. */
    knowledgeCandidateK?: number;
    /** Optional long-term memory; relevant entries are recalled into context each turn. */
    memory?: Memory;
    /** How many memory entries to recall per turn (default 4). */
    memoryTopK?: number;
    tools?: Tool[];
    /**
     * Deferred tools — registered but with their schemas HIDDEN from the model.
     * When any are present, a built-in `tool_search` meta-tool is advertised in
     * their place; the model calls it to fuzzy-match and promote the ones it needs,
     * which then become visible + dispatchable on subsequent turns. Keeps the tool
     * schema payload small when there are many rarely-used tools. An unpromoted
     * deferred tool is NOT dispatchable.
     */
    deferredTools?: Tool[];
    /**
     * Approximate token budget for the context window. Before each model call,
     * older non-system messages are dropped (sliding window) to stay under it.
     * `0` disables compaction. Defaults to 8000.
     */
    maxContextTokens?: number;
    /** Optional ceiling for the turn (token and/or USD). The turn stops early once a model call pushes usage/cost over the budget. */
    budget?: CostBudget;
    /** Per-model pricing override for cost accounting (defaults to DEFAULT_PRICING). */
    pricing?: Record<string, ModelPricing>;
    /** Optional store for persisting/resuming the conversation. Used with `conversationId`. */
    checkpointStore?: CheckpointStore;
    /** Conversation id for the checkpoint store (required to use checkpointing). */
    conversationId?: string;
    /**
     * Optional tool-access policy. When set, a tool the clearance forbids is not
     * dispatched — a "tool not permitted" result is returned to the model instead.
     * Undefined allows every tool (the prior behaviour).
     */
    clearance?: Clearance;
    /**
     * Optional human-in-the-loop gate. When set, the agent asks it for approval before
     * running any tool call for which {@link requiresApproval} returns true. A denied call
     * is not executed; the model is told it was denied and can adapt.
     */
    humanGate?: HumanGate;
    /**
     * Which tool calls need human approval (e.g. writes / destructive actions), given the
     * tool name and parsed arguments. Default: none. Only consulted when `humanGate` is set.
     * Example: `requiresApproval: (name) => name === 'delete_record' || name === 'send_email'`.
     */
    requiresApproval?: (name: string, args: Record<string, unknown>) => boolean;
}

export interface AgentRunResponse {
    text: string;
    iterations: number;
    toolCalls: number;
    usage: Usage;
    costUsd: number;
    /** True if the turn stopped because the cost/token budget was hit. */
    budgetExceeded: boolean;
}

/**
 * The minimal shape of the OpenAI-compatible client the agent needs. The real
 * `openai` SDK's `OpenAI` satisfies this; tests inject a fake.
 */
export interface ChatClientLike {
    chat: {
        completions: {
            create(body: Record<string, unknown>): Promise<{
                choices: Array<{
                    message: {
                        content: string | null;
                        tool_calls?: Array<{ id: string; function: { name: string; arguments: string } }> | null;
                    };
                }>;
                usage?: { prompt_tokens?: number | null; completion_tokens?: number | null } | null;
            }>;
        };
    };
}

const DEFAULTS = {
    model: 'claude-haiku-4-5',
    maxIterations: 8,
    maxTokens: 512,
    temperature: 0,
    knowledgeTopK: 4,
    maxContextTokens: 8000,
};

/** Pull token usage from an OpenAI-shaped response, defaulting to zero when absent. */
function extractUsage(usage: { prompt_tokens?: number | null; completion_tokens?: number | null } | null | undefined): Usage {
    return { promptTokens: usage?.prompt_tokens ?? 0, completionTokens: usage?.completion_tokens ?? 0 };
}

export class SmoothAgent {
    private readonly toolsByName: Map<string, Tool>;

    constructor(
        private readonly client: ChatClientLike,
        private readonly options: AgentOptions = {},
    ) {
        if (!client) throw new Error('client is required');
        this.toolsByName = new Map((options.tools ?? []).map((t) => [t.name, t]));
    }

    private buildSystem(message: string): string {
        let system = this.options.instructions ?? '';

        const mem = this.options.memory;
        if (mem) {
            const recalled = mem.recall(message, this.options.memoryTopK ?? 4);
            if (recalled.length > 0) {
                const block = recalled.map((e) => `- ${e.text}`).join('\n');
                system = `${system}\n\nRelevant memory (things you remember about this user/context):\n${block}`.trim();
            }
        }

        const kb = this.options.knowledge;
        if (kb) {
            const topK = this.options.knowledgeTopK ?? DEFAULTS.knowledgeTopK;
            const candidateK = Math.max(this.options.knowledgeCandidateK ?? 0, topK);
            let hits = kb.query(message, candidateK);
            if (this.options.reranker) hits = this.options.reranker.rerank(message, hits);
            hits = hits.slice(0, topK);
            if (hits.length > 0) {
                const block = hits.map((h) => `[${h.source}] ${h.content}`).join('\n\n');
                system = `${system}\n\nKnowledge base (ground all facts ONLY in this; if it is not here, say you don't know):\n${block}`.trim();
            }
        }
        return system;
    }

    private toolSpecs(search?: ToolSearch): Array<Record<string, unknown>> | undefined {
        // Eager (always-visible) tools, plus — when deferred tools exist — the
        // built-in `tool_search` meta-tool and any deferred tools promoted so far
        // this run. Deferred-but-unpromoted tools are deliberately omitted so the
        // model never sees their schemas until it searches for them.
        const visible: Tool[] = [...(this.options.tools ?? [])];
        if (search?.hasDeferred()) {
            visible.push(search);
            visible.push(...search.promotedTools());
        }
        if (visible.length === 0) return undefined;
        return visible.map((t) => ({
            type: 'function',
            function: { name: t.name, description: t.description, parameters: t.parameters },
        }));
    }

    /**
     * Run a single turn.
     *
     * `history` is prior OpenAI-format messages (multi-turn). `thread`, when given,
     * is a {@link SmoothAgentThread} carrying the conversation across runs: the turn
     * is seeded from the thread's messages, and this turn's new user + assistant
     * (+ tool) messages are appended back to it before returning. The thread takes
     * precedence over `history` as the prior context.
     */
    async run(message: string, history?: Array<Record<string, unknown>>, thread?: SmoothAgentThread): Promise<AgentRunResponse> {
        const messages: Array<Record<string, unknown>> = [];
        const system = this.buildSystem(message);
        if (system) messages.push({ role: 'system', content: system });

        // Source prior conversation: the thread (if passed) wins, then the checkpoint
        // store (if configured), then the explicit `history` argument.
        const cpStore = this.options.checkpointStore;
        const cpId = this.options.conversationId;
        let prior = history;
        if (cpStore && cpId) {
            const loaded = cpStore.load(cpId);
            if (loaded) prior = loaded.messages;
        }
        if (thread) prior = [...thread.messages];
        if (prior) messages.push(...prior);
        const userMsg: Record<string, unknown> = { role: 'user', content: message };
        messages.push(userMsg);

        // Track this turn's new messages by identity so they can be appended back to
        // the thread on exit. Index slicing would be unsafe — compaction may drop or
        // reorder `messages` mid-turn.
        const turnMessages: Array<Record<string, unknown>> = [userMsg];

        // Per-run promotion state for deferred tools (undefined when none registered).
        const search = this.options.deferredTools && this.options.deferredTools.length > 0 ? new ToolSearch(this.options.deferredTools) : undefined;
        const maxIterations = this.options.maxIterations ?? DEFAULTS.maxIterations;
        let toolCalls = 0;
        let lastText = '';

        const maxContextTokens = this.options.maxContextTokens ?? DEFAULTS.maxContextTokens;
        const model = this.options.model ?? DEFAULTS.model;
        const tracker = new CostTracker();
        try {
            for (let iteration = 1; iteration <= maxIterations; iteration++) {
                // Keep the context window within budget before each model call.
                messages.splice(0, messages.length, ...compact(messages, maxContextTokens));
                // Recompute tool specs each iteration: a `tool_search` call in the
                // previous iteration may have promoted deferred tools into view.
                const tools = this.toolSpecs(search);
                const response = await this.client.chat.completions.create({
                    model,
                    messages,
                    ...(tools ? { tools } : {}),
                    temperature: this.options.temperature ?? DEFAULTS.temperature,
                    max_tokens: this.options.maxTokens ?? DEFAULTS.maxTokens,
                });
                tracker.record(model, extractUsage(response.usage), this.options.pricing);
                const choice = response.choices[0].message;
                lastText = choice.content ?? '';
    
                const assistantMsg: Record<string, unknown> = { role: 'assistant', content: choice.content ?? '' };
                if (choice.tool_calls && choice.tool_calls.length > 0) {
                    assistantMsg.tool_calls = choice.tool_calls.map((tc) => ({
                        id: tc.id,
                        type: 'function',
                        function: { name: tc.function.name, arguments: tc.function.arguments },
                    }));
                }
                messages.push(assistantMsg);
                turnMessages.push(assistantMsg);

                // Stop early if this turn has hit its token/cost budget.
                if (tracker.exceeds(this.options.budget)) {
                    return { text: lastText, iterations: iteration, toolCalls, usage: tracker.usage, costUsd: tracker.costUsd, budgetExceeded: true };
                }
    
                if (!choice.tool_calls || choice.tool_calls.length === 0) {
                    return { text: lastText, iterations: iteration, toolCalls, usage: tracker.usage, costUsd: tracker.costUsd, budgetExceeded: false };
                }
    
                for (const tc of choice.tool_calls) {
                    toolCalls++;
                    const result = await this.dispatchTool(tc.function.name, tc.function.arguments, search);
                    const toolMsg: Record<string, unknown> = { role: 'tool', tool_call_id: tc.id, content: result };
                    messages.push(toolMsg);
                    turnMessages.push(toolMsg);
                }
            }

            return { text: lastText, iterations: maxIterations, toolCalls, usage: tracker.usage, costUsd: tracker.costUsd, budgetExceeded: false };
        } finally {
            // Persist the conversation (sans system prompt, which is rebuilt each turn).
            if (cpStore && cpId) {
                cpStore.save({ conversationId: cpId, messages: messages.filter((m) => m.role !== 'system') });
            }
            // Append this turn's new messages (user + assistant + tool, never system)
            // back to the thread so the next run sees the full conversation.
            if (thread) thread.extend(turnMessages);
        }
    }

    private async dispatchTool(name: string, rawArgs: string, search?: ToolSearch): Promise<string> {
        // Enforce the role's tool clearance before dispatch: a forbidden tool is
        // never executed — the model is told it isn't permitted, mirroring how the
        // loop surfaces other tool errors.
        const clearance = this.options.clearance;
        if (clearance && !clearance.isAllowed(name)) {
            return `error: tool '${name}' is not permitted for this role`;
        }

        // Resolve the tool: eager tools first, then the built-in `tool_search`
        // meta-tool, then deferred tools that have been promoted. An unpromoted
        // deferred tool resolves to nothing — it's invisible until searched for.
        let tool = this.toolsByName.get(name);
        if (!tool && search) {
            tool = name === search.name ? search : search.toolByName(name);
        }
        if (!tool) return `error: unknown tool '${name}'`;
        let args: Record<string, unknown>;
        try {
            args = rawArgs ? JSON.parse(rawArgs) : {};
        } catch {
            return `error: tool '${name}' received invalid JSON arguments`;
        }

        // Human-in-the-loop: pause for approval before running a flagged (write/sensitive)
        // tool. A denial is fed back to the model as a result — the tool never runs.
        const gate = this.options.humanGate;
        if (gate && this.options.requiresApproval?.(name, args)) {
            const decision = await gate({ toolName: name, arguments: args, prompt: `Approve calling tool '${name}'?` });
            if (!isApproved(decision)) {
                return `Denied by human: ${decision.reason ?? 'no reason given'}`;
            }
        }

        try {
            return await tool.execute(args);
        } catch (err) {
            // Surface tool failures to the model, don't crash the turn.
            return `error: tool '${name}' failed: ${err instanceof Error ? err.message : String(err)}`;
        }
    }
}

/**
 * Build a {@link Tool} that delegates a subtask to a child {@link SmoothAgent}.
 *
 * A sub-agent is just a tool backed by another agent: the model calls this tool
 * with a `task` argument, the child agent runs that task, and the child's final
 * reply becomes the tool result — composing with the existing tool loop, no special
 * wiring. The child can have its own instructions, tools, knowledge, etc.
 */
export function delegateTool(name: string, description: string, child: SmoothAgent, taskProperty = 'task'): Tool {
    return {
        name,
        description,
        parameters: {
            type: 'object',
            properties: { [taskProperty]: { type: 'string', description: 'The subtask for the sub-agent to perform.' } },
            required: [taskProperty],
        },
        async execute(args: Record<string, unknown>): Promise<string> {
            const task = String(args[taskProperty] ?? '');
            const result = await child.run(task);
            return result.text;
        },
    };
}
