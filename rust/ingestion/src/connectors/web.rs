//! [`WebConnector`] — fetch a public URL → readable text → one [`RawDocument`].
//!
//! Reuses the engine's `fetch_url` tool internals so the web connector and the
//! agent's `fetch_url` tool share **exactly one** HTML→text stripper and one
//! SSRF guard (no second, drifting copy):
//!
//! - [`assert_url_is_public`] rejects loopback / private / link-local /
//!   metadata / non-http(s) URLs *before* any request,
//! - [`html_to_text`] strips scripts/styles/tags/entities to plain text.
//!
//! ## Test split (G9)
//!
//! The strip + guard logic is unit-tested **offline** (fixture HTML, no
//! network). The one test that actually fetches a URL is `#[ignore]` and gated
//! on `SMOOTH_AGENT_E2E=1`, so credential-free CI never makes a network call;
//! nightly/e2e runs exercise the live path.

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;

use smooth_operator::tools::fetch_url::{assert_url_is_public, html_to_text, safe_http_client};

use crate::connector::{Connector, RawDocument, Timestamp};

/// Fetches a single public URL and exposes it as one document.
pub struct WebConnector {
    url: String,
    client: reqwest::Client,
}

impl WebConnector {
    /// Build a connector for `url` with a default HTTP client.
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            // Shared SSRF-safe client: re-validates every redirect hop (a bare
            // reqwest::Client follows 30x to internal/metadata IPs).
            client: safe_http_client(),
        }
    }

    /// Build over a caller-provided client (custom timeouts/proxy).
    #[must_use]
    pub fn with_client(url: impl Into<String>, client: reqwest::Client) -> Self {
        Self {
            url: url.into(),
            client,
        }
    }

    /// Turn an HTML/text body into a [`RawDocument`]. Pure (no I/O) so the
    /// strip path is unit-testable offline. `content_type` may be empty.
    fn body_to_doc(url: &str, content_type: &str, body: &str) -> RawDocument {
        let is_html = content_type.contains("html") || looks_like_html(body);
        let text = if is_html {
            html_to_text(body)
        } else {
            body.split_whitespace().collect::<Vec<_>>().join(" ")
        };
        RawDocument::new(url, "web", text)
            .with_title(url)
            .with_metadata("url", url)
            .with_metadata("content_type", content_type)
    }
}

/// Heuristic: does this body look like HTML even without a content-type?
fn looks_like_html(body: &str) -> bool {
    // Floor the 512-byte window to a char boundary — a naive `&body[..512]`
    // byte-slice panics when byte 512 lands mid-multibyte-char (DoS on a
    // server-controlled body).
    let mut end = body.len().min(512);
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    let head = body[..end].to_ascii_lowercase();
    head.contains("<html") || head.contains("<!doctype html") || head.contains("<body")
}

#[async_trait]
impl Connector for WebConnector {
    fn name(&self) -> &str {
        "web"
    }

    async fn pull(&self, _since: Option<Timestamp>) -> Result<Vec<RawDocument>> {
        // SSRF guard runs BEFORE any request — same guard as the fetch_url tool.
        let url = assert_url_is_public(&self.url)
            .with_context(|| format!("web connector refused URL {:?}", self.url))?;

        let resp = self
            .client
            .get(url.clone())
            .header(reqwest::header::USER_AGENT, "smooth-operator/web-connector")
            .send()
            .await
            .map_err(|e| anyhow!("web connector request failed for {url}: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!(
                "web connector got HTTP {} from {url}",
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
            .map_err(|e| anyhow!("web connector failed reading body from {url}: {e}"))?;

        Ok(vec![Self::body_to_doc(
            self.url.as_str(),
            &content_type,
            &body,
        )])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- offline: SSRF guard is enforced before any request ---------------

    #[tokio::test]
    async fn rejects_internal_url_without_fetching() {
        let connector = WebConnector::new("http://169.254.169.254/latest/meta-data/");
        let err = connector
            .pull(None)
            .await
            .expect_err("metadata IP must be rejected");
        assert!(err.to_string().contains("SSRF guard") || err.to_string().contains("refused"));
    }

    #[tokio::test]
    async fn rejects_non_http_scheme() {
        let connector = WebConnector::new("file:///etc/passwd");
        assert!(connector.pull(None).await.is_err());
    }

    // ---- offline: HTML → text strip (fixture body, no network) ------------

    #[test]
    fn body_to_doc_strips_html_to_text() {
        let html = r#"<!doctype html><html><head>
            <style>.x{color:red}</style>
            <script>var leak="nope";</script>
            </head><body><h1>Hello &amp; welcome</h1>
            <p>The window is&nbsp;17 days.</p></body></html>"#;
        let doc = WebConnector::body_to_doc("https://example.com/help", "text/html", html);
        assert_eq!(doc.source, "web");
        assert_eq!(doc.id, "https://example.com/help");
        assert!(
            doc.content.contains("Hello & welcome"),
            "got: {}",
            doc.content
        );
        assert!(doc.content.contains("The window is 17 days."));
        assert!(
            !doc.content.contains("nope"),
            "script leaked: {}",
            doc.content
        );
        assert!(
            !doc.content.contains("color:red"),
            "style leaked: {}",
            doc.content
        );
        assert!(!doc.content.contains('<'), "tags leaked: {}", doc.content);
    }

    #[test]
    fn body_to_doc_detects_html_without_content_type() {
        let html = "<html><body><p>bare html</p></body></html>";
        let doc = WebConnector::body_to_doc("https://example.com/x", "", html);
        assert_eq!(doc.content, "bare html");
    }

    #[test]
    fn body_to_doc_no_panic_on_multibyte_boundary() {
        // 200 × the 3-byte '€' (600 bytes) — byte 512 is not a char boundary, so a
        // naive &body[..512] would panic on this server-controlled body.
        let mut body = "€".repeat(200);
        body.push_str("<html><body>x</body></html>");
        let doc = WebConnector::body_to_doc("https://example.com/x", "", &body); // must not panic
        assert_eq!(doc.id, "https://example.com/x");
    }

    #[test]
    fn body_to_doc_passes_plain_text_through() {
        let doc =
            WebConnector::body_to_doc("https://example.com/x", "text/plain", "just  plain\n text");
        assert_eq!(doc.content, "just plain text");
    }

    // ---- gated: real network fetch (skips credential-free) ----------------

    /// Live fetch — only runs with `SMOOTH_AGENT_E2E=1` (network). Run with:
    /// `SMOOTH_AGENT_E2E=1 cargo test -p smooai-smooth-operator-ingestion \
    ///    --lib web::tests::live_fetch_example -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "network: gated on SMOOTH_AGENT_E2E"]
    async fn live_fetch_example() {
        if std::env::var("SMOOTH_AGENT_E2E").as_deref() != Ok("1") {
            eprintln!("skipping live web fetch: set SMOOTH_AGENT_E2E=1 to run");
            return;
        }
        let connector = WebConnector::new("https://example.com/");
        let docs = connector.pull(None).await.expect("live fetch");
        assert_eq!(docs.len(), 1);
        assert!(
            docs[0].content.to_lowercase().contains("example domain"),
            "got: {}",
            docs[0].content
        );
    }
}
