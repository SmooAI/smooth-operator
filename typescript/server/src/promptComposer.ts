/**
 * Prompt composition seam (SMOODEV-3798) — parity with the Rust reference's
 * `smooth_operator_server::prompt_composer`.
 *
 * A {@link PromptComposer} assembles the turn's system prompt from its ordered
 * sections plus who is asking and which agent answers. The dispatcher drops
 * blank sections and joins the rest with a blank line ({@link joinSections}).
 * With no composer installed it uses {@link DefaultPromptComposer}, which keeps
 * this server's historical order: base (personality, agent instructions, server
 * prompt), first-turn greeting, workflow step, then the invoked skill.
 */
import type { AccessContext } from './auth.js';
import type { AgentConfig } from './agentConfig.js';

/** Where {@link PromptSections.base} came from. */
export type BaseSource = 'agent' | 'orgPersona' | 'defaultPersona' | 'builtIn';

/** Every input to one turn's system prompt. `undefined` ⇒ the section does not apply. */
export interface PromptSections {
    base: string;
    baseSource: BaseSource;
    greeting?: string;
    workflow?: string;
    skill?: string;
    suggestedReplies: string;
    agent?: AgentConfig;
    access?: AccessContext;
    sessionAuthenticated: boolean;
    conversationId: string;
}

/** Assembles the turn's system prompt. Return the sections in order. */
export interface PromptComposer {
    compose(sections: PromptSections): string[];
}

/** The historical order: base, greeting, workflow, skill, suggested replies. */
export const DefaultPromptComposer: PromptComposer & { sections(s: PromptSections): string[] } = {
    sections(s: PromptSections): string[] {
        const out = [s.base];
        for (const part of [s.greeting, s.workflow, s.skill]) {
            if (part !== undefined) out.push(part);
        }
        out.push(s.suggestedReplies);
        return out;
    },
    compose(s: PromptSections): string[] {
        return this.sections(s);
    },
};

/** Drop blank sections and join the rest with a blank line. */
export function joinSections(sections: string[]): string {
    return sections.filter((s) => s.trim().length > 0).join('\n\n');
}

/** Compose `sections` with `composer` (or the default) and join. */
export function renderPrompt(composer: PromptComposer | undefined, sections: PromptSections): string {
    return joinSections((composer ?? DefaultPromptComposer).compose(sections));
}
