#!/usr/bin/env node
/**
 * `create-smooth-extension` — scaffold a new SEP (Smooth Extension Protocol)
 * extension. `npm create smooth-extension <name> -- --template <kind>`.
 *
 * Deliberately tiny: Node fs + one arg parse + literal template files under
 * `templates/<kind>/` (plus shared files under `templates/_shared/`) with a
 * `__NAME__` placeholder. No generator DSL, no prompt library — plain readline
 * only when a required answer is missing on a TTY.
 */
import { cpSync, existsSync, mkdirSync, readdirSync, readFileSync, renameSync, statSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { createInterface } from 'node:readline/promises';
import { fileURLToPath } from 'node:url';

const TEMPLATES = ['tool', 'permission-gate', 'command', 'provider-less', 'provider'] as const;
type Template = (typeof TEMPLATES)[number];

const __dirname = dirname(fileURLToPath(import.meta.url));
/** templates/ ships next to dist/ in the published package (files: [dist, templates]). */
const TEMPLATES_DIR = join(__dirname, '..', 'templates');

interface Options {
    name?: string;
    template?: string;
}

/** Parse `<name>` positional + `--template <kind>` / `-t <kind>`. */
export function parseArgs(argv: string[]): Options {
    const opts: Options = {};
    for (let i = 0; i < argv.length; i++) {
        const arg = argv[i]!;
        if (arg === '--template' || arg === '-t') {
            opts.template = argv[++i];
        } else if (arg.startsWith('--template=')) {
            opts.template = arg.slice('--template='.length);
        } else if (!arg.startsWith('-') && opts.name === undefined) {
            opts.name = arg;
        }
    }
    return opts;
}

/** A directory name is usable if it doesn't exist or is empty. */
function isUsableTarget(dir: string): boolean {
    return !existsSync(dir) || (statSync(dir).isDirectory() && readdirSync(dir).length === 0);
}

/** Recursively copy `from` into `to`, substituting `__NAME__` in every file and
 * renaming the scaffold-safe `_gitignore` → `.gitignore`. */
function copyTemplate(from: string, to: string, name: string): void {
    for (const entry of readdirSync(from, { withFileTypes: true })) {
        const src = join(from, entry.name);
        const destName = entry.name === '_gitignore' ? '.gitignore' : entry.name;
        const dest = join(to, destName);
        if (entry.isDirectory()) {
            mkdirSync(dest, { recursive: true });
            copyTemplate(src, dest, name);
        } else {
            const contents = readFileSync(src, 'utf8').replaceAll('__NAME__', name);
            mkdirSync(dirname(dest), { recursive: true });
            writeFileSync(dest, contents);
        }
    }
}

/** Scaffold `template` into `<targetRoot>` for an extension called `name`.
 * Shared files land first, then the template's own files override. */
export function scaffold(template: Template, name: string, targetDir: string): void {
    mkdirSync(targetDir, { recursive: true });
    copyTemplate(join(TEMPLATES_DIR, '_shared'), targetDir, name);
    copyTemplate(join(TEMPLATES_DIR, template), targetDir, name);
    // `_package.json` avoids npm's nested-package.json publish quirks; land it as package.json.
    const staged = join(targetDir, '_package.json');
    if (existsSync(staged)) renameSync(staged, join(targetDir, 'package.json'));
}

async function prompt(question: string, fallback: string): Promise<string> {
    if (!process.stdin.isTTY) return fallback;
    const rl = createInterface({ input: process.stdin, output: process.stdout });
    try {
        const answer = (await rl.question(question)).trim();
        return answer || fallback;
    } finally {
        rl.close();
    }
}

export async function main(argv: string[]): Promise<number> {
    const opts = parseArgs(argv);

    const name = opts.name ?? (await prompt('Extension name: ', 'my-extension'));
    if (!/^[@a-z0-9][\w./@-]*$/i.test(name)) {
        console.error(`Invalid extension name: ${JSON.stringify(name)}`);
        return 1;
    }

    let template = opts.template ?? (await prompt(`Template [${TEMPLATES.join(' | ')}] (tool): `, 'tool'));
    if (!TEMPLATES.includes(template as Template)) {
        console.error(`Unknown template ${JSON.stringify(template)}. Choose one of: ${TEMPLATES.join(', ')}`);
        return 1;
    }
    template = template as Template;

    const targetDir = resolve(process.cwd(), name);
    if (!isUsableTarget(targetDir)) {
        console.error(`Target directory is not empty: ${targetDir}`);
        return 1;
    }

    scaffold(template as Template, name, targetDir);

    console.log(`\nScaffolded ${template} extension "${name}" in ${targetDir}\n`);
    console.log('Next steps:');
    console.log(`  cd ${name}`);
    console.log('  pnpm install');
    console.log('  pnpm build');
    console.log('  pnpm test        # unit test + SEP conformance\n');
    return 0;
}

// Run when invoked as the bin (not when imported by a test).
if (import.meta.url === `file://${process.argv[1]}`) {
    main(process.argv.slice(2)).then(
        (code) => process.exit(code),
        (err) => {
            console.error(err);
            process.exit(1);
        },
    );
}
