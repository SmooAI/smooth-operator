/**
 * Unit tests for per-agent config parsing + prompt assembly (SMOODEV-590).
 */
import { describe, expect, it } from 'vitest';

import { assembleSystemPrompt, parseAgentConfig, StaticAgentConfigResolver, type AgentConfig } from '../src/agentConfig.js';

const BASE = 'BASE ORG PROMPT — ground answers in knowledge.';

describe('parseAgentConfig — tolerant', () => {
    it('reads instructions from the jsonb { prompt } shape', () => {
        const config = parseAgentConfig({ instructions: { prompt: 'You are Ada, a billing specialist.' } });
        expect(config?.instructions).toBe('You are Ada, a billing specialist.');
    });

    it('reads instructions from a bare string', () => {
        expect(parseAgentConfig({ instructions: 'plain string prompt' })?.instructions).toBe('plain string prompt');
    });

    it('parses conversation_workflow (snake) and conversationWorkflow (camel)', () => {
        const wf = { goal: 'g', steps: [{ id: 'a', intent: 'i', criteria: 'c' }] };
        expect(parseAgentConfig({ conversation_workflow: wf })?.conversationWorkflow?.goal).toBe('g');
        expect(parseAgentConfig({ conversationWorkflow: wf })?.conversationWorkflow?.goal).toBe('g');
    });

    it('drops a malformed workflow but KEEPS a valid instructions.prompt', () => {
        const config = parseAgentConfig({ instructions: { prompt: 'keep me' }, conversation_workflow: { steps: [] } });
        expect(config?.instructions).toBe('keep me');
        expect(config?.conversationWorkflow).toBeUndefined();
    });

    it('reads greeting + personality', () => {
        const config = parseAgentConfig({ greeting: 'Hi, thanks for calling Acme!', personality: 'warm and concise' });
        expect(config?.greeting).toBe('Hi, thanks for calling Acme!');
        expect(config?.personality).toBe('warm and concise');
    });

    it('parses tool_config.enabledTools with defaults, preserving authLevel/config', () => {
        const config = parseAgentConfig({
            tool_config: {
                enabledTools: [
                    { toolId: 'knowledge_search' }, // enabled defaults true, authLevel "none"
                    { toolId: 'notify_humans', enabled: false, authLevel: 'oauth', config: { channel: 'ops' } },
                    { enabled: true }, // no toolId → skipped
                    42, // not an object → skipped
                ],
            },
        });
        expect(config?.enabledTools).toEqual([
            { toolId: 'knowledge_search', enabled: true, authLevel: 'none', config: undefined },
            { toolId: 'notify_humans', enabled: false, authLevel: 'oauth', config: { channel: 'ops' } },
        ]);
    });

    it('treats an empty enabledTools list as no restriction (undefined)', () => {
        expect(parseAgentConfig({ tool_config: { enabledTools: [] } })).toBeUndefined();
        expect(parseAgentConfig({ tool_config: {} })).toBeUndefined();
    });

    it('returns undefined when nothing usable is present', () => {
        expect(parseAgentConfig({})).toBeUndefined();
        expect(parseAgentConfig({ instructions: { prompt: '   ' } })).toBeUndefined();
        expect(parseAgentConfig(null)).toBeUndefined();
        expect(parseAgentConfig('nope')).toBeUndefined();
    });

    it('never throws on hostile input', () => {
        expect(() => parseAgentConfig(undefined)).not.toThrow();
        expect(() => parseAgentConfig({ instructions: 123, tool_config: 'x' })).not.toThrow();
    });
});

describe('assembleSystemPrompt', () => {
    it('returns the base prompt unchanged when no config', () => {
        expect(assembleSystemPrompt(BASE, undefined, undefined, true)).toBe(BASE);
    });

    it('makes per-agent instructions the primary body and keeps the base rules', () => {
        const config: AgentConfig = { instructions: 'You are Ada.' };
        const prompt = assembleSystemPrompt(BASE, config, undefined, true);
        expect(prompt).toContain('<AgentInstructions>\nYou are Ada.\n</AgentInstructions>');
        expect(prompt).toContain(BASE);
        // Instructions come before the base rules.
        expect(prompt.indexOf('You are Ada.')).toBeLessThan(prompt.indexOf(BASE));
    });

    it('includes the base once when there are no instructions', () => {
        const prompt = assembleSystemPrompt(BASE, { personality: 'warm' }, undefined, true);
        expect(prompt).toContain('<Personality>\nwarm\n</Personality>');
        expect(prompt).toContain(BASE);
    });

    it('folds in personality always and the greeting on the first turn', () => {
        const prompt = assembleSystemPrompt(BASE, { instructions: 'x', greeting: 'Hi there', personality: 'warm' }, undefined, true);
        expect(prompt).toContain('<Personality>\nwarm\n</Personality>');
        expect(prompt).toContain('Hi there');
    });

    it('drops the greeting section on later turns but keeps personality', () => {
        const prompt = assembleSystemPrompt(BASE, { instructions: 'x', greeting: 'Hi there', personality: 'warm' }, undefined, false);
        expect(prompt).toContain('<Personality>\nwarm\n</Personality>');
        expect(prompt).not.toContain('Hi there');
        expect(prompt).not.toContain('GreetingAwareness');
    });

    it('renders the current workflow step into the prompt', () => {
        const config: AgentConfig = {
            instructions: 'x',
            conversationWorkflow: { goal: 'Book a demo', steps: [{ id: 'greet', intent: 'Greet', criteria: 'Greeted' }, { id: 'book', intent: 'Book', criteria: 'Booked' }] },
        };
        expect(assembleSystemPrompt(BASE, config, 'book', false)).toContain('CURRENT STEP (2/2): book');
        expect(assembleSystemPrompt(BASE, config, undefined, true)).toContain('CURRENT STEP (1/2): greet');
    });
});

describe('StaticAgentConfigResolver', () => {
    it('resolves a known agentId and returns undefined for an unknown one', () => {
        const resolver = new StaticAgentConfigResolver({ 'agent-1': { instructions: 'one' } });
        expect(resolver.resolve('agent-1')?.instructions).toBe('one');
        expect(resolver.resolve('agent-2')).toBeUndefined();
    });
});
