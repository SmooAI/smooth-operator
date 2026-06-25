#!/usr/bin/env node
/**
 * Lockstep version sync.
 *
 * Changesets natively versions only the npm package (`@smooai/smooth-operator`). This script
 * stamps that canonical version onto every OTHER published-package manifest in the repo, so all
 * smooth-operator artifacts ship at one shared version (the @smooai/config model). It runs
 * automatically as part of `version:bump` (right after `changeset version`), and can be run
 * standalone to re-align.
 *
 * Add a target here when a new language package becomes publishable from this repo (a
 * Cargo.toml, a pyproject.toml, etc.).
 */
import { readFileSync, writeFileSync } from 'node:fs';

const anchorUrl = new URL('../typescript/package.json', import.meta.url);
const version = JSON.parse(readFileSync(anchorUrl, 'utf8')).version;

/**
 * Stamp the anchor version onto a publishable Rust member Cargo.toml.
 *
 * Two kinds of version strings get the anchor:
 *   1. The crate's own `[package] version = "..."` — i.e. the FIRST `version = "..."`
 *      line in the file (it precedes `[dependencies]`).
 *   2. Intra-workspace dep version REQUIREMENTS on the sibling reference lib
 *      `smooai-smooth-operator` and its derived crates (…-ingestion, …-adapter-*,
 *      …-server). These appear inline as `… version = "..." }`.
 *
 * CRITICAL: the engine crate `smooai-smooth-operator-core` is published on its OWN
 * cadence (0.14) and must NEVER be re-stamped. So a dep line only qualifies if it
 * references a `smooai-smooth-operator*` name that is NOT exactly `-core`. The
 * workspace dep `smooth-operator = { package = "smooai-smooth-operator", … }` is keyed
 * by `smooth-operator` but its `package =` value identifies it as the reference lib —
 * we detect either the key or the `package = "..."` value.
 *
 * @param {string} text
 * @returns {string}
 */
function stampRustCargoToml(text) {
    const lines = text.split('\n');
    let packageVersionStamped = false;
    return lines
        .map((line) => {
            // 1. First `version = "..."` line = the [package] version.
            if (!packageVersionStamped && /^version = "[^"]*"\s*$/.test(line)) {
                packageVersionStamped = true;
                return line.replace(/^version = "[^"]*"/, `version = "${version}"`);
            }
            // 2. Intra-workspace dep entries on the reference lib (non-core).
            //    Match a `smooai-smooth-operator*` token (as a dep key or as a
            //    `package = "..."` value) that is NOT `-core`, on a line that also
            //    carries an inline `version = "..."` requirement.
            if (!/version = "/.test(line)) return line;
            const referencesRefLib = /smooai-smooth-operator(?!-core)[\w-]*/.test(line);
            if (!referencesRefLib) return line;
            return line.replace(/version = "[^"]*"/g, `version = "${version}"`);
        })
        .join('\n');
}

/**
 * Stamp the anchor version onto every WORKSPACE-MEMBER entry in `rust/Cargo.lock`.
 *
 * `changeset version` + the Cargo.toml stamps below bump each member's `[package]`
 * version, but the lockfile still pins the OLD versions — so `cargo build --locked`
 * (kind-deploy-smoke, and every release `cargo publish`) refuses to build until the
 * lock is re-synced. Rather than shell out to `cargo update` (which can't re-resolve
 * on a CI runner where the sibling `smooth-operator-core` path dep is absent), we
 * string-stamp the lock the same way we stamp the manifests: bump the `version` of
 * each `[[package]]` whose `name` is one of the lockstep-stamped published members
 * (`publishedRustCrates` — NOT `-core`, which ships on its own 0.14 cadence, and NOT
 * non-published crates like the lambda/examples that stay at 0.1.0). Workspace
 * members are referenced by name-only in the lock (unique versions, no checksums),
 * so the version line is the only change — exactly what `cargo update --workspace`
 * would produce.
 *
 * @param {string} text
 * @returns {string}
 */
function stampRustCargoLock(text) {
    const lines = text.split('\n');
    let currentName = null;
    return lines
        .map((line) => {
            const nameMatch = line.match(/^name = "([^"]*)"$/);
            if (nameMatch) {
                currentName = nameMatch[1];
                return line;
            }
            if (currentName && publishedRustCrates.has(currentName) && /^version = "[^"]*"\s*$/.test(line)) {
                return line.replace(/^version = "[^"]*"/, `version = "${version}"`);
            }
            return line;
        })
        .join('\n');
}

/** Publishable Rust members whose `[package]` version + ref-lib dep reqs are lockstep-stamped. */
const rustMembers = [
    'rust/smooth-operator/Cargo.toml',
    'rust/ingestion/Cargo.toml',
    'rust/adapters/in-memory/Cargo.toml',
    'rust/adapters/postgres/Cargo.toml',
    'rust/adapters/dynamodb/Cargo.toml',
    'rust/adapters/backplane-redis/Cargo.toml',
    'rust/adapters/backplane-nats/Cargo.toml',
    'rust/smooth-operator-server/Cargo.toml',
];

/**
 * The exact `[package] name`s of the lockstep-stamped published members (read from
 * their Cargo.toml). The lock stamp keys off THIS set rather than a broad
 * `smooai-smooth-operator*` name match, so it never bumps non-published workspace
 * crates (e.g. `smooai-smooth-operator-lambda`, the dev-support examples) that stay
 * at their own `0.1.0` — bumping those would make the lock mismatch their unstamped
 * Cargo.toml and break `--locked` exactly the way we're trying to prevent.
 */
const publishedRustCrates = new Set(
    rustMembers
        .map((rel) => {
            const text = readFileSync(new URL(`../${rel}`, import.meta.url), 'utf8');
            const m = text.match(/^name = "([^"]*)"/m);
            return m ? m[1] : null;
        })
        .filter(Boolean),
);

/** @type {{ name: string, url: URL, apply: (text: string) => string }[]} */
const targets = [
    // NOTE: the .NET Core package (SmooAI.SmoothOperator.Core) moved OUT of this
    // repo — commit 1f566ce ("repoint C# server to published NuGet Core 1.3.0 +
    // remove in-tree engine") deleted dotnet/core/src/SmooAI.SmoothOperator.Core.csproj
    // and the package is now versioned + published from the smooth-operator-core
    // repo. The remaining dotnet/ binding has no <Version> (pack-time versioned),
    // so there is no .csproj to stamp here. (Previously this target ENOENT-crashed
    // the release after the dotnet restructure.)
    {
        name: 'python/pyproject.toml',
        url: new URL('../python/pyproject.toml', import.meta.url),
        // Stamp the `[project] version`, NOT the `name`.
        apply: (text) => text.replace(/^version = "[^"]*"$/m, `version = "${version}"`),
    },
    {
        name: 'go/version.go',
        url: new URL('../go/version.go', import.meta.url),
        apply: (text) => text.replace(/const Version = "[^"]*"/, `const Version = "${version}"`),
    },
    // Rust: the workspace manifest carries the ref-lib dep version requirement, and
    // each publishable member carries its own [package] version + ref-lib dep reqs.
    {
        name: 'rust/Cargo.toml',
        url: new URL('../rust/Cargo.toml', import.meta.url),
        // ONLY the `smooth-operator = { package = "smooai-smooth-operator", … }` dep
        // req — leave `smooai-smooth-operator-core`'s 0.14 and unrelated deps alone.
        apply: (text) =>
            text
                .split('\n')
                .map((line) => {
                    if (!/version = "/.test(line)) return line;
                    if (!/smooai-smooth-operator(?!-core)[\w-]*/.test(line)) return line;
                    return line.replace(/version = "[^"]*"/g, `version = "${version}"`);
                })
                .join('\n'),
    },
    ...rustMembers.map((rel) => ({
        name: rel,
        url: new URL(`../${rel}`, import.meta.url),
        apply: stampRustCargoToml,
    })),
    // The lockfile MUST be re-synced after the [package] bumps above, or
    // `cargo build --locked` (kind-deploy-smoke + every `cargo publish`) fails.
    {
        name: 'rust/Cargo.lock',
        url: new URL('../rust/Cargo.lock', import.meta.url),
        apply: stampRustCargoLock,
    },
];

let changed = 0;
for (const target of targets) {
    const before = readFileSync(target.url, 'utf8');
    const after = target.apply(before);
    if (after !== before) {
        writeFileSync(target.url, after);
        changed++;
        console.log(`synced ${version} → ${target.name}`);
    } else {
        console.log(`already at ${version}: ${target.name}`);
    }
}

console.log(`version-sync: anchor @smooai/smooth-operator@${version}, ${changed} file(s) updated.`);
