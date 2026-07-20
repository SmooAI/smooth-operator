using System.Security.Cryptography;
using System.Text;
using System.Text.Json;

namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// An authenticated identity. Mirrors the Rust engine's <c>Principal</c>.
/// <para>
/// <see cref="Email"/> is the per-user data-scoping key: it is the identity conversations are owned
/// by and filtered on (<c>list_conversations</c>, resume, <c>get_conversation_messages</c>). It is an
/// init-only property rather than a positional parameter so adding it does not break every downstream
/// host that constructs a <c>Principal</c>. Populated from the validated token's <c>email</c> claim
/// (the customer's ALB-OIDC → Auth0/Google flow puts it there); <c>null</c> when the token carries no
/// email, which fails closed — such a principal sees no conversations. th-966fab.
/// </para>
/// </summary>
public sealed record Principal(string Sub, string Org, string Role, IReadOnlyList<string> Groups)
{
    public string? Email { get; init; }

    public static Principal Anonymous { get; } = new("anonymous", "public", "anonymous", Array.Empty<string>());
}

/// <summary>
/// The access context threaded through a turn — who's asking, for ACL-filtered retrieval. Mirrors
/// the Rust <c>AccessContext</c>. Fails closed: absent/invalid identity is anonymous (org-public).
/// </summary>
public sealed record AccessContext(Principal Principal, bool IsAnonymous)
{
    /// <summary>
    /// Whether the server has auth CONFIGURED at all — <c>true</c> for any verifier that validates a
    /// token (jwt / trusted / local), <c>false</c> only for the no-auth single-tenant local/dev path.
    /// This is the switch that decides whether per-user conversation scoping applies, and it is
    /// deliberately distinct from <see cref="IsAnonymous"/>: a bad or missing token on an auth-enabled
    /// server is anonymous AND auth-enabled, so it fails closed (sees nothing) instead of falling back
    /// to the unscoped no-auth behavior.
    /// <para>
    /// Defaults to <c>true</c> (fail closed) so a context built by hand is scoped; only
    /// <see cref="Anonymous"/> and the <c>none</c> auth mode clear it. th-966fab.
    /// </para>
    /// </summary>
    public bool AuthEnabled { get; init; } = true;

    /// <summary>Anonymous on a server with NO auth configured — the only unscoped identity.</summary>
    public static AccessContext Anonymous { get; } = new(Principal.Anonymous, true) { AuthEnabled = false };

    public IReadOnlyList<string> Groups => Principal.Groups;

    /// <summary>
    /// The conversation scope this connection may read. Auth off ⇒ <see cref="ConversationScope.Unscoped"/>
    /// (single-tenant, unchanged). Auth on ⇒ scoped to the principal's email, or
    /// <see cref="ConversationScope.None"/> when the principal carries none (fail closed — never
    /// unscoped). th-966fab.
    /// </summary>
    public ConversationScope ConversationScope => !AuthEnabled
        ? ConversationScope.Unscoped
        : string.IsNullOrEmpty(Principal.Email) ? ConversationScope.None : ConversationScope.ForUser(Principal.Email!);
}

/// <summary>
/// Resolves a connection token (the <c>?token=</c> query slot) into an <see cref="AccessContext"/>.
/// The seam the server is wired with at connect time — the C# analog of the Go <c>AuthVerifier</c>
/// interface, the Python <c>AuthVerifier</c> ABC, and the Rust verifier trait. Two impls cover the
/// shapes the other servers expose: <see cref="NoAuthVerifier"/> (permissive default → anonymous)
/// and <see cref="LocalTokenVerifier"/> (validates a configured local HS256 token → authenticated).
/// <para>
/// Fail-closed contract: a missing, empty, malformed, expired, or otherwise unverifiable token
/// resolves to <see cref="AccessContext.Anonymous"/> (org-public) — never a rejected connection and
/// never an all-access principal. This matches the Rust/Python/Go semantics exactly.
/// </para>
/// </summary>
public interface IAuthVerifier
{
    /// <summary>
    /// Resolve a raw token to an access context. Implementations MUST return
    /// <see cref="AccessContext.Anonymous"/> (never throw) for an empty/invalid token so the no-auth
    /// and dev paths keep serving org-public knowledge.
    /// </summary>
    AccessContext Resolve(string? token);

    /// <summary>A short label for logs (<c>none</c> / <c>local</c> / …); never includes secrets.</summary>
    string Mode { get; }
}

/// <summary>
/// The default permissive verifier: every connection is anonymous (org-public). The C# analog of the
/// Go <c>PermissiveVerifier</c>, the Python <c>NoAuthVerifier</c>, and the Rust <c>NoAuthVerifier</c>
/// — used by the local flavor and protocol-only paths. Leaves default server behavior unchanged.
/// </summary>
public sealed class NoAuthVerifier : IAuthVerifier
{
    public static NoAuthVerifier Instance { get; } = new();

    public AccessContext Resolve(string? token) => AccessContext.Anonymous;

    public string Mode => "none";
}

/// <summary>
/// Resolves a token as an HS256-signed JWT (<c>header.payload.signature</c>), failing closed to
/// anonymous on any error. The C# analog of the Go <c>LocalTokenVerifier</c>, the Python
/// <c>LocalTokenVerifier</c>, and the Rust <c>LocalTokenVerifier</c> — the smooth-agent-suggested
/// local-token seam.
/// <para>
/// The signature is verified in constant time against the configured secret; the <c>exp</c> claim
/// (when present) is enforced. A missing/empty token, a malformed JWT, a bad signature, or an
/// expired token all degrade to <see cref="AccessContext.Anonymous"/> — a bad/missing token is
/// anonymous, NOT a rejected connection.
/// </para>
/// </summary>
public sealed class LocalTokenVerifier : IAuthVerifier
{
    private readonly TokenAccessResolver _resolver;

    /// <summary>Build an HS256 verifier over the given shared secret.</summary>
    /// <exception cref="ArgumentException">The secret is null or empty.</exception>
    public LocalTokenVerifier(string secret)
    {
        if (string.IsNullOrEmpty(secret))
        {
            throw new ArgumentException("LocalTokenVerifier requires a non-empty HS256 secret", nameof(secret));
        }

        // Reuse the JWT verification + fail-closed path the TokenAccessResolver already implements.
        _resolver = new TokenAccessResolver(new AuthOptions { Mode = AuthMode.Jwt, Hs256Secret = secret });
    }

    public AccessContext Resolve(string? token) => _resolver.Resolve(token);

    public string Mode => "local";
}

/// <summary>How the server interprets the connection token. Mirrors the Rust <c>AUTH_MODE</c>.</summary>
public enum AuthMode
{
    /// <summary>No auth — every connection is anonymous (org-public).</summary>
    None,

    /// <summary>The token is a signed JWT; verify it (HS256 here).</summary>
    Jwt,

    /// <summary>The token is base64url(JSON) identity forwarded by a trusted proxy; no verification.</summary>
    Trusted,
}

public sealed record AuthOptions
{
    public AuthMode Mode { get; init; } = AuthMode.None;

    /// <summary>Shared secret for HS256 verification when <see cref="Mode"/> is <see cref="AuthMode.Jwt"/>.</summary>
    public string? Hs256Secret { get; init; }
}

/// <summary>
/// Resolves the connection token (the <c>?token=</c> slot) into an <see cref="AccessContext"/>.
/// Fail-closed: anything missing, malformed, expired, or failing verification → anonymous, never an
/// all-access principal. Mirrors the Rust verifier seam (jwt / trusted / none).
/// </summary>
public sealed class TokenAccessResolver : IAuthVerifier
{
    private readonly AuthOptions _options;

    public TokenAccessResolver(AuthOptions options) => _options = options ?? throw new ArgumentNullException(nameof(options));

    /// <summary>A short label for logs, derived from the configured <see cref="AuthMode"/>.</summary>
    public string Mode => _options.Mode switch
    {
        AuthMode.Jwt => "jwt",
        AuthMode.Trusted => "trusted",
        _ => "none",
    };

    public AccessContext Resolve(string? token)
    {
        if (string.IsNullOrEmpty(token))
        {
            return AnonymousForMode();
        }

        try
        {
            return _options.Mode switch
            {
                AuthMode.Trusted => FromTrusted(token),
                AuthMode.Jwt => FromJwt(token),
                _ => AccessContext.Anonymous,
            };
        }
        catch
        {
            // Any failure (malformed, bad signature, expired) fails closed to anonymous.
            return AnonymousForMode();
        }
    }

    /// <summary>
    /// The anonymous context for THIS server's auth mode. On an auth-enabled server a missing or
    /// unverifiable token stays <c>AuthEnabled</c>, so it fails closed to an EMPTY conversation scope
    /// rather than inheriting the unscoped no-auth behavior — the whole point of th-966fab.
    /// </summary>
    private AccessContext AnonymousForMode() => _options.Mode == AuthMode.None
        ? AccessContext.Anonymous
        : AccessContext.Anonymous with { AuthEnabled = true };

    private AccessContext FromTrusted(string token)
    {
        var json = Encoding.UTF8.GetString(Base64UrlDecode(token));
        return FromClaims(json);
    }

    private AccessContext FromJwt(string token)
    {
        var parts = token.Split('.');
        if (parts.Length != 3)
        {
            throw new FormatException("malformed JWT");
        }
        if (string.IsNullOrEmpty(_options.Hs256Secret))
        {
            throw new InvalidOperationException("HS256 secret not configured");
        }

        var signingInput = Encoding.ASCII.GetBytes($"{parts[0]}.{parts[1]}");
        using var hmac = new HMACSHA256(Encoding.UTF8.GetBytes(_options.Hs256Secret));
        var expected = hmac.ComputeHash(signingInput);
        var actual = Base64UrlDecode(parts[2]);
        if (!CryptographicOperations.FixedTimeEquals(expected, actual))
        {
            throw new CryptographicException("bad signature");
        }

        var payload = Encoding.UTF8.GetString(Base64UrlDecode(parts[1]));
        return FromClaims(payload);
    }

    private static AccessContext FromClaims(string json)
    {
        using var document = JsonDocument.Parse(json);
        var root = document.RootElement;

        if (root.TryGetProperty("exp", out var exp) && exp.TryGetInt64(out var expSeconds))
        {
            if (DateTimeOffset.FromUnixTimeSeconds(expSeconds) < DateTimeOffset.UtcNow)
            {
                throw new InvalidOperationException("token expired");
            }
        }

        var sub = root.TryGetProperty("sub", out var s) ? s.GetString() ?? "unknown" : "unknown";
        var org = root.TryGetProperty("org", out var o) ? o.GetString() ?? "public" : "public";
        var role = root.TryGetProperty("role", out var r) ? r.GetString() ?? "basic" : "basic";
        // The data-scoping key. `email` is the standard OIDC claim the customer's ALB → Auth0/Google
        // flow mints; no claim ⇒ null ⇒ the principal sees no conversations (fail closed). th-966fab.
        var email = root.TryGetProperty("email", out var e) ? e.GetString() : null;

        var groups = new List<string>();
        if (root.TryGetProperty("groups", out var g) && g.ValueKind == JsonValueKind.Array)
        {
            foreach (var item in g.EnumerateArray())
            {
                if (item.GetString() is { } group)
                {
                    groups.Add(group);
                }
            }
        }

        return new AccessContext(new Principal(sub, org, role, groups) { Email = string.IsNullOrEmpty(email) ? null : email }, IsAnonymous: false);
    }

    private static byte[] Base64UrlDecode(string value)
    {
        var s = value.Replace('-', '+').Replace('_', '/');
        switch (s.Length % 4)
        {
            case 2: s += "=="; break;
            case 3: s += "="; break;
        }
        return Convert.FromBase64String(s);
    }
}
