/**
 * Env-gated selection of the {@link AgentExecutor} a turn runs on — the one place
 * a durable backend (ADR-030) plugs in.
 *
 * The TypeScript port of the Rust server's `runner.rs::turn_executor`. An injected
 * executor is used verbatim (the env var is not consulted); otherwise the default
 * is the engine's zero-infra {@link InProcessExecutor}, a verbatim delegation to
 * `SmoothAgent.runStream`, so a deployment that never opts in behaves exactly as
 * it did before the seam existed.
 *
 * The durable backend arrives as a **parameter**, never constructed here, so this
 * server compiles and ships with **no dependency on `@smooai/smooth-operator-temporal`**
 * (nor any Temporal SDK). A deployment that wants durable turns constructs a
 * `TemporalAgentExecutor` outside this package and injects it.
 */

import { InProcessExecutor } from '@smooai/smooth-operator-core';
import type { AgentExecutor } from '@smooai/smooth-operator-core';

/**
 * Env var a deployment sets to run turns on a durable backend instead of
 * in-process. Unset — the default — is the in-process executor. Canonical name,
 * identical across every server host (parity with the Rust server's
 * `DURABLE_EXECUTOR_ENV`).
 */
export const DURABLE_EXECUTOR_ENV = 'SMOOTH_AGENT_DURABLE_EXECUTOR';

/**
 * Whether the {@link DURABLE_EXECUTOR_ENV} value asks for durable execution.
 * Separated from the env read so the parse is testable without mutating
 * process-global state. Accepts `1` / `true` / `on` / `yes` (case- and
 * whitespace-insensitive); everything else — unset, empty, unrecognized — is off.
 */
export function durableRequested(value: string | undefined): boolean {
    if (value === undefined) return false;
    switch (value.trim().toLowerCase()) {
        case '1':
        case 'true':
        case 'on':
        case 'yes':
            return true;
        default:
            return false;
    }
}

/**
 * Resolve the executor a turn runs on.
 *
 * `injected` (from {@link TurnRunnerOptions.executor}) wins outright — that is the
 * seam a durable backend arrives through. With nothing injected this returns a
 * fresh {@link InProcessExecutor}. Asking for durable mode via the env var
 * *without* supplying an executor warns and falls back rather than silently
 * pretending the turn is durable — a turn a client believes will survive a
 * disconnect, but won't, is worse than no durable mode at all.
 */
export function turnExecutor(injected?: AgentExecutor, env: NodeJS.ProcessEnv = process.env): AgentExecutor {
    if (injected) return injected;
    if (durableRequested(env[DURABLE_EXECUTOR_ENV])) {
        console.warn(`[turnRunner] durable execution requested (${DURABLE_EXECUTOR_ENV}) but no executor was supplied on the turn; running the turn in-process`);
    }
    return new InProcessExecutor();
}
