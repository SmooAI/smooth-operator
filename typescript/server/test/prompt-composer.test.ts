/**
 * PromptComposer seam (SMOODEV-3798) — parity with the Rust reference's
 * `prompt_composer` tests: the default order, blank sections dropped, a host
 * composer that appends last and sees the context, a reorder, and that the
 * default composition of the agent parts equals `assembleSystemPrompt`.
 */
import { describe, expect, it } from 'vitest';
import { agentPromptParts, assembleSystemPrompt, type AgentConfig } from '../src/agentConfig.js';
import { DefaultPromptComposer, joinSections, renderPrompt, type PromptComposer, type PromptSections } from '../src/promptComposer.js';

function sections(over: Partial<PromptSections> = {}): PromptSections {
    return {
        base: 'BASE',
        baseSource: 'agent',
        greeting: 'GREETING',
        workflow: 'WORKFLOW',
        skill: 'SKILL',
        suggestedReplies: 'TRAILER',
        sessionAuthenticated: false,
        conversationId: 'c1',
        ...over,
    };
}

describe('PromptComposer', () => {
    it('default composer keeps the historical order', () => {
        expect(renderPrompt(undefined, sections())).toBe('BASE\n\nGREETING\n\nWORKFLOW\n\nSKILL\n\nTRAILER');
    });

    it('absent sections are skipped', () => {
        expect(renderPrompt(undefined, sections({ greeting: undefined, workflow: undefined, skill: undefined }))).toBe('BASE\n\nTRAILER');
    });

    it('join drops blank sections', () => {
        expect(joinSections(['a', '', '  ', 'b'])).toBe('a\n\nb');
    });

    it('a host composer appends last and sees the context', () => {
        const safety: PromptComposer = {
            compose: (s) => [...DefaultPromptComposer.sections(s), `SAFETY(source=${s.baseSource}, authed=${s.sessionAuthenticated}, conv=${s.conversationId})`],
        };
        const out = renderPrompt(safety, sections());
        expect(out.startsWith('BASE\n\nGREETING')).toBe(true);
        expect(out.endsWith('TRAILER\n\nSAFETY(source=agent, authed=false, conv=c1)')).toBe(true);
    });

    it('a host composer may reorder', () => {
        const reorder: PromptComposer = { compose: (s) => [s.base, s.workflow ?? '', s.suggestedReplies] };
        expect(renderPrompt(reorder, sections())).toBe('BASE\n\nWORKFLOW\n\nTRAILER');
    });

    it('the default composition of the agent parts equals assembleSystemPrompt', () => {
        const config: AgentConfig = { instructions: 'Be Ada.', personality: 'Warm.', greeting: 'Hi!' } as AgentConfig;
        for (const isFirstTurn of [true, false]) {
            const parts = agentPromptParts('SERVER', config, undefined, isFirstTurn);
            const composed = renderPrompt(undefined, {
                base: parts.base,
                baseSource: 'agent',
                greeting: parts.greeting,
                workflow: parts.workflow,
                suggestedReplies: '',
                sessionAuthenticated: false,
                conversationId: 'c1',
            });
            expect(composed).toBe(assembleSystemPrompt('SERVER', config, undefined, isFirstTurn));
        }
        expect(agentPromptParts('SERVER', undefined, undefined, true)).toEqual({ base: 'SERVER' });
    });
});
