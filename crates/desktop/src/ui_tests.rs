//! Exercise Rune's real layout, focus and input dispatch without a native window.
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rune::{Env, Key, KeyEvent, Mods, Runtime};

use super::{app, Browser, Command};

fn runtime(scale: f32) -> Runtime<Browser> {
    let mut runtime = app()
        .init(|| Browser::new("about:blank".into(), true))
        .headless_runtime((1000.0, 700.0), Env::new(false, scale));
    settle(&mut runtime);
    runtime
}

fn settle(runtime: &mut Runtime<Browser>) {
    // A layout can enqueue a viewport update after poll; the following frame
    // synchronizes that update before consuming its snapshot. No timing sleeps.
    for _ in 0..3 {
        runtime.settle_all();
        runtime.frame(1.0 / 60.0);
    }
    assert!(runtime.state.status.is_none(), "{:?}", runtime.state.status);
}

fn click_at(runtime: &mut Runtime<Browser>, x: f32, y: f32) {
    runtime.pointer_move(x, y);
    runtime.pointer_down(x, y);
    runtime.frame(1.0 / 60.0);
    runtime.pointer_up(x, y);
    settle(runtime);
}

fn click(runtime: &mut Runtime<Browser>, key: &str) {
    let (x, y) = runtime.find(key).unwrap_or_else(|| panic!("missing {key}"));
    click_at(runtime, x, y);
}

fn key(runtime: &mut Runtime<Browser>, key: Key) {
    runtime.key(KeyEvent {
        key,
        mods: Mods::default(),
    });
    settle(runtime);
}

struct Page {
    path: PathBuf,
    url: String,
}

impl Page {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "browser-ui-{}-{}.html",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(
            &path,
            r#"<!doctype html><title>Portable input fixture</title><body>
            <button id="button" style="position:absolute;left:10px;top:10px;width:100px;height:40px">before</button>
            <input id="entry" style="position:absolute;left:10px;top:70px;width:180px;height:30px">
            <script>
                var clicks = 0;
                document.getElementById('button').onclick = function () {
                    clicks++;
                    this.textContent = 'clicked';
                };
            </script>"#,
        )
        .unwrap();
        let absolute = path
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let absolute = absolute.strip_prefix("//?/").unwrap_or(&absolute);
        let url = format!("file:///{}", absolute.trim_start_matches('/'));
        Self { path, url }
    }
}

impl Drop for Page {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn navigate(runtime: &mut Runtime<Browser>, page: &Page) {
    // Command-L exercises the app shortcut and selects the existing URL;
    // paste and Enter then travel through Rune's text editor and submit hook.
    runtime.key(KeyEvent {
        key: Key::Command('l'),
        mods: Mods {
            cmd: true,
            ..Mods::default()
        },
    });
    settle(runtime);
    runtime.paste(page.url.clone());
    settle(runtime);
    assert_eq!(runtime.state.address.text, page.url);
    key(runtime, Key::Enter);
    let snapshot = &runtime.state.tab().unwrap().snapshot;
    assert_eq!(snapshot.url, page.url);
    assert_eq!(snapshot.title, "Portable input fixture");
    assert!(!snapshot.loading);
    assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
}

fn evaluate(runtime: &mut Runtime<Browser>, expression: &str) -> String {
    // Inspection only: page interaction itself uses Rune's input callbacks.
    runtime.state.console.clear();
    runtime.state.send(Command::Eval(expression.into()));
    settle(runtime);
    runtime.state.console.last().unwrap().clone()
}

#[test]
fn tab_buttons_activate_close_and_keep_one_tab_open() {
    let mut runtime = runtime(1.0);
    let first = runtime.state.tabs[0].id;
    click(&mut runtime, "new-tab");
    assert_eq!(runtime.state.tabs.len(), 2);
    assert_eq!(runtime.state.active, 1);
    let second = runtime.state.tabs[1].id;

    click(&mut runtime, &format!("activate-{first}"));
    assert_eq!(runtime.state.tab().unwrap().id, first);
    click(&mut runtime, &format!("close-{second}"));
    assert_eq!(runtime.state.tabs.len(), 1);
    assert_eq!(runtime.state.tab().unwrap().id, first);

    click(&mut runtime, &format!("close-{first}"));
    assert_eq!(runtime.state.tabs.len(), 1);
    assert_ne!(runtime.state.tab().unwrap().id, first);
    assert_eq!(runtime.state.tab().unwrap().snapshot.url, "about:blank");
}

#[test]
fn address_submission_loads_a_page_and_updates_navigation_controls() {
    let page = Page::new();
    let mut runtime = runtime(1.0);
    navigate(&mut runtime, &page);
    assert!(runtime.state.tab().unwrap().snapshot.frame.is_some());
    assert!(runtime.state.tab().unwrap().snapshot.can_back);

    click(&mut runtime, "back");
    assert_eq!(runtime.state.tab().unwrap().snapshot.url, "about:blank");
    assert!(runtime.state.tab().unwrap().snapshot.can_forward);
    click(&mut runtime, "forward");
    assert_eq!(runtime.state.tab().unwrap().snapshot.url, page.url);
}

#[test]
fn page_pointer_and_keyboard_callbacks_use_hidpi_coordinates() {
    let page = Page::new();
    let mut runtime = runtime(2.0);
    navigate(&mut runtime, &page);
    let frame = runtime
        .state
        .tab()
        .unwrap()
        .snapshot
        .frame
        .as_ref()
        .unwrap();
    assert_eq!(frame.width, runtime.state.viewport.0 * 2);
    assert_eq!(frame.height, runtime.state.viewport.1 * 2);
    let (rect, _) = runtime.presented("page").unwrap();

    // Runtime input is logical points. The page worker receives device pixels.
    // These positions also catch forgetting to subtract the toolbar's origin.
    click_at(&mut runtime, rect[0] + 30.0, rect[1] + 25.0);
    assert_eq!(evaluate(&mut runtime, "clicks"), "1");
    assert_eq!(
        evaluate(
            &mut runtime,
            "document.getElementById('button').textContent"
        ),
        "clicked"
    );

    click_at(&mut runtime, rect[0] + 30.0, rect[1] + 85.0);
    assert!(runtime.state.tab().unwrap().snapshot.text_focus);
    key(&mut runtime, Key::Text("portable".into()));
    key(&mut runtime, Key::Backspace);
    runtime.paste("!".to_owned());
    settle(&mut runtime);
    assert_eq!(
        evaluate(&mut runtime, "document.getElementById('entry').value"),
        "portabl!"
    );
    key(&mut runtime, Key::Tab);
    assert!(
        !runtime
            .elements()
            .iter()
            .find(|element| element.key.as_deref() == Some("page"))
            .unwrap()
            .focused
    );
}
