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
    tools?: Tool[];
}

export interface AgentRunResponse {
    text: string;
    iterations: number;
    toolCalls: number;
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
};

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
        const kb = this.options.knowledge;
        if (kb) {
            const hits = kb.query(message, this.options.knowledgeTopK ?? DEFAULTS.knowledgeTopK);
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
        if (history) messages.push(...history);
        messages.push({ role: 'user', content: message });

        const tools = this.toolSpecs();
        const maxIterations = this.options.maxIterations ?? DEFAULTS.maxIterations;
        let toolCalls = 0;
        let lastText = '';

        for (let iteration = 1; iteration <= maxIterations; iteration++) {
            const response = await this.client.chat.completions.create({
                model: this.options.model ?? DEFAULTS.model,
                messages,
                ...(tools ? { tools } : {}),
                temperature: this.options.temperature ?? DEFAULTS.temperature,
                max_tokens: this.options.maxTokens ?? DEFAULTS.maxTokens,
            });
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

            if (!choice.tool_calls || choice.tool_calls.length === 0) {
                return { text: lastText, iterations: iteration, toolCalls };
            }

            for (const tc of choice.tool_calls) {
                toolCalls++;
                const result = await this.dispatchTool(tc.function.name, tc.function.arguments);
                messages.push({ role: 'tool', tool_call_id: tc.id, content: result });
            }
        }

        return { text: lastText, iterations: maxIterations, toolCalls };
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
