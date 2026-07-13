//! End-to-end WebDriver flow against a server on an ephemeral port, driven over a raw TcpStream.
//!
//! Covers: New Session → Navigate (data: page) → Get Title → Find Element (css) → Get Element Text
//! → Execute Sync → Execute Async → Screenshot → Delete Session.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use webdriver::json::{parse, Json};

/// Minimal HTTP client: send one request, read the full response, return (status, body).
fn request(port: u16, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    let body = body.unwrap_or("");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).unwrap();

    let mut resp = Vec::new();
    stream.read_to_end(&mut resp).unwrap();
    let text = String::from_utf8_lossy(&resp).to_string();
    let status: u16 = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = text
        .split_once("\r\n\r\n")
        .map(|x| x.1)
        .unwrap_or("")
        .to_string();
    (status, body)
}

/// Extract `.value` from a `{"value": ...}` response body.
fn value(body: &str) -> Json {
    parse(body)
        .and_then(|v| v.get("value").cloned())
        .unwrap_or(Json::Null)
}

#[test]
fn full_webdriver_flow() {
    // Start the server on an ephemeral port in a background thread.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        webdriver::server::serve(listener).unwrap();
    });
    // Give the listener a moment.
    std::thread::sleep(Duration::from_millis(50));

    // GET /status
    let (st, body) = request(port, "GET", "/status", None);
    assert_eq!(st, 200);
    assert_eq!(value(&body).get("ready"), Some(&Json::Bool(true)));

    // New Session.
    let (st, body) = request(
        port,
        "POST",
        "/session",
        Some(r#"{"capabilities":{"alwaysMatch":{}}}"#),
    );
    assert_eq!(st, 200, "new session body: {body}");
    let v = value(&body);
    let sid = v
        .get("sessionId")
        .and_then(|s| s.as_str())
        .expect("sessionId")
        .to_string();
    assert!(!sid.is_empty());

    // Navigate to a file: page with a known title + an element. (Our `net` layer serves
    // `http(s)://` and `file://`; it has no `data:` handler, so the test uses a temp file.)
    let page = "<html><head><title>Hi There</title></head><body><p id=\"greet\">Hello WD</p></body></html>";
    let path = std::env::temp_dir().join("wd_flow_test.html");
    std::fs::write(&path, page).unwrap();
    let file_url = format!("file://{}", path.display());
    let nav_body = Json::Obj(
        [("url".to_string(), Json::Str(file_url))]
            .into_iter()
            .collect(),
    )
    .to_string();
    let (st, body) = request(
        port,
        "POST",
        &format!("/session/{sid}/url"),
        Some(&nav_body),
    );
    assert_eq!(st, 200, "navigate body: {body}");

    // Get Title.
    let (st, body) = request(port, "GET", &format!("/session/{sid}/title"), None);
    assert_eq!(st, 200);
    assert_eq!(
        value(&body).as_str(),
        Some("Hi There"),
        "title body: {body}"
    );

    // A second navigation creates history; back and forward restore each document.
    let page2 = "<html><head><title>Second Page</title></head><body>Later</body></html>";
    let path2 = std::env::temp_dir().join("wd_flow_test_second.html");
    std::fs::write(&path2, page2).unwrap();
    let file_url2 = format!("file://{}", path2.display());
    let nav_body2 = Json::Obj(
        [("url".to_string(), Json::Str(file_url2))]
            .into_iter()
            .collect(),
    )
    .to_string();
    let (st, body) = request(
        port,
        "POST",
        &format!("/session/{sid}/url"),
        Some(&nav_body2),
    );
    assert_eq!(st, 200, "second navigate body: {body}");
    let (st, body) = request(port, "POST", &format!("/session/{sid}/back"), Some("{}"));
    assert_eq!(st, 200, "back body: {body}");
    let (_, body) = request(port, "GET", &format!("/session/{sid}/title"), None);
    assert_eq!(value(&body).as_str(), Some("Hi There"));
    let (st, body) = request(port, "POST", &format!("/session/{sid}/forward"), Some("{}"));
    assert_eq!(st, 200, "forward body: {body}");
    let (_, body) = request(port, "GET", &format!("/session/{sid}/title"), None);
    assert_eq!(value(&body).as_str(), Some("Second Page"));
    let (st, body) = request(port, "POST", "/session/missing/back", Some("{}"));
    assert_eq!(st, 404, "invalid-session back body: {body}");
    assert_eq!(
        value(&body).get("error").and_then(|e| e.as_str()),
        Some("invalid session id")
    );

    // Return to the first page for the element assertions below.
    let (st, body) = request(port, "POST", &format!("/session/{sid}/back"), Some("{}"));
    assert_eq!(st, 200, "second back body: {body}");

    // Find Element (css).
    let find_body = r##"{"using":"css selector","value":"#greet"}"##;
    let (st, body) = request(
        port,
        "POST",
        &format!("/session/{sid}/element"),
        Some(find_body),
    );
    assert_eq!(st, 200, "find body: {body}");
    let el = value(&body);
    let handle = el
        .get("element-6066-11e4-a52e-4f735466cecf")
        .and_then(|h| h.as_str())
        .expect("element handle")
        .to_string();

    // Get Element Text.
    let (st, body) = request(
        port,
        "GET",
        &format!("/session/{sid}/element/{handle}/text"),
        None,
    );
    assert_eq!(st, 200, "text body: {body}");
    assert_eq!(
        value(&body).as_str(),
        Some("Hello WD"),
        "element text: {body}"
    );

    // Execute Sync: return 1+1 → 2.
    let exec_body = r#"{"script":"return 1+1;","args":[]}"#;
    let (st, body) = request(
        port,
        "POST",
        &format!("/session/{sid}/execute/sync"),
        Some(exec_body),
    );
    assert_eq!(st, 200, "exec sync body: {body}");
    assert_eq!(value(&body).as_f64(), Some(2.0), "exec sync: {body}");

    // Execute Async: callback with a value.
    let async_body = r#"{"script":"var cb = arguments[arguments.length-1]; cb(42);","args":[]}"#;
    let (st, body) = request(
        port,
        "POST",
        &format!("/session/{sid}/execute/async"),
        Some(async_body),
    );
    assert_eq!(st, 200, "exec async body: {body}");
    assert_eq!(value(&body).as_f64(), Some(42.0), "exec async: {body}");

    // Screenshot: non-empty base64.
    let (st, body) = request(port, "GET", &format!("/session/{sid}/screenshot"), None);
    assert_eq!(st, 200, "screenshot status");
    let b64 = value(&body).as_str().unwrap_or("").to_string();
    assert!(
        b64.len() > 100,
        "screenshot base64 too short: {}",
        b64.len()
    );

    // Find a missing element → 404 no such element.
    let (st, body) = request(
        port,
        "POST",
        &format!("/session/{sid}/element"),
        Some(r##"{"using":"css selector","value":"#nope"}"##),
    );
    assert_eq!(st, 404, "missing element should 404: {body}");
    assert_eq!(
        value(&body).get("error").and_then(|e| e.as_str()),
        Some("no such element")
    );

    // Delete Session.
    let (st, _) = request(port, "DELETE", &format!("/session/{sid}"), None);
    assert_eq!(st, 200);

    // Commands on a deleted session → 404 invalid session id.
    let (st, body) = request(port, "GET", &format!("/session/{sid}/title"), None);
    assert_eq!(st, 404, "deleted session should 404: {body}");
    assert_eq!(
        value(&body).get("error").and_then(|e| e.as_str()),
        Some("invalid session id")
    );
}
