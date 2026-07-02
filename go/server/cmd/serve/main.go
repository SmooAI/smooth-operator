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

	log.Printf("smooth-operator-server (Go, local flavor) listening on ws://%s/ws", addr)
	if err := server.ServeLocal(context.Background(), addr, opts...); err != nil {
		log.Fatalf("serve: %v", err)
	}
}
