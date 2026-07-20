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
	Sub string
	Org string
	// Email is the identity's email claim — the per-user ownership key for conversations
	// (th-8fe998). "" means the token carried no email: with auth enabled that fails CLOSED
	// (no conversations visible), never falls back to unscoped.
	Email  string
	Role   string
	Groups []string
}

// AnonymousPrincipal is the fail-closed identity: org-public, no groups, no email.
var AnonymousPrincipal = Principal{Sub: "anonymous", Org: "public", Role: "anonymous", Groups: nil}

// AccessContext is who's asking, threaded through a turn for (future) ACL-filtered
// retrieval. Mirrors the Rust AccessContext / C# AccessContext. Fails closed: an
// absent/invalid identity is anonymous (org-public).
type AccessContext struct {
	Principal   Principal
	IsAnonymous bool
	// AuthEnabled records whether the SERVER has auth configured at all, independent of
	// whether THIS connection presented a valid token. It is the switch that decides
	// whether conversations are per-user scoped, so it must not be confused with
	// IsAnonymous: a connection that presented a garbage token to an auth-enabled server is
	// anonymous AND auth-enabled, and must see nothing — not everything. th-8fe998.
	AuthEnabled bool
}

// AnonymousAccess is the fail-closed default access context (auth NOT configured).
var AnonymousAccess = AccessContext{Principal: AnonymousPrincipal, IsAnonymous: true}

// anonymousAuthedAccess is the fail-closed context an auth-ENABLED verifier returns when a
// token is missing, malformed, expired, or fails verification. Distinct from AnonymousAccess
// only in AuthEnabled — which is exactly what keeps a bad token from being handed the
// unscoped (auth-disabled) view of every user's conversations. th-8fe998.
var anonymousAuthedAccess = AccessContext{Principal: AnonymousPrincipal, IsAnonymous: true, AuthEnabled: true}

// Groups is the principal's group list.
func (a AccessContext) Groups() []string { return a.Principal.Groups }

// ConversationScope derives this connection's conversation visibility from its
// authenticated principal — never from anything the client sent in a frame. Exactly three
// outcomes, and only the first is unscoped:
//
//	auth disabled                     → unscoped (local/dev single-tenant, unchanged)
//	auth enabled, principal has email → scoped to that email
//	auth enabled, no/blank email      → fail CLOSED (zero value: nothing visible)
//
// th-8fe998.
func (a AccessContext) ConversationScope() ConversationScope {
	if !a.AuthEnabled {
		return ConversationScope{Unscoped: true}
	}
	return ConversationScope{Email: a.Principal.Email}
}

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
// failure it fails closed to anonymous — but with AuthEnabled still set, so a rejected
// token gets the empty per-user view rather than the unscoped one. th-8fe998.
func (v *LocalTokenVerifier) Resolve(token string) AccessContext {
	token = strings.TrimSpace(token)
	if token == "" {
		return anonymousAuthedAccess
	}
	access, err := v.verify(token)
	if err != nil {
		return anonymousAuthedAccess
	}
	return access
}

func (v *LocalTokenVerifier) verify(token string) (AccessContext, error) {
	parts := strings.Split(token, ".")
	if len(parts) != 3 {
		return anonymousAuthedAccess, errors.New("malformed JWT")
	}
	if len(v.secret) == 0 {
		return anonymousAuthedAccess, errors.New("HS256 secret not configured")
	}
	mac := hmac.New(sha256.New, v.secret)
	mac.Write([]byte(parts[0] + "." + parts[1]))
	expected := mac.Sum(nil)
	actual, err := base64URLDecode(parts[2])
	if err != nil {
		return anonymousAuthedAccess, err
	}
	if !hmac.Equal(expected, actual) {
		return anonymousAuthedAccess, errors.New("bad signature")
	}
	payload, err := base64URLDecode(parts[1])
	if err != nil {
		return anonymousAuthedAccess, err
	}
	return claimsToAccess(payload)
}

func claimsToAccess(payload []byte) (AccessContext, error) {
	var claims struct {
		Exp    int64    `json:"exp"`
		Sub    string   `json:"sub"`
		Org    string   `json:"org"`
		Email  string   `json:"email"`
		Role   string   `json:"role"`
		Groups []string `json:"groups"`
	}
	if err := json.Unmarshal(payload, &claims); err != nil {
		return anonymousAuthedAccess, err
	}
	if claims.Exp != 0 && time.Unix(claims.Exp, 0).Before(time.Now()) {
		return anonymousAuthedAccess, errors.New("token expired")
	}
	p := Principal{
		Sub: orDefault(claims.Sub, "unknown"),
		Org: orDefault(claims.Org, "public"),
		// No default: a token without an email claim must NOT be silently given one, and
		// must NOT widen to unscoped. Empty here = fail closed. th-8fe998.
		Email:  strings.TrimSpace(claims.Email),
		Role:   orDefault(claims.Role, "basic"),
		Groups: claims.Groups,
	}
	return AccessContext{Principal: p, IsAnonymous: false, AuthEnabled: true}, nil
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
