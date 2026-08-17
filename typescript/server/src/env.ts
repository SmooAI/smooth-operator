/**
 * The env contract this host reads, resolved canonical-first.
 *
 * Every server implementation (Rust, Go, Python, TypeScript, .NET) reads the same
 * canonical `SMOOTH_AGENT_*` names. Each host's PRE-PARITY name is kept as an alias
 * so no existing deployment breaks; the canonical name wins when both are set.
 *
 * | setting | canonical             | alias (this host)      |
 * | ------- | --------------------- | ---------------------- |
 * | host    | `SMOOTH_AGENT_BIND`   | `SMOOTH_OPERATOR_HOST` |
 * | port    | `SMOOTH_AGENT_PORT`   | `SMOOTH_OPERATOR_PORT` |
 * | model   | `SMOOTH_AGENT_MODEL`  | `SMOOAI_MODEL`         |
 *
 * The gateway triple (`SMOOAI_GATEWAY_URL` / `SMOOAI_GATEWAY_KEY`) keeps its
 * `SMOOAI_*` spelling — that name is already identical across all five hosts and
 * is the wider SmooAI gateway contract, not this server's own config surface.
 *
 * Lives apart from `main.ts` because that module boots a server on import.
 */

/** Process defaults, shared with the sibling hosts. */
export const DEFAULT_HOST = '127.0.0.1';
export const DEFAULT_PORT = 8787;
export const DEFAULT_MODEL = 'claude-haiku-4-5';

/** The first of `keys` with a non-blank value, or `undefined`. */
function firstSet(env: NodeJS.ProcessEnv, keys: string[]): string | undefined {
    for (const key of keys) {
        const value = env[key]?.trim();
        if (value) return value;
    }
    return undefined;
}

/** Bind host + port. A non-numeric port falls back to the default rather than `NaN`. */
export function resolveBind(env: NodeJS.ProcessEnv = process.env): { host: string; port: number } {
    const rawPort = firstSet(env, ['SMOOTH_AGENT_PORT', 'SMOOTH_OPERATOR_PORT']);
    const port = Number(rawPort);
    return {
        host: firstSet(env, ['SMOOTH_AGENT_BIND', 'SMOOTH_OPERATOR_HOST']) ?? DEFAULT_HOST,
        port: Number.isInteger(port) && port > 0 ? port : DEFAULT_PORT,
    };
}

/** The model id turns run against. */
export function resolveModel(env: NodeJS.ProcessEnv = process.env): string {
    return firstSet(env, ['SMOOTH_AGENT_MODEL', 'SMOOAI_MODEL']) ?? DEFAULT_MODEL;
}
