package main

import "testing"

// The env-parity contract: every server implementation reads the canonical
// SMOOTH_AGENT_* names, and each one's pre-parity name keeps working as an
// alias so no existing deployment breaks. Canonical wins when both are set.
func TestResolveAddr(t *testing.T) {
	for _, tc := range []struct {
		name string
		env  map[string]string
		want string
	}{
		{"defaults", nil, "127.0.0.1:8787"},
		{"canonical", map[string]string{"SMOOTH_AGENT_BIND": "0.0.0.0", "SMOOTH_AGENT_PORT": "9000"}, "0.0.0.0:9000"},
		{"alias", map[string]string{"SMOOTH_OPERATOR_BIND": "0.0.0.0:8793"}, "0.0.0.0:8793"},
		{
			"canonical wins over alias",
			map[string]string{"SMOOTH_OPERATOR_BIND": "10.0.0.1:8793", "SMOOTH_AGENT_BIND": "0.0.0.0", "SMOOTH_AGENT_PORT": "9000"},
			"0.0.0.0:9000",
		},
		// A canonical host with no canonical port leaves the alias's port in place,
		// so half-migrated deployments keep the port they were already serving on.
		{
			"canonical host keeps alias port",
			map[string]string{"SMOOTH_OPERATOR_BIND": "10.0.0.1:8793", "SMOOTH_AGENT_BIND": "0.0.0.0"},
			"0.0.0.0:8793",
		},
		// SMOOTH_AGENT_BIND is documented as a bare host, but accepting host:port
		// costs nothing and is what someone migrating off the combined name types.
		{"canonical accepts combined", map[string]string{"SMOOTH_AGENT_BIND": "0.0.0.0:9100"}, "0.0.0.0:9100"},
		{"ipv6 host is bracketed", map[string]string{"SMOOTH_AGENT_BIND": "::1"}, "[::1]:8787"},
		{"blank falls through to default", map[string]string{"SMOOTH_AGENT_BIND": "  "}, "127.0.0.1:8787"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			got := resolveAddr(func(k string) string { return tc.env[k] })
			if got != tc.want {
				t.Fatalf("resolveAddr = %q, want %q", got, tc.want)
			}
		})
	}
}
