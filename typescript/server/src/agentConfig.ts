/**
 * SMOODEV-590 — Per-agent configuration (TypeScript server port).
 *
 * The server resolves each conversation's `agentId` into an {@link AgentConfig}
 * (the agent's own `instructions`, `conversationWorkflow`, optional greeting /
 * personality / tool allow-list), and folds it into the system prompt for that
 * agent's turns — so two agents in the same org behave differently instead of all
 * using one generic org persona.
 *
 * The delivery seam is an {@link AgentConfigResolver}, mirroring the server's other
 * pluggable seams ({@link AuthVerifier}, {@link AccessKnowledge}): the reference
 * ships an in-memory / no-op resolver; a real deployment plugs in one backed by the
 * monorepo `agents` table. The `create_conversation_session` payload carries only an
 * `agentId` (per the spec), so config is resolved server-side by that id, never from
 * the wire frame.
 */
import { parseWorkflow, renderWorkflowPromptSection, type ConversationWorkflow } from './workflow.js';

/**
 * The per-agent config that shapes an agent's conversations. Every field is
 * optional — an agent may set only `instructions`, only a `conversationWorkflow`,
 * or nothing (in which case the server falls back to its base/org prompt).
 */
export interface AgentConfig {
    /** Freeform system-prompt body for this agent (`agents.instructions.prompt`). */
    instructions?: string;
    /** Structured guided-agency workflow (`agents.conversation_workflow`). */
    conversationWorkflow?: ConversationWorkflow;
    /** Optional greeting to weave into the agent's first reply. */
    greeting?: string;
    /** Optional short personality descriptor folded into the persona section. */
    personality?: string;
    /** Optional allow-list of tool names this agent may use; when set, the server's
     *  tool set is filtered to it. Empty / undefined → all server tools available. */
    allowedTools?: string[];
}

/**
 * Resolves an `agentId` into its {@link AgentConfig}. `undefined` (agent unknown /
 * no per-agent config) → the server uses its base/org default prompt + tools, so
 * behavior is unchanged for un-configured agents.
 */
export interface AgentConfigResolver {
    resolve(agentId: string): Promise<AgentConfig | undefined> | AgentConfig | undefined;
}

/** A resolver backed by a fixed in-memory `agentId → AgentConfig` map. The reference
 *  implementation for tests / local use; a real deployment reads the `agents` table. */
export class StaticAgentConfigResolver implements AgentConfigResolver {
    private readonly byId: Map<string, AgentConfig>;

    constructor(configs: Record<string, AgentConfig> = {}) {
        this.byId = new Map(Object.entries(configs));
    }

    resolve(agentId: string): AgentConfig | undefined {
        return this.byId.get(agentId);
    }
}

/**
 * Tolerantly parse a raw agent record (the shape stored in the monorepo `agents`
 * table: `instructions` jsonb `{prompt}`, `conversation_workflow` jsonb, etc.) into
 * an {@link AgentConfig}. Malformed sub-fields are dropped individually — a broken
 * `conversation_workflow` doesn't discard a valid `instructions.prompt` — and the
 * function never throws, so a bad record degrades gracefully. Returns `undefined`
 * only when nothing usable is present.
 */
export function parseAgentConfig(raw: unknown): AgentConfig | undefined {
    if (typeof raw !== 'object' || raw === null || Array.isArray(raw)) return undefined;
    const obj = raw as Record<string, unknown>;
    const config: AgentConfig = {};

    // instructions: either the jsonb `{ prompt: string }` or a bare string.
    const instr = obj.instructions;
    if (typeof instr === 'string' && instr.trim().length > 0) {
        config.instructions = instr;
    } else if (typeof instr === 'object' && instr !== null) {
        const prompt = (instr as Record<string, unknown>).prompt;
        if (typeof prompt === 'string' && prompt.trim().length > 0) config.instructions = prompt;
    }

    // conversation_workflow (snake) / conversationWorkflow (camel) — tolerant parse.
    const workflow = parseWorkflow(obj.conversation_workflow ?? obj.conversationWorkflow);
    if (workflow) config.conversationWorkflow = workflow;

    if (typeof obj.greeting === 'string' && obj.greeting.trim().length > 0) config.greeting = obj.greeting;
    if (typeof obj.personality === 'string' && obj.personality.trim().length > 0) config.personality = obj.personality;

    const tools = obj.tool_config ?? obj.allowedTools;
    if (Array.isArray(tools)) {
        const names = tools.filter((t): t is string => typeof t === 'string' && t.length > 0);
        if (names.length > 0) config.allowedTools = names;
    }

    return Object.keys(config).length > 0 ? config : undefined;
}

/**
 * Assemble the effective system prompt for a turn from the server's base prompt, the
 * per-agent config, and the conversation's current workflow step.
 *
 * When `config` is undefined or empty this returns `base` unchanged (behavior is
 * identical to before per-agent config existed). Otherwise the agent's own
 * `instructions` become the primary body, augmented by the base prompt's grounding
 * rules, plus optional personality / greeting sections and the rendered workflow
 * step.
 */
export function assembleSystemPrompt(base: string, config: AgentConfig | undefined, currentStepId: string | null | undefined): string {
    if (!config) return base;

    const sections: string[] = [];

    if (config.personality) sections.push(`<Personality>\n${config.personality}\n</Personality>`);

    // The agent's own instructions are the primary persona; the base prompt's
    // grounding / behavior rules follow so they always apply.
    if (config.instructions) {
        sections.push(`<AgentInstructions>\n${config.instructions}\n</AgentInstructions>`);
        sections.push(base);
    } else {
        sections.push(base);
    }

    if (config.greeting) {
        sections.push(
            `<GreetingAwareness>\nIf this is your first reply in the conversation, open with a natural, brief variant of: "${config.greeting}" — then address the user's message. Do not repeat it verbatim on later turns.\n</GreetingAwareness>`,
        );
    }

    const workflowSection = renderWorkflowPromptSection(config.conversationWorkflow, currentStepId);
    if (workflowSection) sections.push(workflowSection);

    return sections.join('\n\n');
}
