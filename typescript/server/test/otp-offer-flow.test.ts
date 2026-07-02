/**
 * Integration: the end-user OTP offer flow over a REAL WebSocket server.
 *
 * Parity with the Rust reference's post-turn OTP offer + the auth-gate/session-identity
 * seam. Proves end-to-end that:
 *  - an `end_user` tool refused on an unverified session (with a contact + an OTP
 *    service installed) triggers `otp_verification_required` → `otp_sent`, IN THAT
 *    ORDER, BEFORE the terminal `eventual_response`;
 *  - an `admin` refusal is NEVER offered OTP (not OTP-remediable);
 *  - a refusal with NO session contact offers nothing (no channel to reach);
 *  - after a successful `verify_otp`, the SAME session's `end_user` tool RUNS on the
 *    re-sent message (the verified bit threads into the gate) and no OTP is re-offered.
 */
import { MockLlmProvider } from '@smooai/smooth-operator-core';
import { afterEach, describe, expect, it } from 'vitest';

import type { OtpService } from '../src/otp.js';
import { serve, type RunningServer } from '../src/server.js';
import type { ServerTool } from '../src/toolGating.js';
import { TestClient } from './wsClient.js';

const AGENT = '11111111-1111-1111-1111-111111111111';

/** A stub host OTP service: delivers to a masked email, verifies with a fixed outcome. */
function stubOtp(verified: boolean): OtpService {
    return {
        sendOtp: () => ({ channel: 'email', maskedDestination: 'j***@example.com' }),
        verifyOtp: () => (verified ? { verified: true } : { verified: false, attemptsRemaining: 0, error: 'INVALID_CODE', message: 'nope' }),
    };
}

/** A gate-participating end_user tool that records each execution. */
function recordingTool(name: string, calls: string[]): ServerTool {
    return {
        name,
        description: name,
        parameters: { type: 'object', properties: {} },
        supportsAuthRequirement: true,
        async execute() {
            calls.push(name);
            return `EXECUTED ${name}`;
        },
    };
}

// One turn: model calls the gated tool (script[0]), engine feeds the result back,
// model answers (script[1]).
const oneTurn = () => new MockLlmProvider().pushToolCall('c1', 'crm', JSON.stringify({})).pushText('done');

async function openSession(client: TestClient, opts: { userEmail?: string } = {}): Promise<string> {
    client.sendAction({ action: 'create_conversation_session', requestId: 'cs-1', agentId: AGENT, ...opts });
    const created = await client.receive();
    return (created.data as Record<string, unknown>).sessionId as string;
}

const endUserAgentConfig = () => ({ [AGENT]: { visibility: 'public' as const, enabledTools: [{ toolId: 'crm', enabled: true, authLevel: 'end_user' }] } });

function typesOf(seen: Record<string, unknown>[]): string[] {
    return seen.map((f) => f.type as string);
}

describe('OTP offer flow over a real WebSocket', () => {
    let server: RunningServer | undefined;
    afterEach(async () => {
        await server?.close();
        server = undefined;
    });

    it('offers OTP (verification_required → sent) before the terminal response on an end_user refusal', async () => {
        const calls: string[] = [];
        const { StaticAgentConfigResolver } = await import('../src/agentConfig.js');
        server = await serve({
            chatClient: oneTurn(),
            tools: [recordingTool('crm', calls)],
            otpService: stubOtp(true),
            agentConfig: new StaticAgentConfigResolver(endUserAgentConfig()),
        });
        const client = await TestClient.connect(server.url);
        const sessionId = await openSession(client, { userEmail: 'alice@example.com' });

        client.sendAction({ action: 'send_message', requestId: 'sm-1', sessionId, message: 'look up my account' });
        const { seen } = await client.receiveUntil('eventual_response');
        const types = typesOf(seen);

        // The tool was refused (never executed) and OTP was offered in order.
        expect(calls).toHaveLength(0);
        const reqIdx = types.indexOf('otp_verification_required');
        const sentIdx = types.indexOf('otp_sent');
        const respIdx = types.indexOf('eventual_response');
        expect(reqIdx).toBeGreaterThanOrEqual(0);
        expect(sentIdx).toBeGreaterThan(reqIdx);
        expect(respIdx).toBeGreaterThan(sentIdx);

        // The verification_required event advertises the email channel + the tool.
        const required = seen[reqIdx]!;
        const inner = (required.data as Record<string, unknown>).data as Record<string, unknown>;
        expect(inner.toolId).toBe('crm');
        expect(inner.authLevel).toBe('end_user');
        expect(inner.availableChannels).toEqual(['email']);
        expect(((seen[sentIdx]!.data as Record<string, unknown>).data as Record<string, unknown>).maskedDestination).toBe('j***@example.com');

        await client.close();
    });

    it('never offers OTP for an admin refusal', async () => {
        const calls: string[] = [];
        const { StaticAgentConfigResolver } = await import('../src/agentConfig.js');
        server = await serve({
            chatClient: oneTurn(),
            tools: [recordingTool('crm', calls)],
            otpService: stubOtp(true),
            agentConfig: new StaticAgentConfigResolver({ [AGENT]: { visibility: 'public', enabledTools: [{ toolId: 'crm', enabled: true, authLevel: 'admin' }] } }),
        });
        const client = await TestClient.connect(server.url);
        const sessionId = await openSession(client, { userEmail: 'alice@example.com' });

        client.sendAction({ action: 'send_message', requestId: 'sm-1', sessionId, message: 'admin lookup' });
        const { seen } = await client.receiveUntil('eventual_response');

        expect(calls).toHaveLength(0);
        expect(typesOf(seen)).not.toContain('otp_verification_required');
        await client.close();
    });

    it('offers nothing when the refused session has no contact to reach', async () => {
        const calls: string[] = [];
        const { StaticAgentConfigResolver } = await import('../src/agentConfig.js');
        server = await serve({
            chatClient: oneTurn(),
            tools: [recordingTool('crm', calls)],
            otpService: stubOtp(true),
            agentConfig: new StaticAgentConfigResolver(endUserAgentConfig()),
        });
        const client = await TestClient.connect(server.url);
        // No userEmail → no contact stashed → server can't offer OTP.
        const sessionId = await openSession(client);

        client.sendAction({ action: 'send_message', requestId: 'sm-1', sessionId, message: 'look up my account' });
        const { seen } = await client.receiveUntil('eventual_response');

        expect(typesOf(seen)).not.toContain('otp_verification_required');
        await client.close();
    });

    it('runs the end_user tool on the re-sent message once the session is OTP-verified', async () => {
        const calls: string[] = [];
        const { StaticAgentConfigResolver } = await import('../src/agentConfig.js');
        // Two turns' worth of script: the FIRST send_message (refused) + the re-sent one
        // (executes). verify_otp does not consume the model.
        const mock = new MockLlmProvider()
            .pushToolCall('c1', 'crm', JSON.stringify({}))
            .pushText('done-1')
            .pushToolCall('c2', 'crm', JSON.stringify({}))
            .pushText('done-2');
        server = await serve({
            chatClient: mock,
            tools: [recordingTool('crm', calls)],
            otpService: stubOtp(true),
            agentConfig: new StaticAgentConfigResolver(endUserAgentConfig()),
        });
        const client = await TestClient.connect(server.url);
        const sessionId = await openSession(client, { userEmail: 'alice@example.com' });

        // Turn 1: refused → OTP offered, tool did not run.
        client.sendAction({ action: 'send_message', requestId: 'sm-1', sessionId, message: 'look up my account' });
        await client.receiveUntil('eventual_response');
        expect(calls).toHaveLength(0);

        // Verify the code → session becomes identity-verified.
        client.sendAction({ action: 'verify_otp', requestId: 'sm-1', sessionId, code: '123456' });
        const verified = await client.receive();
        expect(verified.type).toBe('otp_verified');

        // Turn 2 (re-send): the verified bit threads into the gate → the tool RUNS and
        // no OTP is re-offered.
        client.sendAction({ action: 'send_message', requestId: 'sm-2', sessionId, message: 'look up my account' });
        const { seen } = await client.receiveUntil('eventual_response');
        expect(calls).toEqual(['crm']);
        expect(typesOf(seen)).not.toContain('otp_verification_required');

        await client.close();
    });
});
