/**
 * Skill resolution (Rust PR #338, `rust/smooth-operator-server/src/skills.rs`) — the TS server's parity.
 *
 * The wire carries INTENT (`send_message.skill: "code-review"`); the server resolves the body and
 * composes it into the turn's system prompt, so the persisted user message stays exactly what the user
 * typed and skill prose never accumulates in history to be replayed every later turn.
 *
 * Unit tests are named after their Rust counterparts so parity gaps stay visible; the WS tests cover
 * the handler behavior (fail-CLOSED) that the Rust unit tests don't reach.
 */
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { MockLlmProvider } from '@smooai/smooth-operator-core';
import { afterEach, describe, expect, it } from 'vitest';

import { serve, type RunningServer } from '../src/server.js';
import { DirSkillResolver, isValidSkillName, resolveSection, skillSection, stripFrontmatter } from '../src/skills.js';
import { TestClient } from './wsClient.js';

let tmp: string;
function root(name: string, body: string, sub = 'only'): string {
    tmp ??= mkdtempSync(join(tmpdir(), 'skills-'));
    const dir = join(tmp, sub, name);
    mkdirSync(dir, { recursive: true });
    writeFileSync(join(dir, 'SKILL.md'), body);
    return join(tmp, sub);
}

afterEach(() => {
    if (tmp) {
        rmSync(tmp, { recursive: true, force: true });
        tmp = undefined as unknown as string;
    }
});

// ── rust: rejects_traversal_and_separators ───────────────────────────────────

describe('isValidSkillName', () => {
    it('rejects traversal and separators', () => {
        expect(isValidSkillName('code-review')).toBe(true);
        expect(isValidSkillName('add_show')).toBe(true);
        expect(isValidSkillName('')).toBe(false);
        expect(isValidSkillName('..')).toBe(false);
        expect(isValidSkillName('../../etc/passwd')).toBe(false);
        expect(isValidSkillName('a/b')).toBe(false);
        expect(isValidSkillName('a\\b')).toBe(false);
        expect(isValidSkillName('a b')).toBe(false);
        expect(isValidSkillName('a'.repeat(129))).toBe(false);
        expect(isValidSkillName('a'.repeat(128))).toBe(true);
    });
});

// ── rust: strips_frontmatter_only_when_well_formed ───────────────────────────

describe('stripFrontmatter', () => {
    it('strips frontmatter only when well formed', () => {
        expect(stripFrontmatter('---\nname: x\ndescription: y\n---\nBody here\n')).toBe('Body here\n');
        expect(stripFrontmatter('Body here\n')).toBe('Body here\n');
        // Unterminated → untouched (don't swallow the file).
        expect(stripFrontmatter('---\nname: x\n')).toBe('---\nname: x\n');
        // A `---` mid-body (a markdown rule) after real frontmatter still closes at the FIRST fence.
        expect(stripFrontmatter('---\nname: x\n---\nintro\n\n---\n\nmore\n')).toBe('intro\n\n---\n\nmore\n');
    });
});

// ── rust: dir_resolver_reads_first_matching_root / path_list_parsing_is_off_when_empty ──

describe('DirSkillResolver', () => {
    it('reads the first matching root', async () => {
        const high = root('greet', '---\nname: greet\n---\nHIGH BODY\n', 'high');
        const low = root('greet', '---\nname: greet\n---\nLOW BODY\n', 'low');

        const resolver = new DirSkillResolver([high, low]);
        expect(await resolver.resolve('greet')).toBe('HIGH BODY');
        expect(await resolver.resolve('nope')).toBeUndefined();
        // Traversal can't escape the root even if a file exists above it.
        expect(await resolver.resolve('../low/greet')).toBeUndefined();

        expect(await new DirSkillResolver([join(tmp, 'missing'), low]).resolve('greet')).toBe('LOW BODY');
    });

    it('path list parsing is off when empty', () => {
        expect(DirSkillResolver.fromPathList('')).toBeUndefined();
        expect(DirSkillResolver.fromPathList('  : ')).toBeUndefined();
        expect(DirSkillResolver.fromPathList('/a: /b :')?.roots).toEqual(['/a', '/b']);
    });
});

// ── rust: resolve_section_composes_and_reports_unknown ───────────────────────

describe('resolveSection', () => {
    it('composes and reports unknown', async () => {
        const r = new DirSkillResolver([root('review', 'Check the diff.')]);
        const section = await resolveSection(r, 'review');
        expect(section?.startsWith('## Skill: review\n')).toBe(true);
        expect(section?.endsWith('Check the diff.')).toBe(true);

        expect(await resolveSection(r, 'unknown')).toBeUndefined();
        // No resolver installed ⇒ every skill is unknown.
        expect(await resolveSection(undefined, 'review')).toBeUndefined();
    });

    it('renders the framing line the model keys on', () => {
        expect(skillSection('x', 'BODY')).toBe('## Skill: x\n\nThe user invoked this skill for this turn. Follow it.\n\nBODY');
    });
});

// ── handler parity over a real socket ────────────────────────────────────────

describe('send_message.skill (over the wire)', () => {
    let server: RunningServer | undefined;
    afterEach(async () => {
        await server?.close();
        server = undefined;
    });

    async function start(skillRoot: string | undefined, chat: MockLlmProvider) {
        server = await serve({ chatClient: chat, skillResolver: skillRoot ? new DirSkillResolver([skillRoot]) : undefined });
        const client = await TestClient.connect(server.url);
        client.sendAction({ action: 'create_conversation_session', requestId: 'cs', agentId: 'agent' });
        const sessionId = ((await client.receive()).data as Record<string, unknown>).sessionId as string;
        return { client, sessionId };
    }

    it('fails CLOSED on an unknown skill — SKILL_NOT_FOUND and the turn never reaches the model', async () => {
        const chat = new MockLlmProvider().pushText('should never run');
        const { client, sessionId } = await start(root('review', 'Check the diff.'), chat);

        client.sendAction({ action: 'send_message', requestId: 'r2', sessionId, message: 'hi', skill: 'nope' });
        const { terminal } = await client.receiveUntil('error');
        expect((terminal.error as Record<string, unknown>).code).toBe('SKILL_NOT_FOUND');
        // Fail-closed is the whole point: no model call, and no 202 ack promising one.
        expect(chat.calls.length).toBe(0);
        await client.close();
    });

    it('puts a resolved skill in the SYSTEM prompt, leaving the user message exactly as typed', async () => {
        const chat = new MockLlmProvider().pushText('done');
        const { client, sessionId } = await start(root('review', '---\nname: review\n---\nCheck the diff.\n'), chat);

        client.sendAction({ action: 'send_message', requestId: 'r2', sessionId, message: 'look at this', skill: 'review' });
        await client.receiveUntil('eventual_response');

        const messages = chat.calls[0].messages as Array<Record<string, unknown>>;
        const system = messages.filter((m) => m.role === 'system').map((m) => String(m.content)).join('\n');
        expect(system).toContain('## Skill: review');
        expect(system).toContain('Check the diff.');
        // The frontmatter is discovery metadata, not instructions — the model must not see it.
        expect(system).not.toContain('name: review');

        const user = messages.filter((m) => m.role === 'user').map((m) => JSON.stringify(m.content)).join('\n');
        expect(user).toContain('look at this');
        expect(user).not.toContain('Check the diff.');
        await client.close();
    });

    it('is unchanged when the field is absent, and treats a blank skill as absent', async () => {
        const chat = new MockLlmProvider().pushText('a').pushText('b');
        // A resolver IS installed — so 'blank means absent' is proven, not merely unreachable.
        const { client, sessionId } = await start(root('review', 'Check the diff.'), chat);

        client.sendAction({ action: 'send_message', requestId: 'r2', sessionId, message: 'plain' });
        await client.receiveUntil('eventual_response');
        const system = (chat.calls[0].messages as Array<Record<string, unknown>>)
            .filter((m) => m.role === 'system')
            .map((m) => String(m.content))
            .join('\n');
        expect(system).not.toContain('## Skill:');

        // Whitespace-only ⇒ absent, NOT SKILL_NOT_FOUND (Rust trims then filters).
        client.sendAction({ action: 'send_message', requestId: 'r3', sessionId, message: 'plain2', skill: '   ' });
        const { terminal } = await client.receiveUntil('eventual_response');
        expect(terminal.type).toBe('eventual_response');
        await client.close();
    });
});
