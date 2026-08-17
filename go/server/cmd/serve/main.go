// Command serve runs the Go local-flavor smooth-operator server as a standalone
// process — the Go host, parity with the Rust/C#/Python/TS server binaries — so
// smooth-web (or any protocol client) can drive it over WebSocket.
//
// Env contract (shared with the sibling hosts):
//
//	SMOOTH_AGENT_BIND          host to listen on (default 127.0.0.1)
//	SMOOTH_AGENT_PORT          port to listen on (default 8787)
//	SMOOTH_AGENT_STORAGE       memory (default) | postgres — durable sessions + admin stores
//	SMOOTH_AGENT_DATABASE_URL  Postgres DSN for SMOOTH_AGENT_STORAGE=postgres
//	SMOOAI_GATEWAY_URL         OpenAI-compatible gateway base URL
//	SMOOAI_GATEWAY_KEY         gateway API key (absent → keyless; turns error cleanly)
//	SMOOTH_PERSONA             system prompt for the agent (optional)
//	SMOOTH_WORKSPACE           root the coding tools are confined to (default: cwd)
//	SMOOTH_NO_TOOLS            set to "1" to serve a chat-only agent (no coding tools)
//
// Aliases, still honored so existing deployments keep working:
//
//	SMOOTH_OPERATOR_BIND       combined host:port (this host's pre-parity name)
//	DATABASE_URL               read ONLY once SMOOTH_AGENT_STORAGE=postgres has
//	                           explicitly selected the backend, as in the Rust host
package main

import (
	"context"
	"log"
	"net"
	"os"
	"strings"

	core "github.com/SmooAI/smooth-operator-core/go/core"
	server "github.com/SmooAI/smooth-operator/go/server"
)

// Process defaults, shared with the Rust/Python/TS/.NET hosts. The port was 8793
// before the env-parity pass; every sibling host defaults to 8787, so a client
// pointed at the "smooth-operator port" now reaches whichever engine is running.
const (
	defaultHost = "127.0.0.1"
	defaultPort = "8787"
)

// resolveAddr builds the listen address from the environment, canonical-first.
//
// SMOOTH_AGENT_BIND is a bare host (matching the Rust host), but a combined
// "host:port" is accepted too, in which case its port applies. The legacy
// combined SMOOTH_OPERATOR_BIND is read first so anything canonical set
// alongside it overrides it.
func resolveAddr(get func(string) string) string {
	host, port := defaultHost, defaultPort

	if legacy := strings.TrimSpace(get("SMOOTH_OPERATOR_BIND")); legacy != "" {
		host, port = splitAddr(legacy, host, port)
	}
	if bind := strings.TrimSpace(get("SMOOTH_AGENT_BIND")); bind != "" {
		host, port = splitAddr(bind, host, port)
	}
	if p := strings.TrimSpace(get("SMOOTH_AGENT_PORT")); p != "" {
		port = p
	}
	return net.JoinHostPort(host, port)
}

// splitAddr reads value as either "host:port" or a bare host, keeping the passed
// fallbacks for whichever half it does not carry. net.SplitHostPort handles the
// bracketed IPv6 form, so "[::1]:8787" splits and a bare "::1" does not.
func splitAddr(value, host, port string) (string, string) {
	if h, p, err := net.SplitHostPort(value); err == nil {
		return h, p
	}
	return value, port
}

func main() {
	ctx := context.Background()
	addr := resolveAddr(os.Getenv)

	var opts []server.LocalOption

	// Storage backend. Unset/memory → nothing to install (the in-memory stores stay);
	// postgres → durable session + admin stores. A misconfigured durable backend is
	// fatal rather than a silent fall back to memory.
	storage, err := server.StorageOptionsFromEnv(ctx)
	if err != nil {
		log.Fatalf("storage: %v", err)
	}
	for _, opt := range storage {
		opts = append(opts, server.WithLocalServerOption(opt))
	}
	if len(storage) > 0 {
		log.Printf("durable storage enabled: SMOOTH_AGENT_STORAGE=%s", os.Getenv("SMOOTH_AGENT_STORAGE"))
	}

	if key := os.Getenv("SMOOAI_GATEWAY_KEY"); key != "" {
		opts = append(opts, server.WithLocalChatClient(core.NewGatewayClient(os.Getenv("SMOOAI_GATEWAY_URL"), key)))
	}
	if persona := os.Getenv("SMOOTH_PERSONA"); persona != "" {
		opts = append(opts, server.WithLocalServerOption(server.WithSystemPrompt(persona)))
	}

	// Give the agent a workspace-confined coding toolset (read/write/edit/list/grep/bash)
	// so it can actually edit files — without WithTools the local agent is chat-only and
	// replies "I don't have file editing tools" (th-82ad57). Confined to SMOOTH_WORKSPACE
	// (default: the process cwd, which is what the bench launches the server in).
	if os.Getenv("SMOOTH_NO_TOOLS") != "1" {
		workspace := os.Getenv("SMOOTH_WORKSPACE")
		if workspace == "" {
			if cwd, err := os.Getwd(); err == nil {
				workspace = cwd
			} else {
				workspace = "."
			}
		}
		opts = append(opts, server.WithLocalServerOption(server.WithTools(server.CodingTools(workspace))))
		log.Printf("coding tools enabled, confined to workspace: %s", workspace)
	}

	log.Printf("smooth-operator-server (Go, local flavor) listening on ws://%s/ws", addr)
	if err := server.ServeLocal(ctx, addr, opts...); err != nil {
		log.Fatalf("serve: %v", err)
	}
}
