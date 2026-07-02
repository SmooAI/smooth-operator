/**
 * Unit tests for the tool authLevel gate edges an e2e can't reach cheaply.
 */
import type { Tool } from '@smooai/smooth-operator-core';
import { describe, expect, it } from 'vitest';

import type { AgentConfig } from '../src/agentConfig.js';
import { gateTools, type ServerTool } from '../src/toolGating.js';

function tool(name: string, supportsAuthRequirement: boolean, ran: string[]): ServerTool {
    return {
        name,
        description: name,
        parameters: { type: 'object', properties: {} },
        supportsAuthRequirement,
        async execute() {
            ran.push(name);
            return 'ok';
        },
    };
}

describe('gateTools', () => {
    it('does NOT gate a tool that never opts in, even at admin authLevel on a public agent', async () => {
        const ran: string[] = [];
        const config: AgentConfig = { visibility: 'public', enabledTools: [{ toolId: 'crm', enabled: true, authLevel: 'admin' }] };
        const [gated] = gateTools([tool('crm', false, ran)], config, 'conv', undefined);
        expect(await gated!.execute({})).toBe('ok'); // executed, not blocked
        expect(ran).toEqual(['crm']);
    });

    it('passes a plain tool with no entry + no config through untouched (identity)', () => {
        const t: Tool = { name: 'plain', description: 'p', parameters: {}, execute: async () => 'x' };
        const [out] = gateTools([t], { visibility: 'public' }, 'conv', undefined);
        expect(out).toBe(t);
    });
});
