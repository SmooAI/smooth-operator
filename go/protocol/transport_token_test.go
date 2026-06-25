package protocol

import (
	"context"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strings"
	"testing"
	"time"

	"github.com/coder/websocket"
)

// TestWebSocketTransportTokenInDialURL covers the URL-merge logic: a configured
// Token must land in the dial URL's `token` query param (url-escaped), an existing
// query must be preserved, and an empty Token must leave the URL byte-for-byte
// unchanged.
func TestWebSocketTransportTokenInDialURL(t *testing.T) {
	tests := []struct {
		name      string
		url       string
		token     string
		wantToken string // expected value of the `token` query param ("" => absent)
		wantPairs map[string]string
		wantExact string // when set, dialURL must equal this exactly
	}{
		{
			name:      "token added",
			url:       "wss://example.test/ws",
			token:     "secret123",
			wantToken: "secret123",
		},
		{
			name:      "existing query preserved",
			url:       "wss://example.test/ws?foo=bar",
			token:     "secret123",
			wantToken: "secret123",
			wantPairs: map[string]string{"foo": "bar"},
		},
		{
			name:      "token is url-escaped",
			url:       "wss://example.test/ws",
			token:     "a b/c+d=e",
			wantToken: "a b/c+d=e",
		},
		{
			name:      "empty token leaves url unchanged",
			url:       "wss://example.test/ws?foo=bar",
			token:     "",
			wantExact: "wss://example.test/ws?foo=bar",
		},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			tr := NewWebSocketTransportWithOptions(tc.url, WebSocketOptions{Token: tc.token})
			got := tr.dialURL()

			if tc.wantExact != "" {
				if got != tc.wantExact {
					t.Fatalf("dialURL() = %q, want unchanged %q", got, tc.wantExact)
				}
				return
			}

			u, err := url.Parse(got)
			if err != nil {
				t.Fatalf("dialURL() produced unparseable URL %q: %v", got, err)
			}
			q := u.Query()
			if q.Get("token") != tc.wantToken {
				t.Errorf("token = %q, want %q (dialURL=%q)", q.Get("token"), tc.wantToken, got)
			}
			for k, v := range tc.wantPairs {
				if q.Get(k) != v {
					t.Errorf("query %q = %q, want %q (dialURL=%q)", k, q.Get(k), v, got)
				}
			}
		})
	}
}

// TestWebSocketTransportTokenOnConnect proves the token reaches the wire: a real
// WebSocket server records the request target it was dialed with, and we assert the
// `token` (and any pre-existing query) shows up there.
func TestWebSocketTransportTokenOnConnect(t *testing.T) {
	gotURI := make(chan string, 1)
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotURI <- r.URL.RequestURI()
		conn, err := websocket.Accept(w, r, nil)
		if err != nil {
			return
		}
		conn.Close(websocket.StatusNormalClosure, "done")
	}))
	defer srv.Close()

	wsURL := "ws" + strings.TrimPrefix(srv.URL, "http") + "/ws?foo=bar"

	tr := NewWebSocketTransportWithOptions(wsURL, WebSocketOptions{Token: "secret123"})
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if err := tr.Connect(ctx); err != nil {
		t.Fatalf("Connect: %v", err)
	}
	defer tr.Close()

	select {
	case uri := <-gotURI:
		u, err := url.ParseRequestURI(uri)
		if err != nil {
			t.Fatalf("server request URI %q unparseable: %v", uri, err)
		}
		q := u.Query()
		if q.Get("token") != "secret123" {
			t.Errorf("server saw token=%q, want %q (uri=%q)", q.Get("token"), "secret123", uri)
		}
		if q.Get("foo") != "bar" {
			t.Errorf("server saw foo=%q, want %q — existing query not preserved (uri=%q)", q.Get("foo"), "bar", uri)
		}
	case <-ctx.Done():
		t.Fatal("server never received a connection")
	}
}

// TestWebSocketTransportDefaultUnchanged guards the additive contract: the original
// NewWebSocketTransport constructor (no token) dials the URL verbatim.
func TestWebSocketTransportDefaultUnchanged(t *testing.T) {
	const raw = "wss://example.test/ws?foo=bar"
	tr := NewWebSocketTransport(raw, nil)
	if got := tr.dialURL(); got != raw {
		t.Errorf("dialURL() = %q, want unchanged %q", got, raw)
	}
}
