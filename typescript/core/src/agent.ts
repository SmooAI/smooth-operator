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

import type { CheckpointStore } from './checkpoint.js';
import type { Memory } from './memory.js';
import type { Reranker } from './rerank.js';
import { compact } from './compaction.js';
import { CostTracker } from './cost.js';
import type { CostBudget, ModelPricing, Usage } from './cost.js';
import type { InMemoryKnowledge } from './knowledge.js';

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
    knowledge?: InMemoryKnowledge;
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

    private toolSpecs(): Array<Record<string, unknown>> | undefined {
        const tools = this.options.tools ?? [];
        if (tools.length === 0) return undefined;
        return tools.map((t) => ({
            type: 'function',
            function: { name: t.name, description: t.description, parameters: t.parameters },
        }));
    }

    /** Run a single turn. `history` is prior OpenAI-format messages (multi-turn). */
    async run(message: string, history?: Array<Record<string, unknown>>): Promise<AgentRunResponse> {
        const messages: Array<Record<string, unknown>> = [];
        const system = this.buildSystem(message);
        if (system) messages.push({ role: 'system', content: system });

        // Source prior conversation from the checkpoint store (if configured),
        // otherwise from the explicit `history` argument.
        const cpStore = this.options.checkpointStore;
        const cpId = this.options.conversationId;
        let prior = history;
        if (cpStore && cpId) {
            const loaded = cpStore.load(cpId);
            if (loaded) prior = loaded.messages;
        }
        if (prior) messages.push(...prior);
        messages.push({ role: 'user', content: message });

        const tools = this.toolSpecs();
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
    
                // Stop early if this turn has hit its token/cost budget.
                if (tracker.exceeds(this.options.budget)) {
                    return { text: lastText, iterations: iteration, toolCalls, usage: tracker.usage, costUsd: tracker.costUsd, budgetExceeded: true };
                }
    
                if (!choice.tool_calls || choice.tool_calls.length === 0) {
                    return { text: lastText, iterations: iteration, toolCalls, usage: tracker.usage, costUsd: tracker.costUsd, budgetExceeded: false };
                }
    
                for (const tc of choice.tool_calls) {
                    toolCalls++;
                    const result = await this.dispatchTool(tc.function.name, tc.function.arguments);
                    messages.push({ role: 'tool', tool_call_id: tc.id, content: result });
                }
            }

            return { text: lastText, iterations: maxIterations, toolCalls, usage: tracker.usage, costUsd: tracker.costUsd, budgetExceeded: false };
        } finally {
            // Persist the conversation (sans system prompt, which is rebuilt each turn).
            if (cpStore && cpId) {
                cpStore.save({ conversationId: cpId, messages: messages.filter((m) => m.role !== 'system') });
            }
        }
    }

    private async dispatchTool(name: string, rawArgs: string): Promise<string> {
        const tool = this.toolsByName.get(name);
        if (!tool) return `error: unknown tool '${name}'`;
        let args: Record<string, unknown>;
        try {
            args = rawArgs ? JSON.parse(rawArgs) : {};
        } catch {
            return `error: tool '${name}' received invalid JSON arguments`;
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
