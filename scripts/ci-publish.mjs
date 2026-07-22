#!/usr/bin/env node
/**
 * CI publish orchestrator — runs as the changesets/action `publish:` step (release.yml),
 * i.e. AFTER the "🦋 New version release" PR merges. Publishes EVERY polyglot
 * smooth-operator artifact at the ONE lockstep version, to every registry:
 *
 *   • npm       → @smooai/smooth-operator          (via `changeset publish`)
 *   • NuGet     → SmooAI.SmoothOperator.Server{,.AspNetCore,.Postgres} (dotnet pack + nuget push)
 *   • PyPI      → smooai-smooth-operator (client)    (uv build + twine upload)
 *                 smooai-smooth-operator-server
 *   • crates.io → smooai-smooth-operator + ingestion + adapters + server
 *
 * Design goals (mirrors ~/dev/smooai/smooth/scripts/ci-publish.mjs):
 *   1. IDEMPOTENT — every registry is existence-checked first (npm/nuget/pypi/crates
 *      HTTP index) and skipped if the version is already live. Re-running a release,
 *      or a partial failure retry, never errors on "already published". Publishing is
 *      IRREVERSIBLE (npm 72h window; NuGet/PyPI/crates.io versions can NEVER be reused),
 *      so skip-if-exists is the safety net.
 *   2. LOCKSTEP — sync-versions runs FIRST and fails loudly if any manifest anchor is
 *      missing, so we never publish a mismatched set.
 *   3. FAIL-CLOSED but COMPLETE — one registry's failure does NOT skip the others (each
 *      is attempted), but ANY hard failure makes the whole script exit non-zero so the
 *      release stays visibly red.
 *   4. DRY RUN — `DRY_RUN=1` (or `--dry-run`) packs/builds + runs existence checks but
 *      pushes NOTHING and needs NO tokens. Use it to validate the pipeline locally.
 *
 * Tokens (real publish only; via release.yml step env):
 *   NPM: NODE_AUTH_TOKEN/NPM_TOKEN · NuGet: SMOOAI_NUGET_API_KEY
 *   PyPI: SMOOAI_PYPI_TOKEN · crates.io: CARGO_REGISTRY_TOKEN (SMOOAI_CARGO_REGISTRY_TOKEN)
 */
import { execFileSync } from 'node:child_process';
import { readFileSync, mkdtempSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import https from 'node:https';
import process from 'node:process';

const scriptDir = dirname(fileURLToPath(import.meta.url));
const root = resolve(scriptDir, '..');

const DRY_RUN = process.env.DRY_RUN === '1' || process.env.DRY_RUN === 'true' || process.argv.includes('--dry-run');

const version = JSON.parse(readFileSync(resolve(root, 'typescript/package.json'), 'utf8')).version;
if (!version) {
    console.error('Unable to read anchor version from typescript/package.json');
    process.exit(1);
}

// crates.io publish order: a crate must be on the index before its dependents can
// publish. Ref lib → ingestion → adapters → server. (Matches release.yml; the
// non-published lambda + examples stay off this list.)
const CRATES_ORDER = [
    'smooai-smooth-operator',
    'smooai-smooth-operator-ingestion',
    'smooai-smooth-operator-adapter-memory',
    'smooai-smooth-operator-adapter-backplane-redis',
    'smooai-smooth-operator-adapter-backplane-nats',
    'smooai-smooth-operator-adapter-postgres',
    'smooai-smooth-operator-adapter-dynamodb',
    'smooai-smooth-operator-server',
];

// NuGet packages published from this repo (PackageId → csproj). Server is the base
// package; AspNetCore (WS host) and Postgres (durable store) are add-ons that depend
// on it via ProjectReference (packed as a NuGet dependency). All three ship at the
// one lockstep version.
const NUGET_PACKAGES = [
    { id: 'SmooAI.SmoothOperator.Server', csproj: 'dotnet/server/src/SmooAI.SmoothOperator.Server.csproj' },
    { id: 'SmooAI.SmoothOperator.Server.AspNetCore', csproj: 'dotnet/server/aspnetcore/SmooAI.SmoothOperator.Server.AspNetCore.csproj' },
    { id: 'SmooAI.SmoothOperator.Server.Postgres', csproj: 'dotnet/server/postgres/src/SmooAI.SmoothOperator.Server.Postgres.csproj' },
];
// PyPI dists published from this repo (project dir → distribution name).
const PYPI_PROJECTS = [
    { dir: 'python', name: 'smooai-smooth-operator' },
    { dir: 'python/server', name: 'smooai-smooth-operator-server' },
];

function run(cmd, args, opts = {}) {
    console.log(`\n> ${cmd} ${args.join(' ')}`);
    return execFileSync(cmd, args, { stdio: 'inherit', cwd: root, ...opts });
}

/** GET a URL, resolve { status, body }. Never rejects — network failures resolve status 0. */
function httpGet(url) {
    return new Promise((res) => {
        https
            .get(url, { headers: { 'user-agent': 'smooth-operator-ci-publish' } }, (r) => {
                let body = '';
                r.setEncoding('utf8');
                r.on('data', (c) => (body += c));
                r.on('end', () => res({ status: r.statusCode ?? 0, body }));
            })
            .on('error', (err) => {
                console.warn(`  (existence check network error for ${url}: ${err.message})`);
                res({ status: 0, body: '' });
            });
    });
}

// --- existence checks (idempotency) ------------------------------------------

async function npmHasVersion(pkg, ver) {
    const { status, body } = await httpGet(`https://registry.npmjs.org/${pkg.replace('/', '%2f')}/${ver}`);
    return status === 200 && body.includes(`"version"`);
}

async function nugetHasVersion(id, ver) {
    // NuGet v3 flat-container index lists all published versions of a package.
    const { status, body } = await httpGet(`https://api.nuget.org/v3-flatcontainer/${id.toLowerCase()}/index.json`);
    if (status !== 200) return false;
    try {
        return (JSON.parse(body).versions ?? []).includes(ver);
    } catch {
        return false;
    }
}

async function pypiHasVersion(name, ver) {
    // pypi returns 200 for a published (name, version), 404 otherwise.
    const { status } = await httpGet(`https://pypi.org/pypi/${name}/${ver}/json`);
    return status === 200;
}

// crates.io sparse-index path layout (len 1→1/x, 2→2/x, 3→3/c/x, ≥4→ab/cd/name).
function cratesSparsePath(crate) {
    if (crate.length === 1) return `1/${crate}`;
    if (crate.length === 2) return `2/${crate}`;
    if (crate.length === 3) return `3/${crate[0]}/${crate}`;
    return `${crate.slice(0, 2)}/${crate.slice(2, 4)}/${crate}`;
}

async function cratesHasVersion(crate, ver) {
    const { status, body } = await httpGet(`https://index.crates.io/${cratesSparsePath(crate)}`);
    if (status === 404) return false;
    if (status !== 200) return false; // unknown → let cargo reject cleanly
    return body
        .split('\n')
        .map((l) => l.trim())
        .filter(Boolean)
        .some((line) => {
            try {
                return JSON.parse(line).vers === ver;
            } catch {
                return false;
            }
        });
}

// --- per-registry publishers -------------------------------------------------

async function publishNpm() {
    // Build first (task: keep the build step inside the script), then publish. In the
    // changesets/action flow, `changeset publish`'s stdout drives GitHub-release
    // creation, so we inherit stdio. changeset publish is itself idempotent (skips any
    // package version already on npm), so no explicit skip needed.
    run('pnpm', [
        '--filter',
        '@smooai/smooth-operator',
        '--filter',
        '@smooai/smooth-extension-sdk',
        '--filter',
        '@smooai/create-smooth-extension',
        'build',
    ]);
    if (DRY_RUN) {
        console.log('[npm] dry-run: pnpm publish --dry-run (no upload)');
        run('pnpm', ['--filter', '@smooai/smooth-operator', 'publish', '--dry-run', '--no-git-checks', '--access', 'public']);
        return;
    }
    run('pnpm', ['changeset', 'publish']);
}

async function publishNuget() {
    // Pack every not-yet-published package into one dir, then a single push uploads
    // them all. Each is existence-checked so a partial-failure re-run skips live ones.
    const out = mkdtempSync(resolve(tmpdir(), 'so-nupkg-'));
    let packed = 0;
    for (const { id, csproj } of NUGET_PACKAGES) {
        if (await nugetHasVersion(id, version)) {
            console.log(`[nuget] skip: ${id} ${version} already on nuget.org`);
            continue;
        }
        run('dotnet', ['pack', csproj, '-c', 'Release', '-o', out, `-p:Version=${version}`]);
        packed++;
    }
    if (packed === 0) {
        console.log(`[nuget] nothing to publish — all packages already at ${version}`);
        return;
    }
    if (DRY_RUN) {
        console.log(`[nuget] dry-run: packed ${packed} package(s) @ ${version} → ${out} (no push)`);
        return;
    }
    const key = process.env.SMOOAI_NUGET_API_KEY;
    if (!key) throw new Error('[nuget] SMOOAI_NUGET_API_KEY is not set');
    // --skip-duplicate is belt-and-suspenders on top of the existence checks above.
    run('dotnet', [
        'nuget',
        'push',
        `${out}/*.nupkg`,
        '--api-key',
        key,
        '--source',
        'https://api.nuget.org/v3/index.json',
        '--skip-duplicate',
    ]);
}

async function publishPypi() {
    for (const { dir, name } of PYPI_PROJECTS) {
        if (await pypiHasVersion(name, version)) {
            console.log(`[pypi] skip: ${name} ${version} already on pypi.org`);
            continue;
        }
        const cwd = resolve(root, dir);
        run('uv', ['build', '--out-dir', 'dist'], { cwd });
        if (DRY_RUN) {
            console.log(`[pypi] dry-run: built ${name} ${version} → ${dir}/dist (no upload)`);
            continue;
        }
        const token = process.env.SMOOAI_PYPI_TOKEN;
        if (!token) throw new Error('[pypi] SMOOAI_PYPI_TOKEN is not set');
        // twine --skip-existing = idempotent on top of the existence check.
        run('uvx', ['twine', 'upload', '--skip-existing', 'dist/*'], {
            cwd,
            env: { ...process.env, TWINE_USERNAME: '__token__', TWINE_PASSWORD: token },
        });
    }
}

function stripLocalCorePathDep() {
    // Defensive: if a dev re-added a sibling-repo `path =` on the core dep for local
    // two-repo work, `cargo publish` can't resolve it on a CI runner. Rewrite it to the
    // registry version form (publish-time only; --allow-dirty keeps it uncommitted).
    const cargoToml = resolve(root, 'rust/Cargo.toml');
    const before = readFileSync(cargoToml, 'utf8');
    const after = before.replace(/smooai-smooth-operator-core = \{ path = "[^"]*", version = "([^"]*)" \}/, 'smooai-smooth-operator-core = "$1"');
    if (after !== before && !DRY_RUN) {
        console.log('[crates] stripped local core `path =` dep for publish');
        // eslint-disable-next-line no-undef
        run('bash', ['-c', `cat > rust/Cargo.toml <<'SMOOEOF'\n${after}\nSMOOEOF`]);
    }
}

function sleep(ms) {
    return new Promise((r) => setTimeout(r, ms));
}

async function publishCrates() {
    stripLocalCorePathDep();
    for (const crate of CRATES_ORDER) {
        if (await cratesHasVersion(crate, version)) {
            console.log(`[crates] skip: ${crate} ${version} already on crates.io`);
            continue;
        }
        if (DRY_RUN) {
            console.log(`[crates] dry-run: cargo package ${crate} (validate, no publish)`);
            run('cargo', ['package', '-p', crate, '--no-verify', '--allow-dirty', '--manifest-path', 'rust/Cargo.toml']);
            continue;
        }
        // Retry loop tolerates crates.io index-propagation lag between dependent crates;
        // "already exists/uploaded" is treated as success (idempotent re-runs).
        const max = 6;
        let published = false;
        for (let attempt = 1; attempt <= max && !published; attempt++) {
            try {
                console.log(`[crates] publish ${crate} ${version} (attempt ${attempt}/${max})`);
                run('cargo', ['publish', '-p', crate, '--no-verify', '--allow-dirty', '--manifest-path', 'rust/Cargo.toml']);
                published = true;
            } catch (err) {
                if (await cratesHasVersion(crate, version)) {
                    console.log(`[crates] ${crate} ${version} now on index — treating as success`);
                    published = true;
                    break;
                }
                if (attempt === max) throw err;
                console.log('[crates] failed; waiting 30s for index propagation before retry...');
                await sleep(30_000);
            }
        }
    }
}

// --- orchestration -----------------------------------------------------------

const REGISTRIES = [
    { name: 'npm', run: publishNpm },
    { name: 'nuget', run: publishNuget },
    { name: 'pypi', run: publishPypi },
    { name: 'crates.io', run: publishCrates },
];

(async () => {
    console.log(`\n=== ci-publish: smooth-operator @ ${version} ${DRY_RUN ? '(DRY RUN — no uploads)' : '(LIVE)'} ===`);

    // 1. Lockstep guard — stamps every manifest and throws if an anchor is missing.
    console.log('\n--- sync-versions (lockstep guard) ---');
    run('node', ['scripts/sync-versions.mjs']);

    // 2. Attempt every registry; collect failures so one language can't silently
    //    skip another, but exit non-zero if any hard-failed.
    const failures = [];
    for (const reg of REGISTRIES) {
        console.log(`\n=== [${reg.name}] ===`);
        try {
            await reg.run();
            console.log(`[${reg.name}] done`);
        } catch (err) {
            console.error(`[${reg.name}] FAILED: ${err?.message ?? err}`);
            failures.push(reg.name);
        }
    }

    if (failures.length > 0) {
        console.error(`\nci-publish: ${failures.length} registry(ies) failed: ${failures.join(', ')}`);
        process.exit(1);
    }
    console.log(`\nci-publish: all registries ${DRY_RUN ? 'validated (dry run)' : 'published'} @ ${version}.`);
})().catch((err) => {
    console.error(err);
    process.exit(1);
});
