//! A new session's window size must reach page JS as `window.innerWidth`/`innerHeight`.
//!
//! Lives in its own integration binary because the engine publishes those metrics through
//! process-global atomics: sharing a process with another test that sets a different viewport would
//! make the assertions race.

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

/// `innerWidth x innerHeight` as page JS sees it, after navigating to a fresh document.
fn seeded_inner_size(port: u16, sid: &str, page: &str) -> String {
    let path = std::env::temp_dir().join(page);
    std::fs::write(&path, "<html><body>x</body></html>").unwrap();
    let nav = Json::Obj(
        [(
            "url".to_string(),
            Json::Str(format!("file://{}", path.display())),
        )]
        .into_iter()
        .collect(),
    )
    .to_string();
    let (st, body) = request(port, "POST", &format!("/session/{sid}/url"), Some(&nav));
    assert_eq!(st, 200, "navigate body: {body}");

    let script = Json::Obj(
        [
            (
                "script".to_string(),
                Json::Str("return innerWidth + 'x' + innerHeight".to_string()),
            ),
            ("args".to_string(), Json::Arr(vec![])),
        ]
        .into_iter()
        .collect(),
    )
    .to_string();
    let (st, body) = request(
        port,
        "POST",
        &format!("/session/{sid}/execute/sync"),
        Some(&script),
    );
    assert_eq!(st, 200, "execute body: {body}");
    let _ = std::fs::remove_file(&path);
    value(&body).as_str().unwrap_or_default().to_string()
}

#[test]
fn session_window_size_is_seeded_into_page_js() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        webdriver::server::serve(listener).unwrap();
    });
    std::thread::sleep(Duration::from_millis(50));

    let (st, body) = request(
        port,
        "POST",
        "/session",
        Some(r#"{"capabilities":{"alwaysMatch":{}}}"#),
    );
    assert_eq!(st, 200, "new session body: {body}");
    let sid = value(&body)
        .get("sessionId")
        .and_then(|s| s.as_str())
        .expect("sessionId")
        .to_string();

    // A session with no requested size is 800×600, which is also a fresh engine's own default — so
    // this is the case where `set_viewport` is handed the size it already holds. The metrics page JS
    // reads are process-global and start at their own unrelated default, so skipping the publish on
    // an unchanged viewport leaves every document reporting that default instead of the real window.
    assert_eq!(
        seeded_inner_size(port, &sid, "wd_viewport_default.html"),
        "800x600",
        "a default-sized session must still seed its own size, not the built-in metrics default",
    );

    // And the changed path still has to publish, so the fix can't be "always skip the guard".
    let rect = Json::Obj(
        [
            ("width".to_string(), Json::Num(500.0)),
            ("height".to_string(), Json::Num(400.0)),
        ]
        .into_iter()
        .collect(),
    )
    .to_string();
    let (st, body) = request(
        port,
        "POST",
        &format!("/session/{sid}/window/rect"),
        Some(&rect),
    );
    assert_eq!(st, 200, "set window rect body: {body}");
    assert_eq!(
        seeded_inner_size(port, &sid, "wd_viewport_resized.html"),
        "500x400",
        "a resized window must seed the new size into the next document",
    );
}
