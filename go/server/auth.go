package server

import (
	"crypto/hmac"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"errors"
	"strings"
	"time"
)

// Principal is an authenticated identity. Mirrors the engine's Principal and the
// C# Principal / Rust Principal.
type Principal struct {
	Sub    string
	Org    string
	Role   string
	Groups []string
}

// AnonymousPrincipal is the fail-closed identity: org-public, no groups.
var AnonymousPrincipal = Principal{Sub: "anonymous", Org: "public", Role: "anonymous", Groups: nil}

// AccessContext is who's asking, threaded through a turn for (future) ACL-filtered
// retrieval. Mirrors the Rust AccessContext / C# AccessContext. Fails closed: an
// absent/invalid identity is anonymous (org-public).
type AccessContext struct {
	Principal   Principal
	IsAnonymous bool
}

// AnonymousAccess is the fail-closed default access context.
var AnonymousAccess = AccessContext{Principal: AnonymousPrincipal, IsAnonymous: true}

// Groups is the principal's group list.
func (a AccessContext) Groups() []string { return a.Principal.Groups }

// AuthVerifier resolves a connection token (the ?token= query slot) into an
// AccessContext. The seam mirrors the Rust verifier trait (none / jwt / trusted)
// and the C# TokenAccessResolver: any failure fails CLOSED to anonymous, never to an
// all-access principal. A verifier is chosen once at connect.
type AuthVerifier interface {
	// Resolve maps a raw token to an access context. Implementations must return
	// AnonymousAccess (never an error) for an empty/invalid token so the no-auth and
	// dev paths keep serving org-public knowledge.
	Resolve(token string) AccessContext
	// Mode names the verifier ("none" / "jwt" / "trusted") for logging.
	Mode() string
}

// PermissiveVerifier is the default no-auth verifier: every connection is anonymous
// (org-public). The Go analog of the Rust NoAuthVerifier.
type PermissiveVerifier struct{}

// Resolve always returns the anonymous access context.
func (PermissiveVerifier) Resolve(string) AccessContext { return AnonymousAccess }

// Mode returns "none".
func (PermissiveVerifier) Mode() string { return "none" }

// LocalTokenVerifier verifies an HS256-signed JWT against a shared secret, mirroring
// the Rust LocalTokenVerifier / C# AuthMode.Jwt path. A valid token yields the
// claims' principal; anything missing, malformed, expired, or failing verification
// fails closed to anonymous.
type LocalTokenVerifier struct {
	secret []byte
}

// NewLocalTokenVerifier builds an HS256 verifier over the given shared secret.
func NewLocalTokenVerifier(secret string) *LocalTokenVerifier {
	return &LocalTokenVerifier{secret: []byte(secret)}
}

// Mode returns "jwt".
func (*LocalTokenVerifier) Mode() string { return "jwt" }

// Resolve verifies the HS256 JWT and maps its claims to an access context; on any
// failure it fails closed to anonymous.
func (v *LocalTokenVerifier) Resolve(token string) AccessContext {
	token = strings.TrimSpace(token)
	if token == "" {
		return AnonymousAccess
	}
	access, err := v.verify(token)
	if err != nil {
		return AnonymousAccess
	}
	return access
}

func (v *LocalTokenVerifier) verify(token string) (AccessContext, error) {
	parts := strings.Split(token, ".")
	if len(parts) != 3 {
		return AnonymousAccess, errors.New("malformed JWT")
	}
	if len(v.secret) == 0 {
		return AnonymousAccess, errors.New("HS256 secret not configured")
	}
	mac := hmac.New(sha256.New, v.secret)
	mac.Write([]byte(parts[0] + "." + parts[1]))
	expected := mac.Sum(nil)
	actual, err := base64URLDecode(parts[2])
	if err != nil {
		return AnonymousAccess, err
	}
	if !hmac.Equal(expected, actual) {
		return AnonymousAccess, errors.New("bad signature")
	}
	payload, err := base64URLDecode(parts[1])
	if err != nil {
		return AnonymousAccess, err
	}
	return claimsToAccess(payload)
}

func claimsToAccess(payload []byte) (AccessContext, error) {
	var claims struct {
		Exp    int64    `json:"exp"`
		Sub    string   `json:"sub"`
		Org    string   `json:"org"`
		Role   string   `json:"role"`
		Groups []string `json:"groups"`
	}
	if err := json.Unmarshal(payload, &claims); err != nil {
		return AnonymousAccess, err
	}
	if claims.Exp != 0 && time.Unix(claims.Exp, 0).Before(time.Now()) {
		return AnonymousAccess, errors.New("token expired")
	}
	p := Principal{
		Sub:    orDefault(claims.Sub, "unknown"),
		Org:    orDefault(claims.Org, "public"),
		Role:   orDefault(claims.Role, "basic"),
		Groups: claims.Groups,
	}
	return AccessContext{Principal: p, IsAnonymous: false}, nil
}

func orDefault(v, def string) string {
	if v == "" {
		return def
	}
	return v
}

// base64URLDecode decodes standard or padding-less base64url (JWT segments are
// unpadded). Tries raw first, then padded.
func base64URLDecode(s string) ([]byte, error) {
	if b, err := base64.RawURLEncoding.DecodeString(s); err == nil {
		return b, nil
	}
	return base64.URLEncoding.DecodeString(s)
}
