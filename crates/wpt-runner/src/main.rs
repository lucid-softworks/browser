//! A lightweight Web Platform Tests runner for our engine.
//!
//! WPT's official harness drives a browser over WebDriver; we don't have one yet, so this runs
//! `testharness.js` tests *in-process*: a tiny static HTTP server serves a WPT checkout (so the
//! tests' `/resources/...` references resolve), with our own `testharnessreport.js` injected to
//! disable DOM output and stash the results on `window`. For each test the engine loads the URL,
//! ticks the event loop until the harness completes (or times out), and we read the result counts.
//!
//! Usage: `wpt-runner <wpt-root> <subpath> [max-tests]`
//!   e.g. `wpt-runner ./wpt dom/nodes 200`

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Host `wpt serve` serves the primary origin on (its default `browser_host`). Must be mapped to a
/// loopback address in `/etc/hosts` (see the README) — `wpt serve` resolves it to bind, and our
/// engine resolves it to fetch.
const WPT_HOST: &str = "web-platform.test";
const WPT_HTTP_PORT: u16 = 8000;
const WPT_HTTPS_PORT: u16 = 8443;

/// Our replacement for WPT's `testharnessreport.js`: turn off the harness's DOM output (it would
/// `appendChild` into a `#log`/body that test fixtures often lack) and capture the structured
/// results onto `window` so the runner can read them via `console_eval`.
const REPORT_JS: &str = r#"
setup({ output: false });
add_completion_callback(function (tests, status) {
  var c = [0, 0, 0, 0];
  for (var i = 0; i < tests.length; i++) { c[tests[i].status] = (c[tests[i].status] || 0) + 1; }
  window.__wpt_pass = c[0]; window.__wpt_fail = c[1];
  window.__wpt_timeout = c[2]; window.__wpt_notrun = c[3];
  window.__wpt_total = tests.length;
  window.__wpt_harness = status.status;       // 0 OK, 1 ERROR, 2 TIMEOUT
  window.__wpt_harness_msg = status.message || "";
  // First failure detail, for quick triage.
  window.__wpt_firstfail = "";
  window.__wpt_allfails = "";
  for (var j = 0; j < tests.length; j++) {
    if (tests[j].status === 1) {
      if (!window.__wpt_firstfail) { window.__wpt_firstfail = tests[j].name + ": " + (tests[j].message || ""); }
      window.__wpt_allfails += tests[j].name + " :: " + (tests[j].message || "") + "\n";
    }
  }
  window.__wpt_done = 1;
});
"#;

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" | "htm" | "xht" | "xhtml" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
}

/// Minimal blocking static file server. One request per connection (`Connection: close`). The
/// `/resources/testharnessreport.js` path is overridden with [`REPORT_JS`]; everything else is read
/// from `root`.
/// Substitute WPT server-variable templates (`{{host}}`, `{{domains[...]}}`, `{{ports[...][...]}}`,
/// `{{location[...]}}`) used in `.sub.*` files. We serve a single origin, so every host/domain maps
/// to 127.0.0.1 and every port to our ephemeral port. Unknown templates are passed through.
fn wpt_subst(s: &str, port: u16) -> String {
    let host = "127.0.0.1";
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let var = after[..end].trim();
                if var == "host" || var == "domains" || var.starts_with("domains[") {
                    out.push_str(host);
                } else if var.starts_with("ports[") {
                    out.push_str(&port.to_string());
                } else if var.starts_with("location[") {
                    out.push_str(host);
                    out.push(':');
                    out.push_str(&port.to_string());
                } else {
                    out.push_str("{{");
                    out.push_str(&after[..end]);
                    out.push_str("}}");
                }
                rest = &after[end + 2..];
            }
            None => {
                out.push_str("{{");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn serve(stream: &mut TcpStream, root: &Path, port: u16) {
    let mut buf = [0u8; 8192];
    let n = match stream.read(&mut buf) {
        Ok(n) if n > 0 => n,
        _ => return,
    };
    let req = String::from_utf8_lossy(&buf[..n]);
    let path = req
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/");
    // Strip query/fragment + percent-decode spaces.
    let path = path.split(['?', '#']).next().unwrap_or("/");
    let path = path.replace("%20", " ");

    let (body, ctype, extra): (Vec<u8>, String, String) = if path
        == "/resources/testharnessreport.js"
    {
        (
            REPORT_JS.as_bytes().to_vec(),
            "text/javascript; charset=utf-8".to_string(),
            String::new(),
        )
    } else {
        let rel = path.trim_start_matches('/');
        let full = root.join(rel);
        // Contain within root.
        if !full.starts_with(root) {
            let _ = stream.write_all(
                b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            return;
        }
        match std::fs::read(&full) {
            Ok(b) => {
                // `.sub.*` files carry WPT server-variable templates that must be substituted.
                let body = if path.contains(".sub.") {
                    wpt_subst(&String::from_utf8_lossy(&b), port).into_bytes()
                } else {
                    b
                };
                // A WPT `.headers` sidecar (`<file>.headers`) overrides the Content-Type and adds
                // response headers (e.g. `X-Content-Type-Options: nosniff`).
                let mut ct = content_type(&path).to_string();
                let mut extra = String::new();
                if let Ok(htext) = std::fs::read_to_string(format!("{}.headers", full.display())) {
                    for line in htext.lines() {
                        let line = line.trim();
                        if line.is_empty() || line.starts_with('#') {
                            continue;
                        }
                        if let Some((k, v)) = line.split_once(':') {
                            let (k, v) = (k.trim(), v.trim());
                            if k.eq_ignore_ascii_case("content-type") {
                                ct = v.to_string();
                            } else {
                                extra.push_str(k);
                                extra.push_str(": ");
                                extra.push_str(v);
                                extra.push_str("\r\n");
                            }
                        }
                    }
                }
                (body, ct, extra)
            }
            Err(_) => {
                let _ = stream.write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                return;
            }
        }
    };
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(&body);
}

/// Where tests are served from. Default is our built-in single-origin static server; with
/// `WPT_WPTSERVE=1` we drive the real `wpt serve` instead — it runs the `.py` endpoints, does full
/// `.sub` substitution, generates the `.any`/`.window`/`.worker` variants, and serves multiple
/// origins + `.https`, which the static server can't.
enum Backend {
    /// Built-in static file server on an ephemeral loopback port (http only).
    Static(u16),
    /// A spawned `wpt serve` child. On drop we restore the overridden report file and kill the child.
    WptServe {
        child: Child,
        /// `<root>/resources/testharnessreport.js`, overridden with [`REPORT_JS`] while we run.
        report_path: PathBuf,
        /// The file's original bytes, restored on drop so the checkout is left untouched.
        report_orig: Vec<u8>,
    },
}

impl Backend {
    /// Absolute base origin for a test whose name is `name`. `wpt serve` decides scheme by filename:
    /// `*.https.*` tests must be loaded over TLS from a secure context.
    fn origin(&self, name: &str) -> String {
        match self {
            Backend::Static(port) => format!("http://127.0.0.1:{port}"),
            Backend::WptServe { .. } if name.contains(".https.") => {
                format!("https://{WPT_HOST}:{WPT_HTTPS_PORT}")
            }
            Backend::WptServe { .. } => format!("http://{WPT_HOST}:{WPT_HTTP_PORT}"),
        }
    }

    fn is_wptserve(&self) -> bool {
        matches!(self, Backend::WptServe { .. })
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        if let Backend::WptServe {
            child,
            report_path,
            report_orig,
        } = self
        {
            let _ = std::fs::write(report_path, report_orig);
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The (short-name, root-relative-url-path) pairs to actually load for a discovered test file.
///
/// Plain `.html` tests map to themselves. `.any.js`/`.window.js`/`.worker.js` aren't files the
/// browser loads directly — `wpt serve` *generates* HTML wrappers for them — so in `wpt serve` mode
/// we emit those generated URLs (the window-context variants; `*.worker.html`/`*.any.worker.html`
/// are skipped for now since our worker support is thin and they'd mostly time out). In static mode
/// the `.js` forms yield nothing, since our static server can't generate them.
fn test_variants(rel: &str, wptserve: bool) -> Vec<String> {
    for (suffix, gen) in [
        (".any.js", ".any.html"),
        (".window.js", ".window.html"),
        (".worker.js", ".worker.html"),
    ] {
        if let Some(stem) = rel.strip_suffix(suffix) {
            return if wptserve {
                vec![format!("{stem}{gen}")]
            } else {
                vec![]
            };
        }
    }
    vec![rel.to_string()]
}

/// Spawn `wpt serve` rooted at the WPT checkout. Instead of the buggy `--inject-script` path (its
/// wrapper 500s on any response lacking a Content-Type, e.g. `record-headers.py`'s store), we swap
/// the checkout's `resources/testharnessreport.js` for [`REPORT_JS`] — the report hook
/// `testharness.js` loads right after itself, restored on drop. Blocks until the http port accepts
/// connections (or panics on timeout — almost always a missing `/etc/hosts` map).
fn start_wptserve(root: &Path) -> Backend {
    let report_path = root.join("resources/testharnessreport.js");
    let report_orig = std::fs::read(&report_path).expect("read testharnessreport.js");
    std::fs::write(&report_path, REPORT_JS).expect("override testharnessreport.js");
    let log = std::fs::File::create("/tmp/wptserve.log").expect("create wptserve log");

    // The child is owned by the returned `Backend`, whose `Drop` kills and `wait()`s it; clippy
    // can't see that through the enum, so silence the zombie-process lint here.
    #[allow(clippy::zombie_processes)]
    let child = Command::new("python3")
        .current_dir(root)
        .arg("./wpt")
        .arg("serve")
        // h2 needs extra deps and a separate origin; conformance tests we target don't require it.
        .arg("--no-h2")
        .stdout(Stdio::from(log.try_clone().expect("clone log")))
        .stderr(Stdio::from(log))
        .spawn()
        .expect("spawn `wpt serve` (is python3 on PATH and the WPT `tools/` checked out?)");

    // Wait for readiness. `wpt serve` first builds a venv + installs deps on a cold checkout, so be
    // generous. It binds `WPT_HOST`, which must resolve via /etc/hosts.
    let deadline = Instant::now() + Duration::from_secs(120);
    let addr = format!("{WPT_HOST}:{WPT_HTTP_PORT}");
    while Instant::now() < deadline {
        if let Ok(sock) = addr.to_socket_addrs().map(|mut a| a.next()) {
            if let Some(sock) = sock {
                if TcpStream::connect_timeout(&sock, Duration::from_millis(500)).is_ok() {
                    return Backend::WptServe {
                        child,
                        report_path,
                        report_orig,
                    };
                }
            }
        } else {
            // Restore before bailing so a failed start doesn't leave the checkout modified.
            let _ = std::fs::write(&report_path, &report_orig);
            panic!(
                "cannot resolve {WPT_HOST} — map the WPT subdomains to 127.0.0.1 in /etc/hosts \
                 (`cd wpt && python3 ./wpt make-hosts-file | sudo tee -a /etc/hosts`)"
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let _ = std::fs::write(&report_path, &report_orig);
    panic!("`wpt serve` did not come up within 120s — see /tmp/wptserve.log");
}

/// Recursively collect runnable testharness files under `dir`, skipping the obvious non-tests
/// (references, manual tests, support files, the resources dir).
fn collect_tests(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for e in entries.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if p.is_dir() {
            // `tentative/` holds unstandardized proposals — not part of the conformance metric.
            if matches!(
                name.as_str(),
                "support" | "resources" | "tools" | "META.yml" | "tentative"
            ) {
                continue;
            }
            collect_tests(&p, out);
        } else if name.ends_with(".html") || name.ends_with(".xht") || name.ends_with(".xhtml") {
            // Skip reftest references, manual tests, non-harness helpers, and `*.tentative.*` files.
            if name.contains("-ref.")
                || name.ends_with("-ref.html")
                || name.contains("-manual.")
                || name.contains(".tentative.")
            {
                continue;
            }
            out.push(p);
        } else if name.ends_with(".any.js")
            || name.ends_with(".window.js")
            || name.ends_with(".worker.js")
        {
            // testharness `.js` tests: `wpt serve` generates HTML wrappers for these (see
            // `test_variants`). Skipped in static mode, which can't generate them.
            if name.contains(".tentative.") {
                continue;
            }
            out.push(p);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: wpt-runner <wpt-root> <subpath> [max-tests]");
        std::process::exit(2);
    }
    let root = std::fs::canonicalize(&args[1]).expect("wpt-root not found");
    let root = Arc::new(root);
    let subpath = &args[2];
    let max: usize = args
        .get(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(usize::MAX);

    // Pick a serving backend. Default: our built-in static server (single-origin, http only,
    // dependency-free) — keeps the default CI run unchanged. Opt in to the real `wpt serve` with
    // `WPT_WPTSERVE=1` for `.py` endpoints, full `.sub` substitution, multi-origin, and `.https`.
    let backend = if std::env::var("WPT_WPTSERVE").is_ok() {
        // Trust the WPT test CA so the engine can load `.https` tests over TLS (see `net::agent`).
        let ca = root.join("tools/certs/cacert.pem");
        if ca.is_file() {
            std::env::set_var("WPT_CA_FILE", &ca);
        }
        start_wptserve(&root)
    } else {
        // Built-in static server on an ephemeral port.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let root = root.clone();
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let root = root.clone();
                std::thread::spawn(move || serve(&mut s, &root, port));
            }
        });
        Backend::Static(port)
    };

    let mut all = Vec::new();
    let target = root.join(subpath);
    if target.is_file() {
        // Allow pointing the runner at a single test file, not just a directory.
        all.push(target);
    } else {
        collect_tests(&target, &mut all);
    }
    all.sort();
    // Build the (short-name, absolute-url) list to run. `.html` tests are kept only if they load
    // testharness.js (skips reftests / visual tests); `.js` tests expand to their generated HTML
    // variants (`wpt serve` mode only — see `test_variants`).
    let wptserve = backend.is_wptserve();
    let mut tests: Vec<(String, String)> = Vec::new();
    for path in &all {
        let rel = path
            .strip_prefix(&*root)
            .unwrap()
            .to_string_lossy()
            .replace(' ', "%20");
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let is_js = name.ends_with(".any.js")
            || name.ends_with(".window.js")
            || name.ends_with(".worker.js");
        if is_js {
            for variant in test_variants(&rel, wptserve) {
                let vname = variant.rsplit('/').next().unwrap_or(&variant).to_string();
                let url = format!("{}/{variant}", backend.origin(&vname));
                tests.push((variant, url));
            }
        } else if std::fs::read_to_string(path)
            .map(|s| s.contains("testharness.js"))
            .unwrap_or(false)
        {
            let url = format!("{}/{rel}", backend.origin(&name));
            tests.push((rel, url));
        }
    }
    let total_found = tests.len();
    tests.truncate(max);
    let backend_name = match backend {
        Backend::Static(port) => format!("static :{port}"),
        Backend::WptServe { .. } => {
            format!("wpt serve {WPT_HOST}:{WPT_HTTP_PORT}/{WPT_HTTPS_PORT}")
        }
    };
    eprintln!(
        "running {} testharness tests from {} ({backend_name})",
        tests.len().min(total_found),
        subpath,
    );

    let (mut files_ok, mut sub_pass, mut sub_fail, mut harness_err, mut timeouts) =
        (0, 0u64, 0u64, 0u64, 0u64);
    // (name, pass, fail, state, detail) for the HTML report. pass/fail = -1 for timeout/harness-err.
    let mut rows: Vec<(String, i64, i64, &'static str, String)> = Vec::new();
    for (short, url) in &tests {
        let short = short.clone();
        let mut e = engine::Engine::new();
        e.set_viewport(800, 600, 1.0);
        e.load_url(url);

        // Tick until the harness reports completion or we time out (~10s wall).
        let start = Instant::now();
        let mut done = false;
        while start.elapsed() < Duration::from_secs(5) {
            for _ in 0..5 {
                e.tick();
            }
            if e.console_eval("window.__wpt_done || 0") == "1" {
                done = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        if !done {
            timeouts += 1;
            println!("TIMEOUT  {short}");
            rows.push((short, -1, -1, "timeout", String::new()));
            continue;
        }
        let harness = e.console_eval("window.__wpt_harness");
        if harness == "1" {
            harness_err += 1;
            let msg = e.console_eval("window.__wpt_harness_msg");
            println!("HARNESS-ERR  {short}  — {msg}");
            rows.push((short, -1, -1, "error", msg));
            continue;
        }
        let p: u64 = e.console_eval("window.__wpt_pass").parse().unwrap_or(0);
        let f: u64 = e.console_eval("window.__wpt_fail").parse().unwrap_or(0);
        sub_pass += p;
        sub_fail += f;
        files_ok += 1;
        let mark = if f == 0 { "PASS" } else { "FAIL" };
        let detail = if f == 0 {
            String::new()
        } else {
            e.console_eval("window.__wpt_firstfail")
        };
        if f == 0 {
            println!("{mark} [{p}/{}]  {short}", p + f);
        } else {
            println!("{mark} [{p}/{}]  {short}  — {detail}", p + f);
            if std::env::var("WPT_ALLFAILS").is_ok() {
                println!("{}", e.console_eval("window.__wpt_allfails"));
            }
        }
        rows.push((
            short,
            p as i64,
            f as i64,
            if f == 0 { "pass" } else { "fail" },
            detail,
        ));
    }

    println!("\n==== WPT summary: {subpath} ====");
    println!(
        "files: {} ran, {} harness-errors, {} timeouts",
        files_ok, harness_err, timeouts
    );
    let total = sub_pass + sub_fail;
    let pct = if total > 0 {
        100.0 * sub_pass as f64 / total as f64
    } else {
        0.0
    };
    println!("subtests: {sub_pass}/{total} passed ({pct:.1}%)");

    // Emit an HTML report (viewable in our own browser). Path via WPT_REPORT or default.
    let report_path = std::env::var("WPT_REPORT").unwrap_or_else(|_| "/tmp/wpt-report.html".into());
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    let mut body = String::new();
    for (name, p, f, state, detail) in &rows {
        let (badge, cells) = match *state {
            "pass" => (
                "pass",
                format!("<td class=num>{p}</td><td class=num>0</td>"),
            ),
            "fail" => (
                "fail",
                format!("<td class=num>{p}</td><td class=num bad>{f}</td>"),
            ),
            "timeout" => ("timeout", "<td class=num>–</td><td class=num>–</td>".into()),
            _ => ("error", "<td class=num>–</td><td class=num>–</td>".into()),
        };
        let det = if detail.is_empty() {
            String::new()
        } else {
            format!("<div class=det>{}</div>", esc(detail))
        };
        body.push_str(&format!(
            "<tr class={badge}><td><span class='b {badge}'>{}</span></td><td class=name>{}{}</td>{cells}</tr>\n",
            badge.to_uppercase(), esc(name), det
        ));
    }
    let bar = pct.round() as u64;
    let html = format!(
        r#"<!doctype html><html><head><meta charset=utf-8><title>WPT — {subpath}</title><style>
:root{{color-scheme:light dark}}
body{{font-family:system-ui,sans-serif;margin:0;background:#fff;color:#1a1a1a}}
header{{padding:28px 32px;border-bottom:1px solid #ddd}}
h1{{margin:0 0 4px;font-size:22px}}
.sub{{color:#666;font-size:14px}}
.score{{font-size:56px;font-weight:700;margin:14px 0 6px}}
.score small{{font-size:22px;color:#666;font-weight:400}}
.track{{height:10px;border-radius:5px;background:#eee;overflow:hidden;max-width:520px}}
.fill{{height:100%;background:linear-gradient(90deg,#d33,#e90,#2a2);width:{bar}%}}
.meta{{margin-top:10px;color:#555;font-size:13px}}
table{{border-collapse:collapse;width:100%;font-size:13px}}
td,th{{padding:7px 12px;border-bottom:1px solid #eee;text-align:left;vertical-align:top}}
th{{position:sticky;top:0;background:#fafafa;font-size:12px;color:#666}}
.num{{text-align:right;font-variant-numeric:tabular-nums;width:60px}}
.num.bad{{color:#d33;font-weight:600}}
.name{{font-family:ui-monospace,monospace;color:#222}}
.det{{color:#b00;font-size:12px;margin-top:3px;font-family:ui-monospace,monospace}}
.b{{display:inline-block;padding:2px 8px;border-radius:4px;font-size:11px;font-weight:700;color:#fff}}
.b.pass{{background:#2a2}} .b.fail{{background:#d33}} .b.timeout{{background:#999}} .b.error{{background:#a0a}}
tr.pass td.name{{color:#444}}
@media (prefers-color-scheme: dark) {{
  body{{background:#15171a;color:#e6e6e6}}
  header{{border-bottom-color:#2a2d31}}
  .sub,.meta,.score small{{color:#9aa0a6}}
  .track{{background:#2a2d31}}
  td,th{{border-bottom-color:#23262a}}
  th{{background:#1b1e22;color:#9aa0a6}}
  .name{{color:#cfd3d7}} tr.pass td.name{{color:#8b9096}}
  .num.bad{{color:#ff6b6b}} .det{{color:#ff8a8a}}
}}
</style></head><body>
<header>
<h1>Web Platform Tests — <code>{subpath}</code></h1>
<div class=sub>run in-process against our own engine via <code>wpt-runner</code></div>
<div class=score>{pct:.1}% <small>{sub_pass} / {total} subtests</small></div>
<div class=track><div class=fill></div></div>
<div class=meta>{files_ok} files ran · {harness_err} harness errors · {timeouts} timeouts · {} testharness files</div>
</header>
<table><thead><tr><th>Status</th><th>Test</th><th class=num>Pass</th><th class=num>Fail</th></tr></thead>
<tbody>
{body}</tbody></table>
</body></html>"#,
        rows.len()
    );
    std::fs::write(&report_path, html).expect("write report");
    println!("HTML report: {report_path}");
}
