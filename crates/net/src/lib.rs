//! Networking (Phase 1). This is the only crate that is functional from the start.
//!
//! We wrap a reused blocking HTTP client (`ureq`) behind our own [`Response`]/[`fetch`]
//! surface so that we can later swap in a hand-written HTTP implementation without
//! touching callers. Keep the public surface stable.

use std::io::Read;
use std::sync::OnceLock;

/// A single shared HTTP agent: it pools TCP/TLS connections and caches DNS per host, so many
/// concurrent fetches to the same origin (e.g. a 200+ module graph) reuse connections instead
/// of each doing its own DNS lookup + handshake — which previously overwhelmed flaky resolvers
/// ("failed to lookup address") and was slow.
fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(10))
            .timeout_read(std::time::Duration::from_secs(15))
            .max_idle_connections_per_host(16)
            .build()
    })
}

/// A mainstream desktop-Safari User-Agent so sites serve us their normal content.
const BROWSER_USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 \
     (KHTML, like Gecko) Version/17.4 Safari/605.1.15";

/// Maximum response body we will buffer. Deliberately well above 4 GiB: a tab is just heap
/// in our single 64-bit process (we don't cap tabs at 4 GiB the way Chrome's V8-isolate /
/// pointer-compression model effectively does), so the only ceiling is the machine's
/// RAM+swap. This cap exists only as a backstop against a truly unbounded/malicious stream.
const MAX_BODY_BYTES: u64 = 16 * 1024 * 1024 * 1024; // 16 GiB

/// A fetched HTTP response.
pub struct Response {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    pub final_url: String,
}

/// GET `url` and return a [`Response`], or an `Err(String)` describing the failure.
/// Supports `http(s)://` (via the reused HTTP client) and `file://` (local read), so
/// local test pages can be loaded without a server.
pub fn fetch(url: &str) -> Result<Response, String> {
    if let Some(path) = url.strip_prefix("file://") {
        return fetch_file(path, url);
    }

    // Present a mainstream browser User-Agent. Many sites (Google, etc.) serve a stripped
    // or blocked page to unknown clients like ureq's default UA, so we look like a browser.
    let resp = match agent()
        .get(url)
        // Bound the whole request (DNS + connect + read) so one stalled connection can't hang
        // the engine — important when fetching large resource graphs (e.g. 200+ JS modules).
        .timeout(std::time::Duration::from_secs(15))
        .set("User-Agent", BROWSER_USER_AGENT)
        .set(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .set("Accept-Language", "en-US,en;q=0.9")
        .call()
    {
        Ok(resp) => resp,
        // ureq's `Error::Status` carries a non-2xx response; surface it as an error
        // string. Any other variant is a transport/connection error.
        Err(ureq::Error::Status(code, _)) => {
            return Err(format!("HTTP error status {code} for {url}"));
        }
        Err(e) => return Err(format!("request failed: {e}")),
    };

    let status = resp.status();
    let content_type = resp
        .header("Content-Type")
        .unwrap_or("application/octet-stream")
        .to_string();

    let mut body = Vec::new();
    resp.into_reader()
        .take(MAX_BODY_BYTES)
        .read_to_end(&mut body)
        .map_err(|e| format!("failed to read body: {e}"))?;

    Ok(Response { status, content_type, body, final_url: url.to_string() })
}

/// Read a `file://` URL from local disk. `path` is the part after `file://`.
fn fetch_file(path: &str, original: &str) -> Result<Response, String> {
    let body = std::fs::read(path).map_err(|e| format!("failed to read {path}: {e}"))?;
    let content_type = match path.rsplit('.').next() {
        Some("html") | Some("htm") => "text/html",
        Some("css") => "text/css",
        Some("js") => "text/javascript",
        Some("json") => "application/json",
        Some("txt") => "text/plain",
        _ => "application/octet-stream",
    }
    .to_string();
    Ok(Response { status: 200, content_type, body, final_url: original.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_url_is_err() {
        assert!(fetch("not a url").is_err());
        assert!(fetch("http://").is_err());
    }

    #[test]
    fn missing_file_is_err() {
        assert!(fetch("file:///nonexistent/path/xyz.html").is_err());
    }

    #[test]
    fn body_cap_exceeds_4gib() {
        // Tabs are not capped at 4 GiB; the body backstop must sit comfortably above it.
        assert!(MAX_BODY_BYTES > 4 * 1024 * 1024 * 1024);
    }
}
