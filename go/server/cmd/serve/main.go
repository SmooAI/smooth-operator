// Command serve runs the Go local-flavor smooth-operator server as a standalone
// process — the Go host, parity with the Rust/C#/Python/TS server binaries — so
// smooth-web (or any protocol client) can drive it over WebSocket.
//
// Env contract (shared with the sibling hosts):
//
//	SMOOTH_OPERATOR_BIND   host:port to listen on (default 127.0.0.1:8793)
//	SMOOAI_GATEWAY_URL     OpenAI-compatible gateway base URL
//	SMOOAI_GATEWAY_KEY     gateway API key (absent → keyless; turns error cleanly)
//	SMOOTH_PERSONA         system prompt for the agent (optional)
//	SMOOTH_WORKSPACE       root the coding tools are confined to (default: cwd)
//	SMOOTH_NO_TOOLS        set to "1" to serve a chat-only agent (no coding tools)
package main

import (
	"context"
	"log"
	"os"

	core "github.com/SmooAI/smooth-operator-core/go/core"
	server "github.com/SmooAI/smooth-operator/go/server"
)

func main() {
	addr := os.Getenv("SMOOTH_OPERATOR_BIND")
	if addr == "" {
		addr = "127.0.0.1:8793"
	}

	var opts []server.LocalOption
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
	if err := server.ServeLocal(context.Background(), addr, opts...); err != nil {
		log.Fatalf("serve: %v", err)
	}
}
