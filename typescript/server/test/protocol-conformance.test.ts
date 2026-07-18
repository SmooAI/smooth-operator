/**
 * Protocol conformance: round-trip the `spec/conformance/fixtures.json` golden
 * messages through this server's protocol builders + parse path, and assert the
 * shapes match the shared contract every server speaks.
 *
 * This is the TS parity of the C#/Rust conformance checks: the same fixtures gate
 * every language's server, so a drift in any one of them is caught here.
 */
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { describe, expect, it } from 'vitest';

import * as protocol from '../src/protocol.js';

const __dirname = dirname(fileURLToPath(import.meta.url));
const SPEC_DIR = join(__dirname, '..', '..', '..', 'spec');

interface Fixture {
    description: string;
    instance: Record<string, unknown>;
}
type Fixtures = Record<string, Fixture | string>;

const fixtures = JSON.parse(readFileSync(join(SPEC_DIR, 'conformance', 'fixtures.json'), 'utf8')) as Fixtures;

function fixture(name: string): Record<string, unknown> {
    const entry = fixtures[name];
    if (!entry || typeof entry === 'string') throw new Error(`missing fixture ${name}`);
    return entry.instance;
}

/** A deep round-trip through JSON, proving the builder output is serializable + stable. */
function roundTrip(frame: Record<string, unknown>): Record<string, unknown> {
    return JSON.parse(JSON.stringify(frame)) as Record<string, unknown>;
}

describe('protocol conformance', () => {
    it('round-trips every fixture instance through JSON unchanged', () => {
        for (const [name, entry] of Object.entries(fixtures)) {
            if (typeof entry === 'string') continue; // the $comment header
            expect(roundTrip(entry.instance), name).toEqual(entry.instance);
        }
    });

    it('create_session_request fixture has the action discriminator the dispatcher routes on', () => {
        const req = fixture('create_session_request');
        expect(req.action).toBe('create_conversation_session');
        expect(typeof req.requestId).toBe('string');
        expect(typeof req.agentId).toBe('string');
    });

    it('immediateResponse builder matches the create_session_response shape', () => {
        const resp = fixture('create_session_response');
        const built = protocol.immediateResponse('req-a1b2c3d4-0001', 200, 'Session created', resp);
        const rt = roundTrip(built);
        expect(rt.type).toBe('immediate_response');
        expect(rt.status).toBe(200);
        expect(rt.requestId).toBe('req-a1b2c3d4-0001');
        expect((rt.data as Record<string, unknown>).sessionId).toBe(resp.sessionId);
        expect((rt.data as Record<string, unknown>).agentName).toBe(resp.agentName);
    });

    it('send_message_request fixture carries the fields the dispatcher reads', () => {
        const req = fixture('send_message_request');
        expect(req.action).toBe('send_message');
        expect(typeof req.sessionId).toBe('string');
        expect(typeof req.message).toBe('string');
    });

    it('streamChunk builder matches the stream_chunk_event nesting (type/requestId/node/data.state)', () => {
        const golden = fixture('stream_chunk_event');
        const built = protocol.streamChunk('req-a1b2c3d4-0002', 'knowledge_search', { snippets: ['x'] });
        const rt = roundTrip(built);
        expect(rt.type).toBe(golden.type);
        expect(rt.requestId).toBe(golden.requestId);
        expect(rt.node).toBe(golden.node);
        const data = rt.data as Record<string, unknown>;
        expect(data.requestId).toBe('req-a1b2c3d4-0002');
        expect(data.node).toBe('knowledge_search');
        expect(data.state).toBeDefined();
    });

    it('eventualResponse builder reproduces the triple-nested data.data of the golden event', () => {
        const golden = fixture('eventual_response_event');
        const built = protocol.eventualResponse(
            'req-a1b2c3d4-0002',
            200,
            '66666666-6666-6666-6666-666666666666',
            protocol.generalResponse('Your order shipped.'),
            false,
        );
        const rt = roundTrip(built);
        expect(rt.type).toBe(golden.type);
        expect(rt.requestId).toBe(golden.requestId);
        expect(rt.status).toBe(200);
        const outer = rt.data as Record<string, unknown>;
        expect(outer.requestId).toBe('req-a1b2c3d4-0002');
        expect(outer.status).toBe(200);
        const inner = outer.data as Record<string, unknown>;
        expect(inner.messageId).toBe('66666666-6666-6666-6666-666666666666');
        expect(inner.needsEscalation).toBe(false);
        expect((inner.response as Record<string, unknown>).responseParts).toBeInstanceOf(Array);
        // No citations passed → the key is omitted (matches the no-citations golden).
        expect('citations' in inner).toBe(false);
    });

    it('eventualResponse with citations matches the with-citations golden (url omitted when absent)', () => {
        const golden = fixture('eventual_response_with_citations_event');
        const goldenCitations = (((golden.data as Record<string, unknown>).data as Record<string, unknown>).citations as Array<Record<string, unknown>>);

        const built = protocol.eventualResponse(
            'req-a1b2c3d4-0003',
            200,
            '77777777-7777-7777-7777-777777777777',
            protocol.generalResponse('Returns within 30 days.'),
            false,
            [
                { id: 'doc-returns-policy', title: 'acme/handbook@main#policies/returns.md', url: 'https://github.com/acme/handbook/blob/main/policies/returns.md', snippet: 'SmooAI returns...', score: 0.91 },
                { id: 'doc-shipping-policy', title: 'policies/shipping.md', snippet: 'Standard shipping...', score: 0.42 },
            ],
        );
        const inner = ((roundTrip(built).data as Record<string, unknown>).data as Record<string, unknown>);
        const citations = inner.citations as Array<Record<string, unknown>>;
        expect(citations).toHaveLength(2);
        // First citation carries a url; second omits it — exactly as the golden does.
        expect('url' in citations[0]!).toBe(true);
        expect('url' in citations[1]!).toBe(false);
        expect('url' in goldenCitations[1]!).toBe(false);
        expect(citations[0]!.score).toBe(0.91);
    });

    it('otp event builders reproduce the OTP conformance fixtures', () => {
        const required = fixture('otp_verification_required_event');
        const reqInner = ((required.data as Record<string, unknown>).data as Record<string, unknown>);
        const builtRequired = roundTrip(
            protocol.otpVerificationRequired(
                required.requestId as string,
                reqInner.toolId as string,
                reqInner.actionDescription as string,
                reqInner.availableChannels as string[],
                reqInner.authLevel as string,
            ),
        );
        expect(builtRequired.type).toBe('otp_verification_required');
        expect(((builtRequired.data as Record<string, unknown>).data as Record<string, unknown>)).toEqual(reqInner);

        const sent = fixture('otp_sent_event');
        const sentInner = ((sent.data as Record<string, unknown>).data as Record<string, unknown>);
        const builtSent = roundTrip(protocol.otpSent(sent.requestId as string, sentInner.channel as string, sentInner.maskedDestination as string));
        expect(((builtSent.data as Record<string, unknown>).data as Record<string, unknown>)).toEqual(sentInner);

        const verified = fixture('otp_verified_event');
        const verifiedInner = ((verified.data as Record<string, unknown>).data as Record<string, unknown>);
        const builtVerified = roundTrip(protocol.otpVerified(verified.requestId as string, verifiedInner.message as string));
        expect(((builtVerified.data as Record<string, unknown>).data as Record<string, unknown>)).toEqual(verifiedInner);

        const invalid = fixture('otp_invalid_event');
        const invalidInner = ((invalid.data as Record<string, unknown>).data as Record<string, unknown>);
        const builtInvalid = roundTrip(
            protocol.otpInvalid(invalid.requestId as string, invalidInner.error as string, invalidInner.attemptsRemaining as number, invalidInner.message as string),
        );
        expect(((builtInvalid.data as Record<string, unknown>).data as Record<string, unknown>)).toEqual(invalidInner);
    });

    it('verify_otp_request fixture carries the fields the dispatcher reads', () => {
        const req = fixture('verify_otp_request');
        expect(req.action).toBe('verify_otp');
        expect(typeof req.requestId).toBe('string');
        expect(typeof req.sessionId).toBe('string');
        expect(typeof req.code).toBe('string');
    });

    it('cancelled builder reproduces the cancelled_event golden (status 499, requestId echoed in the envelope + data)', () => {
        // The `cancelled_event` fixture ships with the Rust reference PR (#259); until it
        // lands the contract is pinned by the shape assertions below.
        const golden = fixtures['cancelled_event'];
        const built = roundTrip(protocol.cancelled('req-a1b2c3d4-0002'));
        expect(built.type).toBe('cancelled');
        expect(built.requestId).toBe('req-a1b2c3d4-0002');
        expect(built.status).toBe(499);
        expect(built.data).toEqual({ requestId: 'req-a1b2c3d4-0002', status: 499 });
        expect(typeof built.timestamp).toBe('number');
        // No answer payload: a cancelled turn produced no assistant message.
        expect('messageId' in (built.data as Record<string, unknown>)).toBe(false);
        if (golden && typeof golden !== 'string') {
            const { timestamp: _built, ...builtRest } = built;
            const { timestamp: _golden, ...goldenRest } = golden.instance;
            expect(builtRest).toEqual(goldenRest);
        }

        // No requestId anywhere → the field is omitted at both levels (schema-optional).
        const bare = roundTrip(protocol.cancelled());
        expect('requestId' in bare).toBe(false);
        expect('requestId' in (bare.data as Record<string, unknown>)).toBe(false);
        expect(bare.status).toBe(499);
    });

    it('pong/error builders carry the discriminators a client matches on', () => {
        expect(protocol.pong('p1').type).toBe('pong');
        const err = protocol.error('e1', 'VALIDATION_ERROR', 'bad');
        expect(err.type).toBe('error');
        // Descriptor is duplicated at the envelope level and under `data.error`.
        expect((err.error as Record<string, unknown>).code).toBe('VALIDATION_ERROR');
        expect(((err.data as Record<string, unknown>).error as Record<string, unknown>).code).toBe('VALIDATION_ERROR');
    });
});
