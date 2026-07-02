/**
 * `verify_otp` action — dispatcher-level parity with the Rust reference
 * `tests/otp_flow.rs`.
 *
 * Drives {@link FrameDispatcher.dispatch} directly (no socket) so the
 * credential-accepting surface is exercised exactly as a client hits it. A stub
 * {@link OtpService} stands in for the host (the reference server never generates or
 * validates a code itself):
 *   - a `verified` outcome → `otp_verified` AND the session is now marked
 *     identity-verified on the store;
 *   - a non-verified outcome → `otp_invalid` carrying the host's remaining-attempt
 *     count + machine-readable reason;
 *   - NO OtpService installed → fail closed with `otp_invalid` (`NOT_FOUND`);
 *   - an unknown session id → `error` (`SESSION_NOT_FOUND`);
 *   - a missing `code` / `sessionId` / `requestId` → validation error.
 */
import { MockLlmProvider } from '@smooai/smooth-operator-core';
import { describe, expect, it } from 'vitest';

import { FrameDispatcher } from '../src/frameDispatcher.js';
import type { OtpService, OtpVerifyOutcome } from '../src/otp.js';
import type { Frame } from '../src/protocol.js';
import { InMemorySessionStore } from '../src/sessionStore.js';

/** A stub host OTP service returning a fixed verify outcome + a masked email delivery. */
function stubOtp(outcome: OtpVerifyOutcome): OtpService {
    return {
        sendOtp: () => ({ channel: 'email', maskedDestination: 'j***@example.com' }),
        verifyOtp: () => outcome,
    };
}

/** A dispatcher over a fresh store with one email-contact session; returns both. */
async function setup(otpService?: OtpService) {
    const store = new InMemorySessionStore();
    const session = await store.createSession('agent-otp', 'Alice', 'alice@example.com');
    const dispatcher = new FrameDispatcher({ store, chatClient: new MockLlmProvider(), otpService });
    const sink: Frame[] = [];
    const dispatch = (frame: Record<string, unknown>) => dispatcher.dispatch(JSON.stringify(frame), (f) => sink.push(f));
    return { store, session, sink, dispatch };
}

const inner = (ev: Frame): Record<string, unknown> => (ev.data as Record<string, unknown>).data as Record<string, unknown>;

describe('verify_otp action', () => {
    it('a verified code emits otp_verified and marks the session authenticated', async () => {
        const { store, session, sink, dispatch } = await setup(stubOtp({ verified: true }));
        expect(session.otpVerified ?? false).toBe(false);

        await dispatch({ action: 'verify_otp', requestId: 'vo-1', sessionId: session.sessionId, code: '123456' });

        expect(sink).toHaveLength(1);
        expect(sink[0]!.type).toBe('otp_verified');
        expect(sink[0]!.requestId).toBe('vo-1');
        expect(inner(sink[0]!).message).toBe('Identity verified successfully.');
        expect((await store.getSession(session.sessionId))?.otpVerified).toBe(true);
    });

    it('a rejected code reflects the host attempts + reason and does NOT authenticate', async () => {
        const { store, session, sink, dispatch } = await setup(stubOtp({ verified: false, attemptsRemaining: 2, error: 'INVALID_CODE', message: 'Invalid code. 2 attempt(s) remaining.' }));

        await dispatch({ action: 'verify_otp', requestId: 'vo-1', sessionId: session.sessionId, code: '000000' });

        expect(sink[0]!.type).toBe('otp_invalid');
        expect(inner(sink[0]!).attemptsRemaining).toBe(2);
        expect(inner(sink[0]!).error).toBe('INVALID_CODE');
        expect((await store.getSession(session.sessionId))?.otpVerified ?? false).toBe(false);
    });

    it('fails closed with otp_invalid/NOT_FOUND when no OtpService is installed', async () => {
        const { store, session, sink, dispatch } = await setup(undefined);

        await dispatch({ action: 'verify_otp', requestId: 'vo-1', sessionId: session.sessionId, code: '123456' });

        expect(sink[0]!.type).toBe('otp_invalid');
        expect(inner(sink[0]!).error).toBe('NOT_FOUND');
        expect(inner(sink[0]!).attemptsRemaining).toBe(0);
        expect((await store.getSession(session.sessionId))?.otpVerified ?? false).toBe(false);
    });

    it('an unknown session id is a clean SESSION_NOT_FOUND error (adversarial)', async () => {
        const { sink, dispatch } = await setup(stubOtp({ verified: true }));

        await dispatch({ action: 'verify_otp', requestId: 'vo-1', sessionId: 'no-such-session', code: '123456' });

        expect(sink[0]!.type).toBe('error');
        expect((sink[0]!.error as Record<string, unknown>).code).toBe('SESSION_NOT_FOUND');
    });

    it('a missing code is a validation error', async () => {
        const { session, sink, dispatch } = await setup(stubOtp({ verified: true }));

        await dispatch({ action: 'verify_otp', requestId: 'vo-1', sessionId: session.sessionId });

        expect(sink[0]!.type).toBe('error');
        expect((sink[0]!.error as Record<string, unknown>).code).toBe('VALIDATION_ERROR');
    });

    it('a missing sessionId is a validation error', async () => {
        const { sink, dispatch } = await setup(stubOtp({ verified: true }));

        await dispatch({ action: 'verify_otp', requestId: 'vo-1', code: '123456' });

        expect(sink[0]!.type).toBe('error');
        expect((sink[0]!.error as Record<string, unknown>).code).toBe('VALIDATION_ERROR');
    });

    it('a missing requestId is a validation error', async () => {
        const { session, sink, dispatch } = await setup(stubOtp({ verified: true }));

        await dispatch({ action: 'verify_otp', sessionId: session.sessionId, code: '123456' });

        expect(sink[0]!.type).toBe('error');
        expect((sink[0]!.error as Record<string, unknown>).code).toBe('VALIDATION_ERROR');
    });
});
