//! The `fetch_url` tool — fetch a public web page and return readable text.
//!
//! Lets the agent pull a page the user references (a docs URL, a public help
//! article) into the conversation. The response is best-effort HTML→text
//! (scripts/styles stripped, tags removed, whitespace collapsed) and
//! length-capped so a huge page can't blow the model's context.
//!
//! # SSRF guard
//!
//! An agent-controllable URL fetcher is a classic SSRF vector: a crafted URL
//! (`http://169.254.169.254/…`, `http://localhost/…`, an internal `10.x`
//! address) could pull cloud-metadata credentials or reach internal services.
//! Before any request, [`assert_url_is_public`] rejects:
//!   - non-`http`/`https` schemes,
//!   - `localhost` and any loopback / link-local / private / unspecified IP,
//!   - the cloud metadata IP `169.254.169.254` (covered by link-local),
//!   - hosts that are bare IPs in a private/reserved range.
//!
//! The guard runs on the parsed host BEFORE the request is built, so a rejected
//! URL is never fetched.

use async_trait::async_trait;
use std::net::{Ipv4Addr, Ipv6Addr};
use url::{Host, Url};

use smooth_operator::tool::ToolSchema;
use smooth_operator::Tool;

/// Hard cap on the returned text length (characters). Keeps a huge page from
/// blowing the model's context window.
const MAX_TEXT_LEN: usize = 8_000;

/// A [`Tool`] that fetches a public URL and returns its readable text.
///
/// Holds a shared [`reqwest::Client`] so repeated fetches reuse the connection
/// pool. The SSRF guard runs per call in [`Self::execute`].
pub struct FetchUrlTool {
    client: reqwest::Client,
}

impl FetchUrlTool {
    /// Build the tool with a default HTTP client.
    #[must_use]
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    /// Build the tool over a caller-provided client (e.g. one with custom
    /// timeouts or a proxy).
    #[must_use]
    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }
}

impl Default for FetchUrlTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for FetchUrlTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "fetch_url".to_string(),
            description: "Fetch a PUBLIC web page over HTTP(S) and return its readable text \
                          content (HTML stripped to plain text, length-capped). Use this to read \
                          a public docs page, help article, or webpage the user references. \
                          Internal/private/loopback/metadata addresses are rejected for security."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The absolute http(s) URL to fetch (e.g. \
                                        'https://example.com/docs/page')."
                    }
                },
                "required": ["url"]
            }),
        }
    }

    async fn execute(&self, arguments: serde_json::Value) -> anyhow::Result<String> {
        let raw_url = arguments
            .get("url")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("fetch_url requires a string 'url' argument"))?;

        // SSRF guard: validate scheme + host BEFORE making any request.
        let url = assert_url_is_public(raw_url)?;

        let resp = self
            .client
            .get(url.clone())
            .header(
                reqwest::header::USER_AGENT,
                "smooth-operator-agent/fetch_url",
            )
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("fetch_url request failed for {url}: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow::anyhow!(
                "fetch_url got HTTP {} from {url}",
                status.as_u16()
            ));
        }

        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();

        let body = resp
            .text()
            .await
            .map_err(|e| anyhow::anyhow!("fetch_url failed reading body from {url}: {e}"))?;

        // HTML → text only when it looks like HTML; otherwise treat as plain
        // text (e.g. text/plain, application/json) and just collapse whitespace.
        let text = if content_type.contains("html") || looks_like_html(&body) {
            html_to_text(&body)
        } else {
            collapse_whitespace(&body)
        };

        Ok(cap_len(&text, MAX_TEXT_LEN))
    }

    fn is_read_only(&self) -> bool {
        true
    }
}

/// Parse and validate a URL for the SSRF guard. Returns the parsed [`Url`] on
/// success, or an error describing why it was rejected.
///
/// Rejects: non-http(s) schemes; `localhost`; and any host that resolves to (or
/// literally is) a loopback, link-local (incl. `169.254.169.254` metadata),
/// private, or unspecified IP.
///
/// Note: this validates the *literal* host. A hostname that resolves via DNS to
/// a private IP (DNS-rebinding) is not caught here — a production deployment
/// should additionally pin/validate the resolved address (or front this with an
/// egress proxy). The guard blocks the common, directly-expressible SSRF URLs.
pub fn assert_url_is_public(raw_url: &str) -> anyhow::Result<Url> {
    let url = Url::parse(raw_url)
        .map_err(|e| anyhow::anyhow!("fetch_url: invalid URL {raw_url:?}: {e}"))?;

    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(anyhow::anyhow!(
            "fetch_url: refusing non-http(s) scheme {scheme:?} (only http/https allowed)"
        ));
    }

    let host = url
        .host()
        .ok_or_else(|| anyhow::anyhow!("fetch_url: URL {raw_url:?} has no host"))?;

    match host {
        Host::Domain(domain) => {
            let lowered = domain.to_ascii_lowercase();
            // `localhost` (and any subdomain of it) maps to loopback.
            if lowered == "localhost" || lowered.ends_with(".localhost") {
                return Err(anyhow::anyhow!(
                    "fetch_url: refusing to fetch localhost ({domain:?}) — SSRF guard"
                ));
            }
            // A domain literal that happens to parse as an IP (rare) is caught
            // by the IpAddr arms below; otherwise allow the domain.
        }
        Host::Ipv4(ip) => assert_ipv4_public(ip)?,
        Host::Ipv6(ip) => assert_ipv6_public(ip)?,
    }

    Ok(url)
}

/// Reject loopback / private / link-local / unspecified / broadcast IPv4.
/// Link-local (`169.254.0.0/16`) covers the cloud metadata IP
/// `169.254.169.254`.
fn assert_ipv4_public(ip: Ipv4Addr) -> anyhow::Result<()> {
    let blocked = ip.is_loopback()        // 127.0.0.0/8
        || ip.is_private()                // 10/8, 172.16/12, 192.168/16
        || ip.is_link_local()             // 169.254.0.0/16 (incl. metadata)
        || ip.is_unspecified()            // 0.0.0.0
        || ip.is_broadcast()              // 255.255.255.255
        || ip.is_documentation()          // 192.0.2.0/24 etc.
        || is_shared_cgnat(ip)            // 100.64.0.0/10 (carrier-grade NAT)
        || ip.octets()[0] == 0; // 0.0.0.0/8 "this network"
    if blocked {
        return Err(anyhow::anyhow!(
            "fetch_url: refusing to fetch non-public IPv4 {ip} — SSRF guard"
        ));
    }
    Ok(())
}

/// `100.64.0.0/10` — carrier-grade NAT (RFC 6598); not internet-routable.
fn is_shared_cgnat(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    a == 100 && (64..=127).contains(&b)
}

/// Reject loopback / unspecified / link-local / unique-local IPv6, and
/// IPv4-mapped/compatible addresses that map to a blocked IPv4.
fn assert_ipv6_public(ip: Ipv6Addr) -> anyhow::Result<()> {
    // Unwrap IPv4-mapped (::ffff:a.b.c.d) / compatible addresses and re-check.
    if let Some(v4) = ip.to_ipv4() {
        return assert_ipv4_public(v4);
    }
    let is_unique_local = (ip.segments()[0] & 0xfe00) == 0xfc00; // fc00::/7
    let is_link_local = (ip.segments()[0] & 0xffc0) == 0xfe80; // fe80::/10
    let blocked = ip.is_loopback() || ip.is_unspecified() || is_unique_local || is_link_local;
    if blocked {
        return Err(anyhow::anyhow!(
            "fetch_url: refusing to fetch non-public IPv6 {ip} — SSRF guard"
        ));
    }
    Ok(())
}

/// Heuristic: does this body look like HTML even without a content-type?
fn looks_like_html(body: &str) -> bool {
    let head = &body[..body.len().min(512)].to_ascii_lowercase();
    head.contains("<html") || head.contains("<!doctype html") || head.contains("<body")
}

/// Best-effort HTML → readable text. Drops `<script>`/`<style>` blocks and
/// HTML comments entirely, strips remaining tags, decodes a handful of common
/// entities, and collapses whitespace. Intentionally dependency-free and
/// lenient — it does not aim to be a full HTML parser.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let bytes = html.as_bytes();
    let lower = html.to_ascii_lowercase();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'<' {
            // Skip <script>…</script> and <style>…</style> bodies wholesale.
            if let Some(end) = skip_block(&lower, i, "script") {
                i = end;
                out.push(' ');
                continue;
            }
            if let Some(end) = skip_block(&lower, i, "style") {
                i = end;
                out.push(' ');
                continue;
            }
            // Skip HTML comments <!-- … -->.
            if lower[i..].starts_with("<!--") {
                if let Some(rel) = lower[i..].find("-->") {
                    i += rel + 3;
                    continue;
                }
                break;
            }
            // Skip a normal tag: advance to the matching '>'.
            if let Some(rel) = html[i..].find('>') {
                i += rel + 1;
                // A tag boundary is a word boundary.
                out.push(' ');
                continue;
            }
            break;
        }
        // Copy a run of non-'<' characters.
        let start = i;
        while i < bytes.len() && bytes[i] != b'<' {
            i += 1;
        }
        out.push_str(&html[start..i]);
    }

    let decoded = decode_entities(&out);
    collapse_whitespace(&decoded)
}

/// If a `<tag …>` opens at `start` (case-insensitive, in `lower`), return the
/// index just past its matching `</tag>`. Otherwise `None`.
fn skip_block(lower: &str, start: usize, tag: &str) -> Option<usize> {
    let open = format!("<{tag}");
    if !lower[start..].starts_with(&open) {
        return None;
    }
    let close = format!("</{tag}>");
    match lower[start..].find(&close) {
        Some(rel) => Some(start + rel + close.len()),
        // Unclosed block: consume to end.
        None => Some(lower.len()),
    }
}

/// Decode a small set of common HTML entities. Not exhaustive — enough to make
/// stripped text readable.
fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
}

/// Collapse all runs of whitespace (incl. newlines) to single spaces and trim.
fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Truncate to `max` chars on a char boundary, appending an ellipsis marker
/// when truncated.
fn cap_len(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max).collect();
    format!("{truncated}… [truncated]")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- SSRF guard: rejections (no network) ----------------------------

    #[test]
    fn rejects_localhost() {
        assert!(assert_url_is_public("http://localhost:8080/secret").is_err());
        assert!(assert_url_is_public("http://app.localhost/").is_err());
    }

    #[test]
    fn rejects_loopback_ip() {
        assert!(assert_url_is_public("http://127.0.0.1/").is_err());
        assert!(assert_url_is_public("http://127.0.0.53:53/").is_err());
        assert!(assert_url_is_public("http://[::1]/").is_err());
    }

    #[test]
    fn rejects_metadata_and_link_local() {
        // The cloud metadata endpoint is link-local — the classic SSRF target.
        assert!(assert_url_is_public("http://169.254.169.254/latest/meta-data/").is_err());
        assert!(assert_url_is_public("http://169.254.0.1/").is_err());
    }

    #[test]
    fn rejects_private_ranges() {
        assert!(assert_url_is_public("http://10.0.0.5/").is_err());
        assert!(assert_url_is_public("http://172.16.3.4/").is_err());
        assert!(assert_url_is_public("http://192.168.1.1/").is_err());
        assert!(assert_url_is_public("http://0.0.0.0/").is_err());
        // Carrier-grade NAT.
        assert!(assert_url_is_public("http://100.64.0.1/").is_err());
    }

    #[test]
    fn rejects_ipv4_mapped_ipv6_loopback() {
        assert!(assert_url_is_public("http://[::ffff:127.0.0.1]/").is_err());
    }

    #[test]
    fn rejects_non_http_scheme() {
        assert!(assert_url_is_public("file:///etc/passwd").is_err());
        assert!(assert_url_is_public("ftp://example.com/x").is_err());
        assert!(assert_url_is_public("gopher://evil/").is_err());
    }

    #[test]
    fn allows_public_hosts() {
        assert!(assert_url_is_public("https://example.com/docs").is_ok());
        assert!(assert_url_is_public("http://93.184.216.34/").is_ok()); // example.com's public IP
        assert!(assert_url_is_public("https://api.smoo.ai/v1").is_ok());
    }

    // ---- execute() goes through the guard before any I/O ----------------

    #[tokio::test]
    async fn execute_rejects_internal_url_without_fetching() {
        let tool = FetchUrlTool::new();
        let err = tool
            .execute(serde_json::json!({ "url": "http://169.254.169.254/latest/meta-data/" }))
            .await
            .expect_err("internal URL must be rejected");
        assert!(
            err.to_string().contains("SSRF guard"),
            "expected SSRF guard rejection, got: {err}"
        );
    }

    #[tokio::test]
    async fn execute_requires_url_argument() {
        let tool = FetchUrlTool::new();
        let err = tool
            .execute(serde_json::json!({}))
            .await
            .expect_err("missing url should error");
        assert!(err.to_string().contains("url"));
    }

    // ---- HTML → text (fixture string, no network) -----------------------

    #[test]
    fn html_to_text_strips_tags_scripts_styles_and_entities() {
        let html = r#"
            <!doctype html>
            <html>
              <head>
                <style>.x { color: red; }</style>
                <script>var leak = "should not appear";</script>
                <title>Doc</title>
              </head>
              <body>
                <h1>Hello &amp; welcome</h1>
                <p>The return window is&nbsp;17 days.</p>
                <!-- a comment that should vanish -->
              </body>
            </html>
        "#;
        let text = html_to_text(html);

        assert!(text.contains("Hello & welcome"), "got: {text}");
        assert!(
            text.contains("The return window is 17 days."),
            "got: {text}"
        );
        // Script/style bodies and comments are gone.
        assert!(!text.contains("should not appear"), "script leaked: {text}");
        assert!(!text.contains("color: red"), "style leaked: {text}");
        assert!(!text.contains("a comment"), "comment leaked: {text}");
        // No angle brackets survive.
        assert!(
            !text.contains('<') && !text.contains('>'),
            "tags leaked: {text}"
        );
    }

    #[test]
    fn cap_len_truncates_long_text() {
        let long = "a".repeat(MAX_TEXT_LEN + 100);
        let capped = cap_len(&long, MAX_TEXT_LEN);
        assert!(capped.ends_with("… [truncated]"));
        assert!(capped.chars().count() <= MAX_TEXT_LEN + "… [truncated]".chars().count());
    }

    #[test]
    fn schema_is_read_only_with_url_param() {
        let tool = FetchUrlTool::new();
        let schema = tool.schema();
        assert_eq!(schema.name, "fetch_url");
        assert_eq!(schema.parameters["required"][0], "url");
        assert!(tool.is_read_only());
    }
}
