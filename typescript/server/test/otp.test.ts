/**
 * Unit tests for the OTP seam helpers + the four protocol event builders.
 *
 * Parity with the Rust reference `smooth_operator::otp` unit tests (channel/error
 * wire strings, contact → available channels) and `protocol.rs`'s
 * `otp_*_matches_spec_shape` builder-shape tests. The wire shapes here are what the
 * shared `spec/events/otp-*.schema.json` + `spec/conformance/fixtures.json` gate.
 */
import { describe, expect, it } from 'vitest';

import { availableChannels, isContactEmpty, type OtpContact } from '../src/otp.js';
import * as protocol from '../src/protocol.js';

describe('OTP contact → available channels', () => {
    it('an empty contact offers no channels', () => {
        const contact: OtpContact = {};
        expect(isContactEmpty(contact)).toBe(true);
        expect(availableChannels(contact)).toEqual([]);
    });

    it('an email-only contact offers email', () => {
        const contact: OtpContact = { email: 'a@example.com' };
        expect(isContactEmpty(contact)).toBe(false);
        expect(availableChannels(contact)).toEqual(['email']);
    });

    it('a phone-only contact offers sms', () => {
        expect(availableChannels({ phone: '+15551234567' })).toEqual(['sms']);
    });

    it('both contacts offer email then sms (order is load-bearing)', () => {
        expect(availableChannels({ email: 'a@example.com', phone: '+15551234567' })).toEqual(['email', 'sms']);
    });
});

describe('OTP protocol event builders', () => {
    it('otpVerificationRequired matches the spec double-nested shape', () => {
        const ev = protocol.otpVerificationRequired('r1', 'pay_invoice', "Verify your identity to continue using 'pay_invoice'.", ['email'], 'end_user');
        expect(ev.type).toBe('otp_verification_required');
        expect(ev.requestId).toBe('r1');
        const outer = ev.data as Record<string, unknown>;
        expect(outer.requestId).toBe('r1');
        const inner = outer.data as Record<string, unknown>;
        expect(inner.toolId).toBe('pay_invoice');
        expect(inner.authLevel).toBe('end_user');
        expect(inner.availableChannels).toEqual(['email']);
        expect(typeof inner.actionDescription).toBe('string');
        expect(typeof ev.timestamp).toBe('number');
    });

    it('otpSent carries channel + masked destination', () => {
        const ev = protocol.otpSent('r1', 'email', 'j***@example.com');
        expect(ev.type).toBe('otp_sent');
        const inner = (ev.data as Record<string, unknown>).data as Record<string, unknown>;
        expect(inner.channel).toBe('email');
        expect(inner.maskedDestination).toBe('j***@example.com');
    });

    it('otpVerified carries the confirmation message', () => {
        const ev = protocol.otpVerified('r1', 'Identity verified successfully.');
        expect(ev.type).toBe('otp_verified');
        expect((ev.data as Record<string, unknown>).requestId).toBe('r1');
        const inner = (ev.data as Record<string, unknown>).data as Record<string, unknown>;
        expect(inner.message).toBe('Identity verified successfully.');
    });

    it('otpInvalid carries error + attempts when the host determined a cause', () => {
        const ev = protocol.otpInvalid('r1', 'INVALID_CODE', 2, 'Invalid code. 2 attempt(s) remaining.');
        const inner = (ev.data as Record<string, unknown>).data as Record<string, unknown>;
        expect(inner.error).toBe('INVALID_CODE');
        expect(inner.attemptsRemaining).toBe(2);
        expect(inner.message).toContain('remaining');
    });

    it('otpInvalid OMITS the error key when the host could not determine a cause', () => {
        const ev = protocol.otpInvalid('r1', undefined, 0, 'Verification failed.');
        const inner = (ev.data as Record<string, unknown>).data as Record<string, unknown>;
        expect('error' in inner).toBe(false);
        expect(inner.attemptsRemaining).toBe(0);
    });
});
