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

#[test]
fn send_keys_uses_native_input_and_respects_keydown_cancellation() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || webdriver::server::serve(listener).unwrap());
    let (_, body) = request(port, "POST", "/session", Some("{}"));
    let sid = value(&body)
        .get("sessionId")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    let path = std::env::temp_dir().join(format!("wd_keys_{}.html", std::process::id()));
    std::fs::write(
        &path,
        r#"<html><body><form id="form"><input id="field"></form>
<input id="check" type="checkbox" checked style="position:absolute;left:10px;top:80px;width:20px;height:20px">
<input id="radio1" type="radio" name="group" checked style="position:absolute;left:50px;top:80px;width:20px;height:20px">
<input id="radio2" type="radio" name="group" style="position:absolute;left:90px;top:80px;width:20px;height:20px">
<script>
window.events = []; window.writes = 0; window.submits = 0; window.cancelEnter = true;
window.checkedWrites = 0; window.checkChanges = 0; window.cancelChecked = false;
['check', 'radio1', 'radio2'].forEach(function (id) {
  var control = document.getElementById(id);
  var checkedDescriptor = Object.getOwnPropertyDescriptor(control, 'checked');
  Object.defineProperty(control, 'checked', {
    configurable: true,
    get: function () { return checkedDescriptor.get.call(this); },
    set: function (value) { checkedWrites++; checkedDescriptor.set.call(this, value); }
  });
  control.addEventListener('click', function (e) { if (cancelChecked) e.preventDefault(); });
  control.addEventListener('change', function () { checkChanges++; });
});
var field = document.getElementById('field');
var descriptor = Object.getOwnPropertyDescriptor(field, 'value');
Object.defineProperty(field, 'value', {
  configurable: true,
  get: function () { return descriptor.get.call(this); },
  set: function (value) { window.writes++; descriptor.set.call(this, value); }
});
field.addEventListener('keydown', function (e) {
  events.push('down:' + e.key);
  if (e.key === 'x' || (e.key === 'Enter' && cancelEnter)) e.preventDefault();
});
field.addEventListener('input', function () { events.push('input:' + field.value); });
field.addEventListener('keyup', function (e) { events.push('up:' + e.key); });
field.addEventListener('change', function () { events.push('change'); });
field.addEventListener('click', function () { events.push('click'); });
document.addEventListener('focusin', function (e) { events.push('focusin:' + e.target.id); });
document.getElementById('form').addEventListener('submit', function (e) {
  e.preventDefault(); window.submits++;
});
</script></body></html>"#,
    )
    .unwrap();
    let nav = Json::Obj(
        [(
            "url".into(),
            Json::Str(format!("file://{}", path.display())),
        )]
        .into_iter()
        .collect(),
    )
    .to_string();
    let (st, body) = request(port, "POST", &format!("/session/{sid}/url"), Some(&nav));
    assert_eq!(st, 200, "{body}");
    let (_, body) = request(
        port,
        "POST",
        &format!("/session/{sid}/element"),
        Some(r##"{"using":"css selector","value":"#field"}"##),
    );
    let handle = value(&body)
        .get("element-6066-11e4-a52e-4f735466cecf")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    let send = |text: &str| {
        let body = Json::Obj(
            [("text".into(), Json::Str(text.into()))]
                .into_iter()
                .collect(),
        )
        .to_string();
        let (status, result) = request(
            port,
            "POST",
            &format!("/session/{sid}/element/{handle}/value"),
            Some(&body),
        );
        assert_eq!(status, 200, "{result}");
    };
    let eval = |script: &str| {
        let body = Json::Obj(
            [
                ("script".into(), Json::Str(script.into())),
                ("args".into(), Json::Arr(vec![])),
            ]
            .into_iter()
            .collect(),
        )
        .to_string();
        let (status, result) = request(
            port,
            "POST",
            &format!("/session/{sid}/execute/sync"),
            Some(&body),
        );
        assert_eq!(status, 200, "{result}");
        value(&result)
    };
    send("axé\u{e003}\u{e007}");
    assert_eq!(
        eval("return 'oninput' in document && 'oninput' in field"),
        Json::Bool(true)
    );
    assert_eq!(
        eval("return document.activeElement === field"),
        Json::Bool(true)
    );
    assert_eq!(eval("return field.value"), Json::Str("a".into()));
    assert_eq!(
        eval("return writes"),
        Json::Num(0.0),
        "native edits bypass JS setters"
    );
    assert_eq!(
        eval("return submits"),
        Json::Num(0.0),
        "prevented Enter cannot submit"
    );
    assert_eq!(eval("return events.join('|')"), Json::Str("focusin:field|down:a|input:a|up:a|down:x|up:x|down:é|input:aé|up:é|down:Backspace|input:a|up:Backspace|down:Enter|up:Enter".into()));
    eval("cancelEnter = false; return null");
    send("\u{e007}");
    assert_eq!(
        eval("return submits"),
        Json::Num(1.0),
        "unprevented Enter submits exactly once"
    );
    let click = |id: &str| {
        let query = Json::Obj(
            [
                ("using".into(), Json::Str("id".into())),
                ("value".into(), Json::Str(id.into())),
            ]
            .into_iter()
            .collect(),
        )
        .to_string();
        let (_, response) = request(
            port,
            "POST",
            &format!("/session/{sid}/element"),
            Some(&query),
        );
        let element = value(&response);
        let handle = element
            .get("element-6066-11e4-a52e-4f735466cecf")
            .unwrap()
            .as_str()
            .unwrap();
        let (status, response) = request(
            port,
            "POST",
            &format!("/session/{sid}/element/{handle}/click"),
            Some("{}"),
        );
        assert_eq!(status, 200, "{response}");
    };
    click("check");
    assert_eq!(
        eval("return document.getElementById('check').checked"),
        Json::Bool(false)
    );
    eval("cancelChecked = true; return null");
    click("check");
    assert_eq!(
        eval("return document.getElementById('check').checked"),
        Json::Bool(false),
        "cancelled checkbox restores backing state"
    );
    eval("cancelChecked = false; return null");
    click("radio2");
    assert_eq!(eval("return document.getElementById('radio1').checked + ',' + document.getElementById('radio2').checked"), Json::Str("false,true".into()));
    eval("cancelChecked = true; return null");
    click("radio1");
    assert_eq!(eval("return document.getElementById('radio1').checked + ',' + document.getElementById('radio2').checked"), Json::Str("false,true".into()), "cancelled radio restores the whole group");
    assert_eq!(
        eval("return checkedWrites"),
        Json::Num(0.0),
        "native activation and rollback bypass own JS setters"
    );
    assert_eq!(
        eval("return checkChanges"),
        Json::Num(2.0),
        "cancelled activations do not emit change"
    );
    request(port, "DELETE", &format!("/session/{sid}"), None);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn click_navigation_and_get_form_submission_preserve_history() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || webdriver::server::serve(listener).unwrap());
    let (_, body) = request(port, "POST", "/session", Some("{}"));
    let sid = value(&body)
        .get("sessionId")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    let source = std::env::temp_dir().join(format!("wd_nav_source_{}.html", std::process::id()));
    let pages = TcpListener::bind("127.0.0.1:0").unwrap();
    let target = format!(
        "http://127.0.0.1:{}/destination",
        pages.local_addr().unwrap().port()
    );
    std::thread::spawn(move || {
        for mut stream in pages.incoming().flatten() {
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut headers = Vec::new();
            let mut byte = [0];
            while !headers.ends_with(b"\r\n\r\n") {
                if stream.read(&mut byte).unwrap_or(0) == 0 {
                    break;
                }
                headers.push(byte[0]);
            }
            let page = "<html><title>Destination</title><body>Arrived</body></html>";
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}", page.len());
            let _ = stream.write_all(response.as_bytes());
        }
    });
    std::fs::write(&source, format!(r##"<html><title>Source</title><body>
<a id="cancel" href="{target}" style="display:block;width:200px;height:30px">Cancelled link</a>
<a id="link" href="{target}" style="display:block;width:200px;height:30px">Destination link</a>
<a id="filter" href="#active" style="display:block;width:200px;height:30px">Filter</a>
<form id="search" action="{target}" method="get">
<input name="q" value="web browser & é">
<input name="omit" value="ignored" disabled>
<input type="checkbox" name="checked" checked>
<input type="checkbox" name="unchecked">
<select name="language"><option value="fr">French</option><option value="en" selected>English</option></select>
<button id="submit" name="go" value="yes" style="display:block;width:200px;height:30px">Search</button>
</form><script>
window.navigationEvents = [];
window.addEventListener('popstate', function() {{ navigationEvents.push('popstate'); }});
window.addEventListener('hashchange', function() {{ navigationEvents.push('hashchange'); }});
document.getElementById('cancel').addEventListener('click', function(e) {{ e.preventDefault(); }});
</script></body></html>"##)).unwrap();
    let command = |method: &str, path: &str, body: Option<&str>| {
        let (st, response) = request(port, method, &format!("/session/{sid}{path}"), body);
        assert_eq!(st, 200, "{path}: {response}");
        value(&response)
    };
    let source_url = if cfg!(windows) {
        format!(
            "file:///{}",
            source.display().to_string().replace('\\', "/")
        )
    } else {
        format!("file://{}", source.display())
    };
    let nav = Json::Obj(
        [("url".into(), Json::Str(source_url.clone()))]
            .into_iter()
            .collect(),
    )
    .to_string();
    command("POST", "/url", Some(&nav));
    let click = |selector: &str| {
        let query = Json::Obj(
            [
                ("using".into(), Json::Str("css selector".into())),
                ("value".into(), Json::Str(selector.into())),
            ]
            .into_iter()
            .collect(),
        )
        .to_string();
        let element = command("POST", "/element", Some(&query));
        let handle = element
            .get("element-6066-11e4-a52e-4f735466cecf")
            .unwrap()
            .as_str()
            .unwrap();
        command("POST", &format!("/element/{handle}/click"), Some("{}"));
    };
    click("#cancel");
    assert_eq!(command("GET", "/title", None), Json::Str("Source".into()));
    click("#filter");
    assert_eq!(
        command(
            "POST",
            "/execute/sync",
            Some(r#"{"script":"return navigationEvents.join(',')","args":[]}"#)
        ),
        Json::Str("popstate,hashchange".into()),
        "fragment navigation preserves the document and notifies hash routers"
    );
    command("POST", "/back", Some("{}"));
    assert_eq!(
        command(
            "POST",
            "/execute/sync",
            Some(r#"{"script":"return navigationEvents.join(',')","args":[]}"#)
        ),
        Json::Str("popstate,hashchange,popstate,hashchange".into()),
        "Back preserves the same realm and dispatches history events"
    );
    assert_eq!(command("GET", "/url", None), Json::Str(source_url.clone()));
    command("POST", "/forward", Some("{}"));
    assert_eq!(
        command(
            "POST",
            "/execute/sync",
            Some(r#"{"script":"return navigationEvents.length","args":[]}"#)
        ),
        Json::Num(6.0),
        "Forward preserves application state and dispatches both events"
    );
    assert_eq!(
        command("GET", "/url", None),
        Json::Str(format!("{source_url}#active"))
    );
    command("POST", "/back", Some("{}"));
    click("#link");
    assert_eq!(
        command("GET", "/title", None),
        Json::Str("Destination".into())
    );
    command("POST", "/back", Some("{}"));
    assert_eq!(command("GET", "/title", None), Json::Str("Source".into()));
    click("#submit");
    assert_eq!(
        command("GET", "/title", None),
        Json::Str("Destination".into())
    );
    let url = command("GET", "/url", None);
    let url = url.as_str().unwrap();
    assert!(
        url.ends_with("?q=web+browser+%26+%C3%A9&checked=on&language=en&go=yes"),
        "{url}"
    );
    command("POST", "/back", Some("{}"));
    assert_eq!(command("GET", "/url", None), Json::Str(source_url));
    request(port, "DELETE", &format!("/session/{sid}"), None);
    std::fs::remove_file(source).unwrap();
}
