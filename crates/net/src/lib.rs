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
///
/// Thin wrapper over [`request`] that preserves the historical GET surface.
pub fn fetch(url: &str) -> Result<Response, String> {
    request("GET", url, None, &[])
}

/// Issue an HTTP request with an arbitrary `method` and return a [`Response`], or an
/// `Err(String)` describing the failure. Supports GET/POST/PUT/PATCH/DELETE/HEAD/OPTIONS over
/// `http(s)://` (via the reused HTTP client) and `file://` reads (GET-like, body/headers ignored).
///
/// `body` is sent (via `send_bytes`) for methods that carry a payload (POST/PUT/PATCH/DELETE);
/// other methods use `.call()`. `headers` are applied verbatim (callers set Content-Type etc.).
/// The opt-in disk cache (`NET_CACHE_DIR`) applies to GET only; non-GET requests bypass it.
pub fn request(
    method: &str,
    url: &str,
    body: Option<&[u8]>,
    headers: &[(String, String)],
) -> Result<Response, String> {
    let method_uc = method.to_ascii_uppercase();

    if let Some(path) = url.strip_prefix("file://") {
        // file:// is a local read; method/body/headers don't apply.
        return fetch_file(path, url);
    }

    let is_get = method_uc == "GET";

    // Opt-in on-disk cache (set NET_CACHE_DIR): serve a previously-cached body so repeated runs
    // don't re-hit the network. GET only (keyed by URL); non-GET bypasses the cache entirely.
    let cache = if is_get { cache_path(url) } else { None };
    if let Some(p) = &cache {
        if let Ok(body) = std::fs::read(p) {
            return Ok(Response {
                status: 200,
                content_type: content_type_from_url(url),
                body,
                final_url: url.to_string(),
            });
        }
    }

    // Whether this method carries a request body.
    let has_body = matches!(method_uc.as_str(), "POST" | "PUT" | "PATCH" | "DELETE");

    // Present a mainstream browser User-Agent. Many sites (Google, etc.) serve a stripped
    // or blocked page to unknown clients like ureq's default UA, so we look like a browser.
    //
    // Retry policy is asymmetric, because retry cost differs hugely by failure type:
    //   * A non-2xx STATUS (403/429/5xx) comes back fast, so retrying with backoff is cheap and
    //     worthwhile — CDNs use 403/429 for bot/rate limiting and a 200+ module burst trips it.
    //   * A TRANSPORT error that is a stall hits the full read timeout first; retrying it 4×
    //     would block a single dead sub-resource (image/script) for ~a minute and freeze the
    //     page load. So transport errors get ONE quick retry (catches a transient reset) and a
    //     modest timeout, never the multiply-the-timeout loop.
    let mut attempt = 0;
    let resp = loop {
        let mut req = agent()
            .request(&method_uc, url)
            // Bound the whole request (DNS + connect + read) so one stalled connection can't
            // hang the engine. Kept modest so a dead sub-resource fails fast.
            .timeout(std::time::Duration::from_secs(8))
            .set("User-Agent", BROWSER_USER_AGENT)
            .set(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .set("Accept-Language", "en-US,en;q=0.9");
        for (name, value) in headers {
            req = req.set(name, value);
        }
        let result = if has_body {
            req.send_bytes(body.unwrap_or(&[]))
        } else {
            req.call()
        };
        let backoff = |a: u32| std::time::Duration::from_millis(match a { 1 => 200, 2 => 500, _ => 1000 });
        match result {
            Ok(resp) => break resp,
            // Fast status failures: retry rate-limit/server statuses with backoff (up to 3×).
            Err(ureq::Error::Status(code, _)) => {
                if (code == 403 || code == 429 || code >= 500) && attempt < 3 {
                    attempt += 1;
                    std::thread::sleep(backoff(attempt));
                    continue;
                }
                return Err(format!("HTTP error status {code} for {url}"));
            }
            // Transport/connection error (reset, timeout, DNS): a single quick retry only, so a
            // stalled resource can't multiply an 8s timeout into a minute-long page-load freeze.
            Err(e) => {
                if attempt < 1 {
                    attempt += 1;
                    std::thread::sleep(backoff(attempt));
                    continue;
                }
                return Err(format!("request failed: {e}"));
            }
        }
    };

    let status = resp.status();
    let content_type = resp
        .header("Content-Type")
        .unwrap_or("application/octet-stream")
        .to_string();

    let mut resp_body = Vec::new();
    resp.into_reader()
        .take(MAX_BODY_BYTES)
        .read_to_end(&mut resp_body)
        .map_err(|e| format!("failed to read body: {e}"))?;

    // Populate the opt-in disk cache on success (GET only).
    if status == 200 {
        if let Some(p) = &cache {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(p, &resp_body);
        }
    }

    Ok(Response { status, content_type, body: resp_body, final_url: url.to_string() })
}

/// Disk-cache file path for `url` under `NET_CACHE_DIR` (a stable hash of the URL), or `None`
/// when the cache is disabled.
fn cache_path(url: &str) -> Option<std::path::PathBuf> {
    let dir = std::env::var_os("NET_CACHE_DIR")?;
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut h);
    Some(std::path::Path::new(&dir).join(format!("{:016x}", h.finish())))
}

/// Guess a content type from a URL's file extension (used for cached responses).
fn content_type_from_url(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    match path.rsplit('.').next() {
        Some("js") | Some("mjs") => "text/javascript",
        Some("css") => "text/css",
        Some("html") | Some("htm") => "text/html",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    }
    .to_string()
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

    #[test]
    fn request_get_delegates_to_file_read() {
        // `request("GET", file://…)` reads local disk exactly like `fetch` (delegation path).
        let mut path = std::env::temp_dir();
        path.push(format!("net_request_test_{}.txt", std::process::id()));
        std::fs::write(&path, b"hello body").unwrap();
        let url = format!("file://{}", path.display());
        let r = request("GET", &url, None, &[]).expect("file read");
        assert_eq!(r.body, b"hello body");
        // fetch() delegates to request("GET", …): identical result.
        let r2 = fetch(&url).expect("fetch delegate");
        assert_eq!(r2.body, b"hello body");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn request_post_to_file_is_read() {
        // Non-GET to file:// still reads (method/body ignored for local files); proves the
        // method/body/headers plumbing compiles and routes through `request`.
        let mut path = std::env::temp_dir();
        path.push(format!("net_request_post_{}.txt", std::process::id()));
        std::fs::write(&path, b"payload").unwrap();
        let url = format!("file://{}", path.display());
        let headers = vec![("Content-Type".to_string(), "text/plain".to_string())];
        let r = request("POST", &url, Some(b"ignored"), &headers).expect("file read");
        assert_eq!(r.body, b"payload");
        let _ = std::fs::remove_file(&path);
    }
}
