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
/**
 * One entry of the agent's `tool_config.enabledTools` (authoritative
 * `AgentToolConfig` shape in the monorepo `agents` schema). `toolId` is snake_case
 * and matched against a registered tool's name. `authLevel` / `config` are preserved
 * even though the server doesn't act on them yet.
 */
export interface EnabledTool {
    toolId: string;
    enabled: boolean;
    authLevel: string;
    config?: Record<string, unknown>;
}

export interface AgentConfig {
    /** Freeform system-prompt body for this agent (`agents.instructions.prompt`). */
    instructions?: string;
    /** Structured guided-agency workflow (`agents.conversation_workflow`). */
    conversationWorkflow?: ConversationWorkflow;
    /** Optional greeting to weave into the agent's first reply. */
    greeting?: string;
    /** Optional short personality descriptor folded into the persona section. */
    personality?: string;
    /**
     * Parsed `tool_config.enabledTools`. When present and non-empty, the server's
     * tool set is restricted to entries with `enabled: true` matched by snake_case
     * `toolId`. Empty / undefined → all registered tools available (unchanged).
     */
    enabledTools?: EnabledTool[];
    /**
     * Agent visibility (`agents.visibility`). `internal` agents auto-satisfy
     * `end_user`/`admin` tool auth; `public` (default) agents block `admin` tools and
     * require identity verification for `end_user` tools. Drives {@link gateTools}.
     */
    visibility?: 'public' | 'internal';
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
    if (obj.visibility === 'public' || obj.visibility === 'internal') config.visibility = obj.visibility;

    // tool_config.enabledTools — authoritative AgentToolConfig shape. Defaults to []
    // on every agent row; a non-empty list restricts tools at dispatch time. Malformed
    // entries are skipped individually; `enabled` defaults true, `authLevel` "none".
    const toolConfig = obj.tool_config;
    if (typeof toolConfig === 'object' && toolConfig !== null && !Array.isArray(toolConfig)) {
        const list = (toolConfig as Record<string, unknown>).enabledTools;
        if (Array.isArray(list)) {
            const enabledTools: EnabledTool[] = [];
            for (const raw of list) {
                if (typeof raw !== 'object' || raw === null) continue;
                const t = raw as Record<string, unknown>;
                if (typeof t.toolId !== 'string' || t.toolId.length === 0) continue;
                enabledTools.push({
                    toolId: t.toolId,
                    enabled: typeof t.enabled === 'boolean' ? t.enabled : true,
                    authLevel: typeof t.authLevel === 'string' ? t.authLevel : 'none',
                    config: typeof t.config === 'object' && t.config !== null && !Array.isArray(t.config) ? (t.config as Record<string, unknown>) : undefined,
                });
            }
            if (enabledTools.length > 0) config.enabledTools = enabledTools;
        }
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
export function assembleSystemPrompt(base: string, config: AgentConfig | undefined, currentStepId: string | null | undefined, isFirstTurn: boolean): string {
    const parts = agentPromptParts(base, config, currentStepId, isFirstTurn);
    return [parts.base, parts.greeting, parts.workflow].filter((p): p is string => p !== undefined).join('\n\n');
}

/**
 * The per-agent prompt split into the {@link PromptSections} a `PromptComposer`
 * receives (SMOODEV-3798): the base body (personality, the agent's instructions,
 * then the server prompt), the first-turn greeting and the current workflow step.
 * Joined in that order they equal {@link assembleSystemPrompt}.
 */
export function agentPromptParts(
    base: string,
    config: AgentConfig | undefined,
    currentStepId: string | null | undefined,
    isFirstTurn: boolean,
): { base: string; greeting?: string; workflow?: string } {
    if (!config) return { base };

    const baseParts: string[] = [];

    if (config.personality) baseParts.push(`<Personality>\n${config.personality}\n</Personality>`);

    // The agent's own instructions are the primary persona; the base prompt's
    // grounding / behavior rules follow so they always apply.
    if (config.instructions) {
        baseParts.push(`<AgentInstructions>\n${config.instructions}\n</AgentInstructions>`);
    }
    baseParts.push(base);

    // Greeting is gated server-side to the FIRST turn only (mirrors the Python
    // server's `is_first_turn`): the section is dropped entirely on later turns so
    // the agent doesn't re-greet.
    const greeting =
        isFirstTurn && config.greeting
            ? `<GreetingAwareness>\nThis is your first reply in the conversation. Open with a natural, brief variant of: "${config.greeting}" — then address the user's message in the same reply. Do NOT repeat the greeting verbatim, and do not reintroduce yourself later.\n</GreetingAwareness>`
            : undefined;

    const workflow = renderWorkflowPromptSection(config.conversationWorkflow, currentStepId) || undefined;

    return { base: baseParts.join('\n\n'), greeting, workflow };
}
