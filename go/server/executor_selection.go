package server

import (
	"log/slog"
	"strings"

	core "github.com/SmooAI/smooth-operator-core/go/core"
)

// durableExecutorEnv is the env var a deployment sets to run turns on a durable
// backend (ADR-030) instead of in-process. Unset — the default — is the
// in-process executor, which is a verbatim delegation to SmoothAgent.RunStream,
// so a deployment that never sets this behaves exactly as it did before the seam
// existed. Matches the Rust server's DURABLE_EXECUTOR_ENV.
const durableExecutorEnv = "SMOOTH_AGENT_DURABLE_EXECUTOR"

// durableRequested reports whether the durableExecutorEnv value asks for durable
// execution. Split from the env read so the parse is testable without mutating
// process-global state (mirrors the Rust runner's durable_requested). The opt-in
// tokens match the Rust reference exactly: 1 / true / on / yes (case- and
// whitespace-insensitive); everything else — including empty and unrecognized —
// stays off.
func durableRequested(value string) bool {
	switch strings.ToLower(strings.TrimSpace(value)) {
	case "1", "true", "on", "yes":
		return true
	default:
		return false
	}
}

// turnExecutor picks the executor a turn runs on — the one place a durable backend
// plugs in (ADR-030).
//
// injected is the durable-backend slot (TurnRunner.executor): it is DEPENDENCY-
// INJECTED, deliberately a parameter rather than something this function
// constructs, so the Temporal package stays out of this server binary — the
// durable executor is built elsewhere and handed in. It is used only when the env
// explicitly opts into durable mode AND an executor was supplied; otherwise the
// zero-infra InProcessExecutor.
//
// Asking for durable mode via the env without supplying an executor warns and
// falls back rather than silently pretending the turn is durable — a turn a client
// believes will survive a disconnect, but won't, is worse than no durable mode at
// all. With durable mode off (the default), the injected slot is ignored and the
// in-process executor is a verbatim delegation to SmoothAgent.RunStream, so a
// default deployment behaves exactly as it did before the seam existed.
func turnExecutor(injected core.AgentExecutor, durableEnv string) core.AgentExecutor {
	if durableRequested(durableEnv) {
		if injected != nil {
			return injected
		}
		slog.Warn(
			"durable execution requested but no executor was supplied on the turn; running the turn in-process",
			"env", durableExecutorEnv,
		)
	}
	return core.NewInProcessExecutor()
}
