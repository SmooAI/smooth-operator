/**
 * Skill resolution — the engine side of `send_message.skill` (Rust PR #338).
 *
 * A *skill* is a named, reusable recipe (a markdown body). Before this seam, every client resolved the
 * skill itself and prepended the body to the message text, so the wire carried prose — and the body
 * persisted into conversation history, where it was replayed as context on every subsequent turn. Now
 * the wire carries **intent** (`skill: "code-review"`) and the server composes it into the turn's
 * system prompt, leaving the persisted user message exactly what the user typed.
 *
 * Two pieces, mirroring `rust/smooth-operator-server/src/skills.rs`:
 * - {@link SkillResolver} — the host seam, installed via `serve({ skillResolver })`.
 * - {@link DirSkillResolver} — the working default: `<root>/<name>/SKILL.md` over the roots in
 *   `SMOOTH_SKILLS_DIR` (a `:`-separated list, first match wins). Unset ⇒ no resolver is installed and
 *   any `skill` field is a clean `SKILL_NOT_FOUND`, so a multi-tenant deploy never reads host skills
 *   by accident.
 */
import { readFile } from 'node:fs/promises';
import { join } from 'node:path';

/** Env var naming the skill roots for {@link DirSkillResolver}: `:`-separated, searched in order. */
export const SKILLS_DIR_ENV = 'SMOOTH_SKILLS_DIR';

/**
 * Resolves a skill name to its markdown body. `undefined` means "unknown skill" — the dispatcher turns
 * that into a `SKILL_NOT_FOUND` error and does NOT run the turn, so a typo'd skill never silently
 * degrades into an unskilled answer.
 */
export interface SkillResolver {
    resolve(name: string): Promise<string | undefined>;
}

/**
 * Render a resolved skill as a system-prompt section. The skill moved from the *user message* (where
 * clients used to prepend it) to the *system prompt*, so this framing line is what tells the model the
 * skill applies to this turn.
 */
export function skillSection(name: string, body: string): string {
    return `## Skill: ${name}\n\nThe user invoked this skill for this turn. Follow it.\n\n${body}`;
}

/**
 * Whether `name` is a legal skill name. Deliberately strict: ASCII alphanumerics, `-` and `_` only.
 * That is the kebab-case convention skills already use, and it makes path traversal (`..`, `/`, `\`,
 * NUL) *unrepresentable* rather than filtered — the name is joined onto a filesystem root below.
 */
export function isValidSkillName(name: string): boolean {
    return name.length > 0 && name.length <= 128 && /^[A-Za-z0-9_-]+$/.test(name);
}

/**
 * Strip a leading YAML frontmatter block (`---` … `---`), returning the body. SKILL.md files carry
 * frontmatter (description, triggers, allowed tools) that is discovery metadata, not instructions —
 * the model should see only the body. Unterminated frontmatter is returned untouched rather than
 * swallowing the file.
 */
export function stripFrontmatter(text: string): string {
    if (!text.startsWith('---\n')) return text;
    const rest = text.slice(4);
    // The closing fence is a line that is exactly `---`.
    for (let idx = rest.indexOf('---'); idx !== -1; idx = rest.indexOf('---', idx + 1)) {
        const atLineStart = idx === 0 || rest[idx - 1] === '\n';
        const restOfLine = rest.slice(idx + 3);
        if (atLineStart && restOfLine.startsWith('\n')) return restOfLine.replace(/^\n+/, '');
    }
    return text;
}

/** The default resolver: reads `<root>/<name>/SKILL.md`, first root wins. */
export class DirSkillResolver implements SkillResolver {
    constructor(readonly roots: readonly string[]) {}

    /**
     * Build from {@link SKILLS_DIR_ENV}. `undefined` when the var is unset or names no non-empty root,
     * so the caller installs nothing and the feature stays off by default.
     */
    static fromEnv(): DirSkillResolver | undefined {
        const list = process.env[SKILLS_DIR_ENV];
        return list === undefined ? undefined : DirSkillResolver.fromPathList(list);
    }

    /**
     * Build from a `:`-separated path list — the parsed half of {@link fromEnv}, so it is testable
     * without touching the process environment.
     */
    // ponytail: ':' hardcoded to match the Rust reference rather than a platform separator. On Windows
    // that makes a drive-qualified root ("C:\skills") unrepresentable; change it in both lanes at once
    // or they diverge.
    static fromPathList(list: string): DirSkillResolver | undefined {
        const roots = list
            .split(':')
            .map((s) => s.trim())
            .filter((s) => s.length > 0);
        return roots.length > 0 ? new DirSkillResolver(roots) : undefined;
    }

    async resolve(name: string): Promise<string | undefined> {
        if (!isValidSkillName(name)) return undefined;
        for (const root of this.roots) {
            let text: string;
            try {
                text = await readFile(join(root, name, 'SKILL.md'), 'utf8');
            } catch {
                continue;
            }
            const body = stripFrontmatter(text).trim();
            if (body.length > 0) return body;
        }
        return undefined;
    }
}

/**
 * Resolve `name` through `resolver` and render it as a system-prompt section. `undefined` when there is
 * no resolver installed or the skill is unknown — both are `SKILL_NOT_FOUND` to the client (the
 * distinction is a deployment detail the caller should not have to guess at).
 */
export async function resolveSection(resolver: SkillResolver | undefined, name: string): Promise<string | undefined> {
    if (!resolver) return undefined;
    const body = await resolver.resolve(name);
    return body === undefined ? undefined : skillSection(name, body);
}
