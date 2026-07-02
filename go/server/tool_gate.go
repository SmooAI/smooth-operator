package server

import (
	"context"

	core "github.com/SmooAI/smooth-operator-core/go/core"
)

// SMOODEV-590 — per-agent tool auth gating + per-tool config delivery, applied at
// tool-execution time. Mirrors the monorepo reference
// (packages/backend/src/ai/graphs/general-agent/nodes/tool-execution.ts auth gate +
// packages/backend/src/ai/tools/registry.ts config delivery).

// SessionAuthenticator reports whether a conversation's end user has completed identity
// verification (e.g. OTP). The reference server ships NONE — resolution is fail-closed
// (unauthenticated) so end_user-gated tools on a public agent are blocked until a host
// wires real verification behind this seam. SMOODEV-590.
type SessionAuthenticator interface {
	IsAuthenticated(ctx context.Context, conversationID string) (bool, error)
}

// toolConfigArgKey is the reserved args key a tool's per-agent enabledTools `config` map is
// delivered under at execution — kept separate from the model's call args (mirrors the
// reference's ToolContext.toolSpecificConfig field).
// ponytail: a namespaced arg, not a ToolContext/createTool seam the static reference server
// doesn't have — a real host that builds tools per-turn injects config at construction.
const toolConfigArgKey = "__toolConfig"

// gatedTool wraps a tool with the agent's auth gate + per-tool config delivery. Execute
// enforces the gate (blocking with a tool-result message the model sees, never executing
// the wrapped tool) then delivers the config. Name/Description/Parameters are promoted
// from the embedded tool, so the model sees an unchanged tool.
type gatedTool struct {
	core.Tool
	authLevel      string // "" / "none" / "end_user" / "admin" (from enabledTools entry)
	supportsAuth   bool   // the tool declares supportsAuthRequirement (server-side flag)
	visibility     string // "" / "public" / "internal" (non-"internal" ⇒ public)
	authenticator  SessionAuthenticator
	conversationID string
	config         map[string]any
}

func (g gatedTool) Execute(ctx context.Context, args map[string]any) (string, error) {
	// Auth gate — only when the tool declares auth support AND a non-none level is set.
	if g.supportsAuth && g.authLevel != "" && g.authLevel != "none" {
		if g.visibility != "internal" { // public agent
			if g.authLevel == "admin" {
				// admin-level tools are only available on internal agents.
				return "Tool '" + g.Name() + "' requires admin authentication and is not available on public-facing agents.", nil
			}
			// end_user on a public agent needs verified identity (OTP behind the seam).
			authed := false
			if g.authenticator != nil {
				if ok, err := g.authenticator.IsAuthenticated(ctx, g.conversationID); err == nil {
					authed = ok
				}
			}
			if !authed {
				return "I need to verify your identity before I can use " + g.Name() + ". Please verify with a one-time code.", nil
			}
		}
		// internal agent → end_user/admin auth auto-satisfied by the authenticated session.
	}

	// Deliver the per-tool config to the tool at execution (separate namespaced key).
	if len(g.config) > 0 {
		if args == nil {
			args = map[string]any{}
		}
		args[toolConfigArgKey] = g.config
	}
	return g.Tool.Execute(ctx, args)
}

// gateTools wraps the turn's tools with the agent's auth gate + per-tool config for the
// conversation. A tool is wrapped only when it has something to enforce/deliver (an
// auth-gated entry or a config map); everything else passes through unchanged, so an
// un-configured agent's tools are byte-for-byte identical. authRequiringTools is the set
// of tool names that declare supportsAuthRequirement.
func gateTools(tools []core.Tool, cfg *AgentConfig, authRequiringTools map[string]bool, auth SessionAuthenticator, conversationID string) []core.Tool {
	if cfg == nil || len(cfg.EnabledTools) == 0 {
		return tools
	}
	byID := make(map[string]EnabledTool, len(cfg.EnabledTools))
	for _, e := range cfg.EnabledTools {
		byID[e.ToolID] = e
	}
	out := make([]core.Tool, len(tools))
	for i, t := range tools {
		entry, ok := byID[t.Name()]
		gated := ok && authRequiringTools[t.Name()] && entry.AuthLevel != "" && entry.AuthLevel != "none"
		hasConfig := ok && len(entry.Config) > 0
		if !gated && !hasConfig {
			out[i] = t
			continue
		}
		out[i] = gatedTool{
			Tool:           t,
			authLevel:      entry.AuthLevel,
			supportsAuth:   authRequiringTools[t.Name()],
			visibility:     cfg.Visibility,
			authenticator:  auth,
			conversationID: conversationID,
			config:         entry.Config,
		}
	}
	return out
}
