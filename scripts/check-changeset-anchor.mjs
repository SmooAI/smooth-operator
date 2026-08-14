/**
 * Lockstep-anchor guard.
 *
 * Every smooth-operator artifact — npm, NuGet, PyPI, crates.io — ships at ONE shared version, and
 * that version lives in exactly one place: `@smooai/smooth-operator` (`typescript/package.json`).
 * Changesets natively versions only npm packages, so `sync-versions.mjs` stamps that one number onto
 * every OTHER published manifest (the .NET csprojs, the Rust Cargo.tomls, python, go).
 *
 * The consequence people miss: **a changeset that does not name `@smooai/smooth-operator` cannot
 * republish any non-npm artifact.** And sitting one word away is `@smooai/smooth-operator-server` —
 * the TYPESCRIPT server on npm. For someone who just changed the .NET server, that name is the
 * intuitive pick and the wrong one.
 *
 * It failed silently: CI green, PR merged, release workflow succeeded, artifact never republished.
 * PR #348 (.NET file transfer) and #352 (.NET skill resolution) both landed on main and sat
 * unpublished until a downstream consumer read the NuGet two days later and filed a bug asking us to
 * build a feature we had already shipped. This guard turns that into a red check on the PR.
 *
 * Rule: if a PR touches a lockstep-stamped tree AND carries at least one changeset, then at least one
 * of those changesets must name the anchor.
 *
 * Deliberately conditioned on "carries a changeset". A docs- or test-only PR legitimately has none,
 * and failing those would train everyone to ignore the check. The bug being caught is not "no
 * changeset" — it is "a changeset that names the wrong package".
 */
import { readFileSync, readdirSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { dirname } from 'node:path';

export const ANCHOR = '@smooai/smooth-operator';

/**
 * The repo-relative directories whose contents are lockstep-stamped, derived from `sync-versions.mjs`'s
 * own target list so the two can never drift. A manifest at `dotnet/server/src/Foo.csproj` means the
 * `dotnet/server/src` tree is published under the anchor version.
 */
export function stampedTreesFrom(targets) {
    return [...new Set(targets.map((t) => dirname(t.name)))].sort();
}

/** Package names named in a changeset's frontmatter (`'pkg': minor`). */
export function packagesInChangeset(text) {
    const fence = text.indexOf('---', 3);
    const frontmatter = fence === -1 ? '' : text.slice(3, fence);
    return [...frontmatter.matchAll(/^\s*['"]([^'"]+)['"]\s*:/gm)].map((m) => m[1]);
}

/**
 * The pure decision. Returns `{ ok, touched, packages }` — `touched` is the stamped trees the diff
 * actually hit, so the failure message can name them instead of making the reader guess.
 */
export function evaluate({ changedFiles, changesets, stampedTrees }) {
    const touched = stampedTrees.filter((tree) => changedFiles.some((f) => f === tree || f.startsWith(`${tree}/`)));
    const packages = [...new Set(changesets.flatMap(packagesInChangeset))];
    // No changeset at all ⇒ not this guard's business (docs/test-only PRs are legitimate).
    if (touched.length === 0 || changesets.length === 0) {
        return { ok: true, touched, packages };
    }
    return { ok: packages.includes(ANCHOR), touched, packages };
}

async function main() {
    const root = new URL('..', import.meta.url);
    const base = process.argv[2] || process.env.ANCHOR_GUARD_BASE || 'origin/main';

    const changedFiles = execFileSync('git', ['diff', '--name-only', `${base}...HEAD`], {
        cwd: fileURLToPath(root),
        encoding: 'utf8',
    })
        .split('\n')
        .filter(Boolean);

    const dir = new URL('../.changeset/', import.meta.url);
    const changesets = readdirSync(dir)
        .filter((f) => f.endsWith('.md') && f !== 'README.md')
        .map((f) => readFileSync(new URL(f, dir), 'utf8'));

    // Imported, not re-listed — sync-versions.mjs stamps only when run directly.
    const { targets } = await import('./sync-versions.mjs');
    const { ok, touched, packages } = evaluate({ changedFiles, changesets, stampedTrees: stampedTreesFrom(targets) });

    if (ok) {
        console.log(`anchor-guard: ok (stamped trees touched: ${touched.join(', ') || 'none'})`);
        return;
    }

    console.error(
        [
            `anchor-guard: this PR changes lockstep-stamped tree(s) — ${touched.join(', ')} — but no changeset names ${ANCHOR}.`,
            ``,
            `Changesets name: ${packages.map((p) => `'${p}'`).join(', ') || '(none)'}`,
            ``,
            `Those trees are published as NuGet / PyPI / crates.io artifacts whose version is stamped from`,
            `${ANCHOR} (typescript/package.json) by scripts/sync-versions.mjs. A changeset that does not`,
            `name the anchor bumps nothing there, so the release "succeeds" and the artifact is never`,
            `republished — which is exactly how PR #348 and #352 shipped to main and stayed invisible to`,
            `consumers for two days.`,
            ``,
            `If you meant '@smooai/smooth-operator-server', note that is the TYPESCRIPT server on npm, not`,
            `the .NET one. Add ${ANCHOR} to your changeset (naming both is correct and common).`,
        ].join('\n'),
    );
    process.exitCode = 1;
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
    await main();
}
