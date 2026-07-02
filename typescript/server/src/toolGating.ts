/**
 * SMOODEV-590 — per-tool authLevel enforcement + per-tool config delivery.
 *
 * Mirrors the monorepo general-agent tool-execution gate
 * (`packages/backend/src/ai/graphs/general-agent/nodes/tool-execution.ts`): at
 * execution time, a tool's `authLevel` (from the agent's `tool_config.enabledTools`)
 * combined with the agent's `visibility` decides whether the tool runs — and its
 * per-tool `config` is delivered to `execute`.
 *
 * Implemented by wrapping each tool's `execute` (no engine fork): a blocked call
 * returns the reference tool message instead of running, exactly as the model would
 * see it.
 */
import type { Tool } from '@smooai/smooth-operator-core';

import type { AgentConfig, EnabledTool } from './agentConfig.js';

/**
 * A registered tool that can opt into auth-requirement gating and receive its
 * per-agent config. Both extensions are optional so a plain core {@link Tool} is a
 * valid `ServerTool` — `supportsAuthRequirement` defaults false (never gated,
 * faithful to the reference), and `execute`'s second arg is ignored by tools that
 * don't read config.
 */
export interface ServerTool extends Tool {
    /** When true, the tool participates in authLevel gating. Default false → never gated. */
    supportsAuthRequirement?: boolean;
    execute(args: Record<string, unknown>, config?: Record<string, unknown>): Promise<string>;
}

/**
 * Resolves whether a conversation's session is identity-verified (OTP / auth session).
 * The server ships no implementation — a host wires session state behind this seam.
 * **Fail-closed**: an absent authenticator means "not authenticated".
 */
export interface SessionAuthenticator {
    isAuthenticated(conversationId: string): Promise<boolean> | boolean;
}

/**
 * Wrap `tools` with authLevel enforcement + per-tool config delivery for `config`.
 *
 * Gating applies to a tool ONLY when its `enabledTools` entry sets
 * `authLevel != 'none'` AND the tool declares `supportsAuthRequirement`. Then:
 *   - `admin` on a `public` agent → blocked (admin tools are internal-only);
 *   - `internal` visibility → auto-satisfied (the session is already authenticated);
 *   - `public` + `end_user` → consult {@link SessionAuthenticator} (fail-closed);
 *     not authenticated → blocked with an identity-verification message.
 * A blocked tool returns the reference message; otherwise it runs, receiving its
 * per-tool `config` as `execute`'s second argument.
 */
export function gateTools(tools: Tool[], config: AgentConfig | undefined, conversationId: string, authenticator: SessionAuthenticator | undefined): Tool[] {
    const byId = new Map<string, EnabledTool>((config?.enabledTools ?? []).map((e) => [e.toolId, e]));
    const visibility = config?.visibility ?? 'public';

    return tools.map((tool) => {
        const entry = byId.get(tool.name);
        const authLevel = entry?.authLevel ?? 'none';
        const gated = authLevel !== 'none' && (tool as ServerTool).supportsAuthRequirement === true;
        const toolConfig = entry?.config;
        // Nothing to enforce and no config to deliver → pass the tool through untouched.
        if (!gated && toolConfig === undefined) return tool;

        const original = tool as ServerTool;
        return {
            ...tool,
            async execute(args: Record<string, unknown>): Promise<string> {
                if (gated) {
                    if (authLevel === 'admin' && visibility === 'public') {
                        return `Tool '${tool.name}' requires admin authentication and is not available on public-facing agents.`;
                    }
                    if (visibility !== 'internal') {
                        const authed = authenticator ? await authenticator.isAuthenticated(conversationId) : false;
                        if (!authed) {
                            return `I need to verify your identity before I can use ${tool.name}. Please verify with a one-time code.`;
                        }
                    }
                }
                return original.execute(args, toolConfig);
            },
        };
    });
}
