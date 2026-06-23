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
        let mut builder = ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(10))
            .timeout_read(std::time::Duration::from_secs(15))
            .max_idle_connections_per_host(16)
            // Persist cookies across requests AND redirects so logins/sessions survive.
            .cookie_store(cookie_store::CookieStore::new(None));
        // Test-driving env knobs (no effect on normal browsing):
        //  - `WPT_INSECURE_TLS`: accept any server certificate. The WebDriver server sets this so
        //    `.https` WPT tests load over TLS against `wpt serve`'s self-signed cert without needing
        //    the checkout's CA path (matches the `acceptInsecureCerts` capability it advertises).
        //  - `WPT_CA_FILE`: trust a specific extra CA (the WPT `tools/certs/cacert.pem`).
        if let Some(cfg) = wpt_tls_config() {
            builder = builder.tls_config(std::sync::Arc::new(cfg));
        }
        builder.build()
    })
}

/// A test-only rustls client config, or `None` to leave the agent on its default trust store.
/// `WPT_INSECURE_TLS` disables certificate verification entirely; otherwise `WPT_CA_FILE` adds an
/// extra trusted CA on top of the usual webpki roots. A missing/unparseable `WPT_CA_FILE` degrades
/// gracefully to `None`.
fn wpt_tls_config() -> Option<rustls::ClientConfig> {
    // `ring` is a hard dependency, so the provider is always present — no default-provider panic.
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());

    if std::env::var_os("WPT_INSECURE_TLS").is_some() {
        return rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .ok()?
            .dangerous()
            .with_custom_certificate_verifier(std::sync::Arc::new(insecure::NoVerify(provider)))
            .with_no_client_auth()
            .into();
    }

    let ca_path = std::env::var("WPT_CA_FILE").ok()?;
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let pem = std::fs::read(ca_path).ok()?;
    for cert in rustls_pemfile::certs(&mut &pem[..]).flatten() {
        let _ = roots.add(cert);
    }
    rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .ok()?
        .with_root_certificates(roots)
        .with_no_client_auth()
        .into()
}

/// A rustls verifier that accepts any server certificate — for driving WPT (`wpt serve` uses a
/// self-signed cert) under `WPT_INSECURE_TLS` only. Signature checks still run against the provider.
mod insecure {
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::crypto::CryptoProvider;
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, Error, SignatureScheme};
    use std::sync::Arc;

    #[derive(Debug)]
    pub struct NoVerify(pub Arc<CryptoProvider>);

    impl ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, Error> {
            Ok(ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }
        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }
        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }
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

/// Response metadata (everything except the body), for streaming callers.
pub struct ResponseMeta {
    pub status: u16,
    pub content_type: String,
    pub final_url: String,
}

/// Result of [`fixup_url`]: the normalized URL plus whether we supplied a default `https://` scheme
/// (so the caller may fall back to `http://` if the https attempt can't connect).
pub struct Fixup {
    pub url: String,
    pub https_defaulted: bool,
}

/// Turn user-typed address-bar text into a URL, the way a browser's "URL fixup" does — shared by
/// every shell (Swift app, WebDriver, …) so address handling is identical across platforms:
///   * schemeless input ("example.com", "localhost:8080/x") becomes `https://…`, recorded in
///     `https_defaulted` so the caller can fall back to http on a *connection* failure;
///   * input that already carries a scheme passes through untouched — including the authority-less
///     schemes (`about:blank`, `data:…`, `mailto:…`, …) that have no `://` and must NOT be treated
///     as a bare host.
pub fn fixup_url(input: &str) -> Fixup {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Fixup {
            url: String::new(),
            https_defaulted: false,
        };
    }
    // `scheme://authority…` (http, https, file, ws, …): already a full URL.
    if trimmed.contains("://") {
        return Fixup {
            url: trimmed.to_string(),
            https_defaulted: false,
        };
    }
    // Schemes with no `://` authority are real URLs, not bare hosts.
    const SCHEMELESS: [&str; 7] = [
        "about:",
        "data:",
        "javascript:",
        "mailto:",
        "tel:",
        "blob:",
        "view-source:",
    ];
    let lower = trimmed.to_ascii_lowercase();
    if SCHEMELESS.iter().any(|s| lower.starts_with(s)) {
        return Fixup {
            url: trimmed.to_string(),
            https_defaulted: false,
        };
    }
    // A bare host defaults to https (with the caller free to fall back to http on connect failure).
    Fixup {
        url: format!("https://{trimmed}"),
        https_defaulted: true,
    }
}

/// Whether an `Err` from the fetch functions is a connection-level failure (DNS/connect/TLS/reset/
/// timeout) rather than an HTTP error *status*. Lets a defaulted-https navigation decide to retry
/// over http: an unreachable/refused https port falls back, a real 4xx/5xx page does not.
pub fn is_connection_error(err: &str) -> bool {
    err.starts_with("request failed:")
}

/// Whether HSTS currently forces https for `url`'s host (so a defaulted-https navigation must NOT
/// fall back to http for it).
pub fn hsts_pinned_url(url: &str) -> bool {
    host_of(url).map(|h| hsts::is_pinned(&h)).unwrap_or(false)
}

/// The lowercased registrable host of an `http(s)` URL (no port, no userinfo), or `None` for IP
/// literals / malformed authorities (which never carry HSTS).
fn host_of(url: &str) -> Option<String> {
    let after = url.split_once("://")?.1;
    let authority = after.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    // IPv6 literal (`[::1]`) or empty authority: not an HSTS host.
    if authority.is_empty() || authority.starts_with('[') {
        return None;
    }
    let host = authority.split(':').next().unwrap_or(authority);
    if host.is_empty() {
        return None;
    }
    Some(host.trim_end_matches('.').to_ascii_lowercase())
}

/// Rewrite an `http://` URL to `https://` when its host is HSTS-pinned; otherwise return it
/// unchanged. Applied to every request (documents AND subresources) so a pinned host is never
/// contacted over plaintext.
fn hsts_upgrade(url: &str) -> std::borrow::Cow<'_, str> {
    if let Some(rest) = url.strip_prefix("http://") {
        if let Some(host) = host_of(url) {
            if hsts::is_pinned(&host) {
                return std::borrow::Cow::Owned(format!("https://{rest}"));
            }
        }
    }
    std::borrow::Cow::Borrowed(url)
}

/// HTTP Strict Transport Security: remember hosts that sent a `Strict-Transport-Security` header
/// over https and force https for them thereafter (persisted across runs). A security control, so
/// it lives in the network layer and applies to every fetch.
mod hsts {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Entry {
        expiry: u64, // unix seconds
        include_subdomains: bool,
    }

    fn store() -> &'static Mutex<HashMap<String, Entry>> {
        static STORE: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
        STORE.get_or_init(|| Mutex::new(load()))
    }

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// Whether `host` (exact, or covered by an ancestor's `includeSubDomains`) is pinned to https.
    pub fn is_pinned(host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        let Ok(guard) = store().lock() else {
            return false;
        };
        let t = now();
        if let Some(e) = guard.get(&host) {
            if e.expiry > t {
                return true;
            }
        }
        // includeSubDomains: any unexpired ancestor entry that set the flag covers this host.
        let mut rest = host.as_str();
        while let Some(idx) = rest.find('.') {
            rest = &rest[idx + 1..];
            if let Some(e) = guard.get(rest) {
                if e.expiry > t && e.include_subdomains {
                    return true;
                }
            }
        }
        false
    }

    /// Record a `Strict-Transport-Security` header value for `host` (max-age=0 clears the pin).
    pub fn record(host: &str, header: &str) {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        let mut max_age: Option<u64> = None;
        let mut include_sub = false;
        for part in header.split(';') {
            let p = part.trim().to_ascii_lowercase();
            if let Some(v) = p.strip_prefix("max-age=") {
                max_age = v.trim().trim_matches('"').parse::<u64>().ok();
            } else if p == "includesubdomains" {
                include_sub = true;
            }
        }
        let Some(age) = max_age else {
            return; // a directive-less / max-age-less header is ignored
        };
        let Ok(mut guard) = store().lock() else {
            return;
        };
        if age == 0 {
            guard.remove(&host);
        } else {
            guard.insert(
                host,
                Entry {
                    expiry: now().saturating_add(age),
                    include_subdomains: include_sub,
                },
            );
        }
        save(&guard);
    }

    /// On-disk store path (one `host\texpiry\tincludeSubDomains` line per entry), under the same
    /// cache root as the disk cache; `None` (in-memory only) when caching is disabled.
    fn path() -> Option<std::path::PathBuf> {
        match std::env::var("NET_CACHE_DIR") {
            Ok(v) if v.is_empty() || v == "off" || v == "0" => return None,
            Ok(v) => return Some(std::path::PathBuf::from(v).join("hsts.txt")),
            Err(_) => {}
        }
        let home = std::env::var_os("HOME")?;
        Some(std::path::Path::new(&home).join("Library/Caches/dev.imlunahey.browser/hsts.txt"))
    }

    fn load() -> HashMap<String, Entry> {
        let mut map = HashMap::new();
        let Some(p) = path() else {
            return map;
        };
        let Ok(text) = std::fs::read_to_string(&p) else {
            return map;
        };
        let t = now();
        for line in text.lines() {
            let mut it = line.split('\t');
            if let (Some(h), Some(exp), Some(flag)) = (it.next(), it.next(), it.next()) {
                if let Ok(expiry) = exp.parse::<u64>() {
                    if expiry > t {
                        map.insert(
                            h.to_string(),
                            Entry {
                                expiry,
                                include_subdomains: flag == "1",
                            },
                        );
                    }
                }
            }
        }
        map
    }

    fn save(map: &HashMap<String, Entry>) {
        let Some(p) = path() else {
            return;
        };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut out = String::new();
        for (h, e) in map {
            out.push_str(h);
            out.push('\t');
            out.push_str(&e.expiry.to_string());
            out.push('\t');
            out.push(if e.include_subdomains { '1' } else { '0' });
            out.push('\n');
        }
        let _ = std::fs::write(&p, out);
    }
}

/// One entry in the network activity log (for the devtools Network tab).
#[derive(Clone)]
pub struct NetEntry {
    pub method: String,
    pub url: String,
    pub status: u16, // 0 = transport failure / no response
    pub ok: bool,
    pub duration_ms: u64,
    pub size: usize,
    pub content_type: String,
}

/// Global network activity log. Shared across the process (the engine clears it per navigation).
/// Bounded so a runaway page can't grow it without limit.
static NET_LOG: std::sync::Mutex<Vec<NetEntry>> = std::sync::Mutex::new(Vec::new());
const NET_LOG_CAP: usize = 1000;

/// Clear the network log (called by the engine on navigation).
pub fn clear_network_log() {
    if let Ok(mut log) = NET_LOG.lock() {
        log.clear();
    }
}

/// Snapshot of the network log.
pub fn network_log() -> Vec<NetEntry> {
    NET_LOG.lock().map(|l| l.clone()).unwrap_or_default()
}

fn record_net(entry: NetEntry) {
    if let Ok(mut log) = NET_LOG.lock() {
        if log.len() < NET_LOG_CAP {
            log.push(entry);
        }
    }
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
/// Records the request in the network log (for the devtools Network tab).
pub fn request(
    method: &str,
    url: &str,
    body: Option<&[u8]>,
    headers: &[(String, String)],
) -> Result<Response, String> {
    // Accumulate the streamed body into a Vec so the historical buffered `Response` surface is
    // identical. The shared core records the network log exactly once (here), so we pass
    // `log = true` and don't record again.
    let mut buf = Vec::new();
    let meta = request_streaming_core(method, url, body, headers, true, &mut |chunk| {
        buf.extend_from_slice(chunk);
    })?;
    Ok(Response {
        status: meta.status,
        content_type: meta.content_type,
        body: buf,
        final_url: meta.final_url,
    })
}

/// Perform `url` like `request("GET", ...)` but deliver the body INCREMENTALLY: `on_chunk` is
/// called with each block of bytes as it is read from the socket (do not buffer the whole body
/// first). Returns the response metadata once the body is fully read, or Err on failure.
pub fn fetch_streaming(url: &str, on_chunk: &mut dyn FnMut(&[u8])) -> Result<ResponseMeta, String> {
    // GET-only streaming: no request body or custom headers. Records the network log exactly once.
    request_streaming_core("GET", url, None, &[], true, on_chunk)
}

/// The single shared request path. Builds + sends the request (with the retry loop), then streams
/// the response body chunk-by-chunk through `on_chunk` (without buffering the whole body here).
/// When `log` is true, records exactly one network-log entry for this logical request. Returns the
/// response metadata, with `final_url` always set to the requested `url`.
fn request_streaming_core(
    method: &str,
    url: &str,
    body: Option<&[u8]>,
    headers: &[(String, String)],
    log: bool,
    on_chunk: &mut dyn FnMut(&[u8]),
) -> Result<ResponseMeta, String> {
    let start = std::time::Instant::now();
    // Count the total bytes streamed (sum of chunk lengths) for the network log.
    let mut total: usize = 0;
    let result = request_streaming_inner(method, url, body, headers, &mut |chunk| {
        total += chunk.len();
        on_chunk(chunk);
    });
    if log {
        let (status, ok, ct) = match &result {
            Ok(m) => (
                m.status,
                (200..300).contains(&m.status),
                m.content_type.clone(),
            ),
            Err(_) => (0u16, false, String::new()),
        };
        record_net(NetEntry {
            method: method.to_ascii_uppercase(),
            url: url.to_string(),
            status,
            ok,
            duration_ms: start.elapsed().as_millis() as u64,
            size: total,
            content_type: ct,
        });
    }
    result
}

/// Inner request implementation: handles `file://`, the disk cache, and the network send/retry
/// loop, then streams the response body through `on_chunk`. Does NOT touch the network log
/// (that's the caller's job, exactly once per logical request).
fn request_streaming_inner(
    method: &str,
    url: &str,
    body: Option<&[u8]>,
    headers: &[(String, String)],
    on_chunk: &mut dyn FnMut(&[u8]),
) -> Result<ResponseMeta, String> {
    let method_uc = method.to_ascii_uppercase();

    // HSTS: force https for any http URL whose host is pinned (documents AND subresources), so a
    // pinned host is never contacted over plaintext. No-op for non-http(s) URLs.
    let upgraded = hsts_upgrade(url);
    let url: &str = &upgraded;

    // `about:blank` (and bare `about:`) is the empty initial document every browsing context starts
    // on. There's no network involved — serve a minimal empty HTML document so the engine has a real
    // scriptable `about:blank` (used by new windows / WebDriver sessions before the first navigation).
    if url == "about:blank" || url == "about:" {
        let html = b"<!DOCTYPE html><html><head></head><body></body></html>";
        on_chunk(html);
        return Ok(ResponseMeta {
            status: 200,
            content_type: "text/html; charset=utf-8".to_string(),
            final_url: url.to_string(),
        });
    }

    if let Some(path) = url.strip_prefix("file://") {
        // file:// is a local read; method/body/headers don't apply. A local read isn't
        // meaningfully chunked, so deliver the whole content in a single `on_chunk` call.
        let resp = fetch_file(path, url)?;
        on_chunk(&resp.body);
        return Ok(ResponseMeta {
            status: resp.status,
            content_type: resp.content_type,
            final_url: resp.final_url,
        });
    }

    let is_get = method_uc == "GET";

    // On-disk cache (per-user OS cache dir by default; see `cache_dir`): serve a previously-cached
    // body so repeated loads don't re-hit the network. GET only (keyed by URL); non-GET bypasses it.
    let cache = if is_get { cache_path(url) } else { None };
    if let Some(p) = &cache {
        if let Ok(bytes) = std::fs::read(p) {
            // Cache hit. New-format entries carry the post-redirect final URL and content-type in a
            // small header (see the write path) so a cached navigation reports the SAME address as a
            // live one — e.g. en.wikipedia.org/ → /wiki/Main_Page. Legacy (header-less) entries fall
            // back to deriving both from the requested URL, the old behaviour.
            let (final_url, content_type, body_off) = match parse_cache_header(&bytes) {
                Some((u, ct, off)) => (u, ct, off),
                None => (url.to_string(), content_type_from_url(url), 0),
            };
            // Deliver the cached body in one chunk.
            on_chunk(&bytes[body_off..]);
            return Ok(ResponseMeta {
                status: 200,
                content_type,
                final_url,
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
        let backoff = |a: u32| {
            std::time::Duration::from_millis(match a {
                1 => 200,
                2 => 500,
                _ => 1000,
            })
        };
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
    // The URL the response actually came from — ureq follows redirects, so this is the post-redirect
    // location (e.g. en.wikipedia.org/ → /wiki/Main_Page). Falls back to the requested URL.
    let final_url = {
        let u = resp.get_url();
        if u.is_empty() {
            url.to_string()
        } else {
            u.to_string()
        }
    };
    let content_type = resp
        .header("Content-Type")
        .unwrap_or("application/octet-stream")
        .to_string();

    // Record HSTS pins, but only from an https response (a header sent over plain http is ignored
    // per the spec, since it could be injected by a network attacker).
    if url.starts_with("https://") {
        if let Some(sts) = resp.header("Strict-Transport-Security") {
            if let Some(host) = host_of(url) {
                hsts::record(&host, sts);
            }
        }
    }

    // Stream the body: read into a fixed buffer and hand each block to `on_chunk` as it arrives,
    // never buffering the whole body here. On a cache MISS we accumulate a copy locally so the
    // disk cache can still be populated after a successful read.
    let want_cache = status == 200 && cache.is_some();
    let mut cache_buf: Vec<u8> = Vec::new();
    let mut reader = resp.into_reader().take(MAX_BODY_BYTES);
    let mut buf = [0u8; 32 * 1024];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(n) => n,
            // Many servers (e.g. Python's wptserve) close the TLS connection without sending the
            // `close_notify` alert; rustls reports that as `UnexpectedEof`. The HTTP body has already
            // been received (and streamed via `on_chunk`), so — like real browsers — treat the
            // unclean close as a clean end-of-stream instead of a fatal load error.
            Err(e)
                if e.kind() == std::io::ErrorKind::UnexpectedEof
                    || e.to_string().contains("close_notify") =>
            {
                break;
            }
            Err(e) => return Err(format!("failed to read body: {e}")),
        };
        if n == 0 {
            break;
        }
        if want_cache {
            cache_buf.extend_from_slice(&buf[..n]);
        }
        on_chunk(&buf[..n]);
    }

    // Populate the opt-in disk cache on success (GET only). Persist the post-redirect final URL and
    // content-type in a header ahead of the body, so a later cache hit reports the same address (and
    // type) as a live load instead of the requested URL. URLs and content-types never contain a
    // newline, so the line-delimited header is unambiguous.
    if want_cache {
        if let Some(p) = &cache {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let mut entry = Vec::with_capacity(
                CACHE_MAGIC.len() + final_url.len() + content_type.len() + 2 + cache_buf.len(),
            );
            entry.extend_from_slice(CACHE_MAGIC);
            entry.extend_from_slice(final_url.as_bytes());
            entry.push(b'\n');
            entry.extend_from_slice(content_type.as_bytes());
            entry.push(b'\n');
            entry.extend_from_slice(&cache_buf);
            let _ = std::fs::write(p, &entry);
        }
    }

    Ok(ResponseMeta {
        status,
        content_type,
        final_url,
    })
}

/// Root directory of the on-disk HTTP cache. Like other browsers, this defaults to a per-user OS
/// cache location (macOS: `~/Library/Caches/dev.imlunahey.browser/net`) — never the working
/// directory. `NET_CACHE_DIR` overrides it; setting it empty / `off` / `0` disables the cache.
fn cache_dir() -> Option<std::path::PathBuf> {
    match std::env::var("NET_CACHE_DIR") {
        Ok(v) if v.is_empty() || v == "off" || v == "0" => return None,
        Ok(v) => return Some(std::path::PathBuf::from(v)),
        Err(_) => {}
    }
    let home = std::env::var_os("HOME")?;
    Some(std::path::Path::new(&home).join("Library/Caches/dev.imlunahey.browser/net"))
}

/// Magic prefix marking a cache entry that carries a metadata header (final URL + content-type)
/// ahead of the body. The trailing digit is the format version; bump it when the header layout
/// changes so older builds don't misread a newer entry.
const CACHE_MAGIC: &[u8] = b"BRWC1\n";

/// Parse the header of a metadata-carrying cache entry: [`CACHE_MAGIC`], then `final_url\n`, then
/// `content_type\n`, then the raw body. Returns `(final_url, content_type, body_offset)`, or `None`
/// for a legacy (body-only) entry written before the header existed.
fn parse_cache_header(bytes: &[u8]) -> Option<(String, String, usize)> {
    let rest = bytes.strip_prefix(CACHE_MAGIC)?;
    let nl1 = rest.iter().position(|&b| b == b'\n')?;
    let final_url = std::str::from_utf8(&rest[..nl1]).ok()?.to_string();
    let after = &rest[nl1 + 1..];
    let nl2 = after.iter().position(|&b| b == b'\n')?;
    let content_type = std::str::from_utf8(&after[..nl2]).ok()?.to_string();
    let body_offset = CACHE_MAGIC.len() + nl1 + 1 + nl2 + 1;
    Some((final_url, content_type, body_offset))
}

/// Disk-cache file path for `url` (a stable hash of the URL under [`cache_dir`]), or `None` when the
/// cache is disabled or the URL shouldn't be cached.
fn cache_path(url: &str) -> Option<std::path::PathBuf> {
    // Never disk-cache local dev servers (e.g. the WPT runner): they serve mutable content at stable
    // URLs, so a cache hit would mask edits. This includes the WPT hostnames (`web-platform.test` &
    // friends), which resolve to loopback and serve per-run-regenerated tests/endpoints — caching
    // them replays a previous run's body and silently corrupts conformance results.
    if url.contains("://localhost")
        || url.contains("://127.0.0.1")
        || url.contains("://[::1]")
        || url.contains("web-platform.test")
    {
        return None;
    }
    let dir = cache_dir()?;
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    // Fold the format version into the key so a layout change (e.g. adding the metadata header)
    // produces fresh filenames — older body-only entries are simply never looked up again, rather
    // than read and misinterpreted (which would resurrect the requested-URL-as-final_url bug).
    CACHE_MAGIC.hash(&mut h);
    url.hash(&mut h);
    Some(dir.join(format!("{:016x}", h.finish())))
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

/// Convert the portion of a `file://` URL after the scheme into a filesystem path. On Windows a
/// `file:///C:/dir/x` URL leaves `/C:/dir/x` here; the leading slash before the drive letter must be
/// removed (`C:/dir/x`) or the OS rejects it (error 123, invalid name). A no-op on Unix.
fn file_url_to_path(path: &str) -> &str {
    #[cfg(windows)]
    {
        let b = path.as_bytes();
        if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
            return &path[1..];
        }
    }
    path
}

/// Read a `file://` URL from local disk. `path` is the part after `file://`.
fn fetch_file(path: &str, original: &str) -> Result<Response, String> {
    let path = file_url_to_path(path);
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
    Ok(Response {
        status: 200,
        content_type,
        body,
        final_url: original.to_string(),
    })
}

// ---------------------------------------------------------------------------------------------
// WebSocket client (pure Rust, via `tungstenite`).
//
// Mirrors the async-fetch model: the JS runtime spawns a dedicated thread per socket that runs
// [`ws_run`] for the whole lifetime of the connection. The thread talks to the rest of the engine
// over two `std::sync::mpsc` channels carrying PRIMITIVE tuples only (so the `js` crate never has to
// depend on `net`):
//   * `evt_tx`  — events FROM the socket: `(id, kind, payload)`.
//       kind 0 = open       (payload "")
//       kind 1 = text msg   (payload = the UTF-8 text)
//       kind 2 = binary msg (payload = base64 of the bytes — a binary message is bridged as base64)
//       kind 3 = close      (payload = "code:reason")
//       kind 4 = error      (payload = a human-readable message)
//   * `out_rx`  — outgoing commands TO the socket: `(kind, payload)`.
//       kind 0 = send text   (payload = the text)
//       kind 1 = send binary (payload = base64 of the bytes)
//       kind 2 = close
//
// The loop is a non-blocking poll: drain any queued outgoing commands, attempt one non-blocking
// read, then sleep ~10ms so we never busy-spin. `WouldBlock` from the read is the normal "no data
// yet" case and is ignored.
// ---------------------------------------------------------------------------------------------

use base64::Engine as _;

/// How long to wait for the TCP connect + WebSocket handshake before giving up.
const WS_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Idle sleep between poll iterations so a quiet socket doesn't burn a core.
const WS_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

/// Run a whole WebSocket connection on the CALLING thread (the engine/Session spawns the thread).
/// Handles both `ws://` and `wss://`. See the module-level comment above for the tuple protocol.
///
/// On a fatal error (connect/handshake failure or a mid-stream socket error) this emits an `error`
/// event (kind 4) followed by a `close` event (kind 3) and returns, so the JS object fires
/// `onerror` then `onclose`. Binary frames are base64-bridged across the primitive channel.
pub fn ws_run(
    url: String,
    id: u64,
    evt_tx: std::sync::mpsc::Sender<(u64, u8, String)>,
    out_rx: std::sync::mpsc::Receiver<(u8, String)>,
) {
    use tungstenite::Message;

    // Helper: emit error + close, then we're done.
    let fail = |evt_tx: &std::sync::mpsc::Sender<(u64, u8, String)>, msg: String| {
        let _ = evt_tx.send((id, 4, msg));
        let _ = evt_tx.send((id, 3, "1006:".to_string()));
    };

    // --- Connect (bounded). We resolve + connect the TCP socket ourselves with a timeout so a dead
    // host can't hang this thread forever, then let tungstenite do the (TLS +) WS handshake. The
    // read timeout bounds the handshake; we switch to non-blocking once it succeeds.
    let mut socket = match ws_connect(&url) {
        Ok(s) => s,
        Err(e) => {
            fail(&evt_tx, e);
            return;
        }
    };

    // Make the underlying TcpStream non-blocking so `read()` returns `WouldBlock` instead of
    // parking the thread (which would stall outgoing sends).
    if let Err(e) = set_ws_nonblocking(socket.get_mut(), true) {
        fail(&evt_tx, format!("failed to set non-blocking: {e}"));
        return;
    }

    // Handshake done: the socket is open.
    let _ = evt_tx.send((id, 0, String::new()));

    loop {
        // (a) Flush any queued outgoing commands.
        let mut closing = false;
        loop {
            match out_rx.try_recv() {
                Ok((0, text)) => {
                    if socket.send(Message::Text(text)).is_err() {
                        let _ = evt_tx.send((id, 4, "send failed".to_string()));
                        closing = true;
                        break;
                    }
                }
                Ok((1, b64)) => {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(b64.as_bytes())
                        .unwrap_or_default();
                    if socket.send(Message::Binary(bytes)).is_err() {
                        let _ = evt_tx.send((id, 4, "send failed".to_string()));
                        closing = true;
                        break;
                    }
                }
                Ok((2, _)) | Ok(_) => {
                    let _ = socket.close(None);
                    closing = true;
                    break;
                }
                // No more queued commands right now.
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                // The sender (the JS object's out-channel) was dropped: the page is gone. Close.
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    let _ = socket.close(None);
                    closing = true;
                    break;
                }
            }
        }
        let _ = socket.flush();

        // (b) Attempt one non-blocking read.
        match socket.read() {
            Ok(Message::Text(t)) => {
                let _ = evt_tx.send((id, 1, t));
            }
            Ok(Message::Binary(b)) => {
                let b64 = base64::engine::general_purpose::STANDARD.encode(&b);
                let _ = evt_tx.send((id, 2, b64));
            }
            Ok(Message::Close(frame)) => {
                let payload = match frame {
                    Some(f) => format!("{}:{}", u16::from(f.code), f.reason),
                    None => "1005:".to_string(),
                };
                let _ = evt_tx.send((id, 3, payload));
                return;
            }
            // Ping/Pong are handled by tungstenite internally on the next send/read; ignore.
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // No data available yet — the normal idle case.
            }
            Err(tungstenite::Error::ConnectionClosed) | Err(tungstenite::Error::AlreadyClosed) => {
                let _ = evt_tx.send((id, 3, "1000:".to_string()));
                return;
            }
            Err(e) => {
                let _ = evt_tx.send((id, 4, format!("{e}")));
                let _ = evt_tx.send((id, 3, "1006:".to_string()));
                return;
            }
        }

        // If we initiated a close above, give the close handshake one read cycle then exit.
        if closing {
            let _ = socket.flush();
            let _ = evt_tx.send((id, 3, "1000:".to_string()));
            return;
        }

        // (c) Don't busy-spin.
        std::thread::sleep(WS_POLL_INTERVAL);
    }
}

/// Connect + run the WebSocket handshake for `url`, with a bounded TCP connect/handshake. Returns
/// the live socket (TLS already negotiated for `wss://`) or an error string.
fn ws_connect(
    url: &str,
) -> Result<tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>, String>
{
    use std::net::{TcpStream, ToSocketAddrs};

    let parsed = url::Url::parse(url).map_err(|e| format!("invalid WebSocket URL: {e}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| "WebSocket URL has no host".to_string())?
        .to_string();
    let port = parsed
        .port_or_known_default()
        .unwrap_or(match parsed.scheme() {
            "wss" => 443,
            _ => 80,
        });

    // Resolve + connect with a timeout so a dead host can't hang the thread indefinitely.
    let addrs: Vec<_> = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| format!("DNS resolution failed for {host}: {e}"))?
        .collect();
    let mut last_err = "no addresses resolved".to_string();
    let mut stream: Option<TcpStream> = None;
    for addr in &addrs {
        match TcpStream::connect_timeout(addr, WS_CONNECT_TIMEOUT) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last_err = format!("connect to {addr} failed: {e}"),
        }
    }
    let stream = stream.ok_or(last_err)?;
    // Bound the handshake reads/writes too (cleared to non-blocking by the caller on success).
    let _ = stream.set_read_timeout(Some(WS_CONNECT_TIMEOUT));
    let _ = stream.set_write_timeout(Some(WS_CONNECT_TIMEOUT));

    // tungstenite upgrades to TLS itself for `wss://` (rustls + webpki roots).
    match tungstenite::client_tls(url, stream) {
        Ok((socket, _resp)) => Ok(socket),
        Err(e) => Err(format!("WebSocket handshake failed: {e}")),
    }
}

/// Set the underlying `TcpStream` (whether plain or wrapped in rustls) to (non-)blocking mode.
fn set_ws_nonblocking(
    stream: &mut tungstenite::stream::MaybeTlsStream<std::net::TcpStream>,
    nonblocking: bool,
) -> std::io::Result<()> {
    match stream {
        tungstenite::stream::MaybeTlsStream::Plain(s) => s.set_nonblocking(nonblocking),
        tungstenite::stream::MaybeTlsStream::Rustls(s) => s.get_mut().set_nonblocking(nonblocking),
        // Other variants aren't reachable with our feature set, but stay non-fatal.
        _ => Ok(()),
    }
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
    fn fixup_defaults_bare_host_to_https() {
        let f = fixup_url("example.com");
        assert_eq!(f.url, "https://example.com");
        assert!(f.https_defaulted);

        let f = fixup_url("  localhost:8080/x  "); // trims, keeps port + path
        assert_eq!(f.url, "https://localhost:8080/x");
        assert!(f.https_defaulted);
    }

    #[test]
    fn fixup_preserves_explicit_and_authorityless_schemes() {
        for s in [
            "http://httpforever.com",
            "https://example.com",
            "file:///tmp/x.html",
            "about:blank",
            "About:Blank", // scheme match is case-insensitive
            "data:text/html,hi",
            "mailto:a@b.com",
            "view-source:https://example.com",
        ] {
            let f = fixup_url(s);
            assert_eq!(f.url, s.trim());
            assert!(!f.https_defaulted, "{s} must not be marked https-defaulted");
        }
        assert_eq!(fixup_url("   ").url, "");
    }

    #[test]
    fn connection_errors_are_distinguished_from_statuses() {
        assert!(is_connection_error("request failed: connection refused"));
        assert!(!is_connection_error("HTTP error status 404 for https://x"));
        assert!(!is_connection_error("failed to read body: reset"));
    }

    #[test]
    fn host_of_extracts_host_without_port_or_userinfo() {
        assert_eq!(
            host_of("https://Example.COM/path").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            host_of("http://user:pw@example.com:8080/x").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            host_of("https://example.com.").as_deref(),
            Some("example.com")
        ); // trailing dot
        assert_eq!(host_of("https://[::1]:443/x"), None); // IPv6 literal: no HSTS
        assert_eq!(host_of("about:blank"), None);
    }

    #[test]
    fn hsts_records_pins_and_covers_subdomains() {
        // Use an isolated in-memory store (no persistence) for the test.
        std::env::set_var("NET_CACHE_DIR", "off");
        let host = "hsts-test-example.invalid";
        let sub = "deep.sub.hsts-test-example.invalid";
        assert!(!hsts::is_pinned(host));

        hsts::record(host, "max-age=31536000; includeSubDomains");
        assert!(hsts::is_pinned(host));
        assert!(hsts::is_pinned(sub)); // includeSubDomains covers descendants
        assert!(hsts_pinned_url(&format!("http://{host}/x")));

        // hsts_upgrade rewrites http→https for a pinned host, leaves https/others alone.
        assert_eq!(
            hsts_upgrade(&format!("http://{host}/x")),
            format!("https://{host}/x")
        );
        assert_eq!(
            hsts_upgrade("http://not-pinned.invalid/x"),
            "http://not-pinned.invalid/x"
        );

        // max-age=0 clears the pin.
        hsts::record(host, "max-age=0");
        assert!(!hsts::is_pinned(host));
        assert!(!hsts::is_pinned(sub));
    }

    #[test]
    fn missing_file_is_err() {
        assert!(fetch("file:///nonexistent/path/xyz.html").is_err());
    }

    #[test]
    fn body_cap_exceeds_4gib() {
        // Tabs are not capped at 4 GiB; the body backstop must sit comfortably above it.
        const _: () = assert!(MAX_BODY_BYTES > 4 * 1024 * 1024 * 1024);
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

    #[test]
    fn fetch_streaming_file_concatenates_to_contents() {
        // Streaming a file:// URL: concatenated chunks equal the file, status is 200.
        let mut path = std::env::temp_dir();
        path.push(format!("net_stream_small_{}.txt", std::process::id()));
        std::fs::write(&path, b"streamed contents").unwrap();
        let url = format!("file://{}", path.display());

        let mut acc = Vec::new();
        let meta =
            fetch_streaming(&url, &mut |chunk| acc.extend_from_slice(chunk)).expect("stream");
        assert_eq!(acc, b"streamed contents");
        assert_eq!(meta.status, 200);
        assert_eq!(meta.final_url, url);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn cache_header_round_trips_and_falls_back() {
        // A new-format entry: the header carries the post-redirect URL + content-type, the body
        // follows it byte-for-byte (the redirect target is recovered on a cache hit).
        let mut entry = Vec::new();
        entry.extend_from_slice(CACHE_MAGIC);
        entry.extend_from_slice(b"https://en.wikipedia.org/wiki/Main_Page\n");
        entry.extend_from_slice(b"text/html; charset=utf-8\n");
        entry.extend_from_slice(b"<!doctype html>body");
        let (url, ct, off) = parse_cache_header(&entry).expect("header parses");
        assert_eq!(url, "https://en.wikipedia.org/wiki/Main_Page");
        assert_eq!(ct, "text/html; charset=utf-8");
        assert_eq!(&entry[off..], b"<!doctype html>body");

        // A legacy body-only entry (no magic) → None, so the caller derives both from the requested
        // URL exactly as before. A body that merely starts like the magic but isn't is also legacy.
        assert!(parse_cache_header(b"<!doctype html>raw body").is_none());
        assert!(parse_cache_header(b"BRWC1").is_none());
    }

    #[test]
    fn fetch_streaming_large_file_round_trips() {
        // A body larger than the 32 KiB read buffer. Per the spec, file:// delivers in a single
        // chunk, so we assert the single-chunk accumulation equals the full file contents
        // (kept offline/deterministic — no live network).
        let mut path = std::env::temp_dir();
        path.push(format!("net_stream_large_{}.bin", std::process::id()));
        let data: Vec<u8> = (0..100 * 1024).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let url = format!("file://{}", path.display());

        let mut acc = Vec::new();
        let mut chunks = 0usize;
        let meta = fetch_streaming(&url, &mut |chunk| {
            chunks += 1;
            acc.extend_from_slice(chunk);
        })
        .expect("stream large");
        assert_eq!(acc, data);
        assert_eq!(meta.status, 200);
        // file:// is delivered in exactly one chunk per the spec.
        assert_eq!(chunks, 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ws_run_unreachable_emits_error_then_close() {
        // Deterministic + offline: connecting to a port nothing listens on must produce an error
        // event (kind 4) followed by a close event (kind 3) — never an open. This is the same
        // failure path the JS WebSocket relies on to fire onerror/onclose.
        let (evt_tx, evt_rx) = std::sync::mpsc::channel::<(u64, u8, String)>();
        let (_out_tx, out_rx) = std::sync::mpsc::channel::<(u8, String)>();
        ws_run("ws://127.0.0.1:1/".to_string(), 7, evt_tx, out_rx);
        let events: Vec<_> = evt_rx.try_iter().collect();
        assert!(
            events.iter().any(|(_, k, _)| *k == 4),
            "expected an error event, got {events:?}"
        );
        assert!(
            events.iter().any(|(_, k, _)| *k == 3),
            "expected a close event, got {events:?}"
        );
        assert!(
            !events.iter().any(|(_, k, _)| *k == 0),
            "must not open on an unreachable host, got {events:?}"
        );
    }

    #[test]
    #[ignore = "requires network; run manually with --ignored"]
    fn ws_run_echo_round_trips() {
        // Manual/online check against a public echo server. Tolerant: either we round-trip the
        // message, or we cleanly error (no network) — we never hang or panic.
        let (evt_tx, evt_rx) = std::sync::mpsc::channel::<(u64, u8, String)>();
        let (out_tx, out_rx) = std::sync::mpsc::channel::<(u8, String)>();
        let handle = std::thread::spawn(move || {
            ws_run(
                "wss://ws.postman-echo.com/raw".to_string(),
                1,
                evt_tx,
                out_rx,
            );
        });
        // Wait for open (or error), bounded.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(12);
        let mut opened = false;
        while std::time::Instant::now() < deadline {
            if let Ok((_, kind, _)) = evt_rx.recv_timeout(std::time::Duration::from_millis(200)) {
                if kind == 0 {
                    opened = true;
                    break;
                }
                if kind == 4 || kind == 3 {
                    // Clean failure (no network): acceptable.
                    let _ = out_tx.send((2, String::new()));
                    let _ = handle.join();
                    return;
                }
            }
        }
        assert!(opened, "did not open within budget");
        let _ = out_tx.send((0, "hello".to_string()));
        let mut echoed = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        while std::time::Instant::now() < deadline {
            if let Ok((_, kind, payload)) =
                evt_rx.recv_timeout(std::time::Duration::from_millis(200))
            {
                if kind == 1 && payload == "hello" {
                    echoed = true;
                    break;
                }
            }
        }
        let _ = out_tx.send((2, String::new()));
        let _ = handle.join();
        assert!(echoed, "echo server did not return our message");
    }

    #[test]
    fn request_get_file_body_unchanged_regression() {
        // Regression: after refactoring `request` to share the streaming core, `request("GET", …)`
        // still returns the full body identical to the file contents.
        let mut path = std::env::temp_dir();
        path.push(format!("net_stream_regression_{}.txt", std::process::id()));
        let data: Vec<u8> = (0..70 * 1024).map(|i| (i % 97) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let url = format!("file://{}", path.display());

        let r = request("GET", &url, None, &[]).expect("file read");
        assert_eq!(r.body, data);
        assert_eq!(r.status, 200);
        let _ = std::fs::remove_file(&path);
    }
}
