//! Rune desktop chrome. Page pixels and input cross only the toolkit-independent worker boundary.
use std::cell::Cell;
use std::time::Duration;

use rune::tok::{self, type_};
use rune::{
    col, el, row, text, text_input, AppSpec, Cx, El, Key, KeyEvent, Phase, Pixels, TextEdit, Tx,
    Waker,
};

use crate::worker::{Command, Event, Snapshot, Worker};

struct Tab {
    id: u64,
    worker: Option<Worker>,
    snapshot: Snapshot,
    pixels: Pixels,
    last_frame: u64,
}

pub struct Browser {
    tabs: Vec<Tab>,
    active: usize,
    next_id: u64,
    address: TextEdit,
    console_input: TextEdit,
    console: Vec<String>,
    console_open: bool,
    select: Option<engine::SelectHit>,
    waker: Option<Waker>,
    viewport: (u32, u32),
    environment: Cell<(f32, bool)>,
    applied_environment: (f32, bool),
    wait_for_page: bool,
    initial_url: String,
    status: Option<String>,
}

impl Browser {
    pub fn new(url: String, wait_for_page: bool) -> Self {
        Self {
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
            address: TextEdit::new(&url),
            console_input: TextEdit::default(),
            console: Vec::new(),
            console_open: false,
            select: None,
            waker: None,
            viewport: (1100, 680),
            environment: Cell::new((1.0, false)),
            applied_environment: (1.0, false),
            wait_for_page,
            initial_url: url,
            status: None,
        }
    }

    fn tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }
    fn send(&mut self, command: Command) {
        if let Some(worker) = self.tab().and_then(|tab| tab.worker.as_ref()) {
            if !worker.send(command) {
                self.status = Some("The page is busy. Try again, or open a new tab.".into());
            }
        }
    }
    fn add_tab(&mut self, url: String) {
        let worker = self
            .waker
            .clone()
            .and_then(|wake| match Worker::spawn(wake) {
                Ok(worker) => Some(worker),
                Err(error) => {
                    self.status = Some(format!("Could not open tab: {error}"));
                    None
                }
            });
        let id = self.next_id;
        self.next_id += 1;
        self.tabs.push(Tab {
            id,
            worker,
            snapshot: Snapshot::default(),
            pixels: Pixels::default(),
            last_frame: 0,
        });
        self.activate(self.tabs.len() - 1);
        self.configure_active();
        self.send(Command::Navigate(url.clone()));
        self.address.replace_text(url);
    }
    fn activate(&mut self, index: usize) {
        self.active = index;
        self.select = None;
        for (i, tab) in self.tabs.iter().enumerate() {
            if let Some(worker) = &tab.worker {
                worker.send(Command::Active(i == index));
            }
        }
        if let Some(tab) = self.tab() {
            self.address.replace_text(tab.snapshot.url.clone());
        }
        self.configure_active();
    }
    fn close_tab(&mut self, id: u64) {
        if let Some(index) = self.tabs.iter().position(|tab| tab.id == id) {
            self.tabs.remove(index);
            if self.tabs.is_empty() {
                self.add_tab("about:blank".into());
            } else {
                let active = if index < self.active {
                    self.active - 1
                } else {
                    self.active.min(self.tabs.len() - 1)
                };
                self.activate(active);
            }
        }
    }
    fn configure_active(&mut self) {
        let (scale, dark) = self.environment.get();
        self.send(Command::Viewport {
            width: self.viewport.0,
            height: self.viewport.1,
            scale,
        });
        self.send(Command::Appearance(dark));
        self.applied_environment = (scale, dark);
    }
    fn navigate(&mut self, tx: &mut Tx) {
        let url = self.address.text.trim().to_string();
        if !url.is_empty() {
            self.select = None;
            self.status = None;
            self.send(Command::Navigate(url));
            tx.focus("page", false);
        }
    }
    fn focus_address(&mut self, tx: &mut Tx) {
        self.address.anchor = 0;
        self.address.focus = self.address.text.len();
        tx.focus("address", true);
    }
}

fn init() -> Browser {
    let args = rune::extra_args();
    let url = args
        .windows(2)
        .find(|a| a[0] == "--url")
        .map(|a| a[1].clone())
        .unwrap_or_else(|| "about:blank".into());
    Browser::new(url, args.iter().any(|a| a == "--wait-for-page"))
}

fn setup(state: &mut Browser, waker: Waker) {
    state.waker = Some(waker);
    state.add_tab(state.initial_url.clone());
    if state.wait_for_page {
        if let Some(worker) = state.tab().and_then(|tab| tab.worker.as_ref()) {
            if !worker.synchronize(Duration::from_secs(30)) {
                state.status = Some("Timed out waiting for the page.".into());
            }
        }
    }
}

fn poll(state: &mut Browser, _time: f64, tx: &mut Tx) -> bool {
    let mut changed = false;
    if state.environment.get() != state.applied_environment {
        state.configure_active();
    }
    if state.wait_for_page {
        if let Some(worker) = state.tab().and_then(|tab| tab.worker.as_ref()) {
            if !worker.synchronize(Duration::from_secs(30)) {
                state.status = Some("Timed out waiting for the page.".into());
            }
        }
    }
    for (index, tab) in state.tabs.iter_mut().enumerate() {
        let Some(worker) = &tab.worker else { continue };
        if let Some(snapshot) = worker.take_snapshot() {
            if let Some(frame) = &snapshot.frame {
                if frame.sequence != tab.last_frame {
                    tab.pixels
                        .set(frame.width, frame.height, frame.rgba.clone());
                    tab.last_frame = frame.sequence;
                }
            }
            if index == state.active {
                if snapshot.url != tab.snapshot.url && !state.address.focused {
                    state.address.replace_text(&snapshot.url);
                }
                tx.set_title(if snapshot.title.is_empty() {
                    "Browser".into()
                } else {
                    format!("{} — Browser", snapshot.title)
                });
            }
            tab.snapshot = snapshot;
            changed = true;
        }
        for event in worker.drain_events() {
            match event {
                Event::Clipboard(value) => tx.copy(value),
                Event::Select(hit) if index == state.active => state.select = Some(hit),
                Event::Console(line) => {
                    state.console.push(line);
                    if state.console.len() > 200 {
                        state.console.remove(0);
                    }
                }
                _ => {}
            }
            changed = true;
        }
    }
    changed
}

fn button(
    key: impl Into<String>,
    label: &str,
    enabled: bool,
    dark: bool,
    action: impl Fn(&mut Browser, &mut Tx) + 'static,
) -> El {
    let theme = tok::theme(dark);
    el().key(key)
        .role("button")
        .label(label)
        .focusable(true)
        .disabled(!enabled)
        .h(30.0)
        .min_w(32.0)
        .px(9.0)
        .center()
        .radius(6.0)
        .fill(theme.card)
        .hover(theme.control_off)
        .child(text(label).font(type_::LABEL).color(if enabled {
            theme.text
        } else {
            theme.text_secondary
        }))
        .on_press_tx(action)
}

fn view(state: &Browser, cx: &mut Cx<Browser>) -> El {
    let env = cx.env();
    state.environment.set((env.scale, env.system_dark));
    let dark = env.system_dark;
    let theme = tok::theme(dark);
    let snapshot = state.tab().map(|tab| &tab.snapshot);
    let mut tabs = row()
        .key("tabs")
        .w_full()
        .h(38.0)
        .gap(4.0)
        .px(8.0)
        .center_y()
        .fill(theme.bg);
    for (index, tab) in state.tabs.iter().enumerate() {
        let id = tab.id;
        let title = if tab.snapshot.title.is_empty() {
            "New tab"
        } else {
            &tab.snapshot.title
        };
        tabs = tabs.child(
            row()
                .key(format!("tab-{id}"))
                .h(30.0)
                .w(180.0)
                .min_w(70.0)
                .radius(6.0)
                .gap(4.0)
                .px(6.0)
                .center_y()
                .fill(if index == state.active {
                    theme.card
                } else {
                    theme.bg
                })
                .child(
                    text(title)
                        .key(format!("activate-{id}"))
                        .font(type_::LABEL)
                        .color(theme.text)
                        .grow(1.0)
                        .min_w(0.0)
                        .truncate()
                        .role("tab")
                        .label(title)
                        .focusable(true)
                        .on_press(move |s: &mut Browser| {
                            if let Some(i) = s.tabs.iter().position(|t| t.id == id) {
                                s.activate(i);
                            }
                        }),
                )
                .child(button(
                    format!("close-{id}"),
                    "×",
                    true,
                    dark,
                    move |s, _| s.close_tab(id),
                )),
        );
    }
    tabs = tabs.child(button("new-tab", "+", true, dark, |s, tx| {
        s.add_tab("about:blank".into());
        s.focus_address(tx);
    }));
    let address = text_input(&state.address, type_::BODY)
        .key("address")
        .role("textbox")
        .label("Address")
        .single_line()
        .placeholder("Enter a URL", theme.text_secondary)
        .h(34.0)
        .grow(1.0)
        .min_w(80.0)
        .px(10.0)
        .radius(6.0)
        .fill(theme.bg)
        .color(theme.text)
        .on_edit(|s: &mut Browser| &mut s.address)
        .on_submit(|s: &mut Browser, tx| s.navigate(tx));
    let toolbar = row()
        .key("navigation")
        .w_full()
        .h(48.0)
        .px(8.0)
        .gap(6.0)
        .center_y()
        .fill(theme.card)
        .child(button(
            "back",
            "Back",
            snapshot.is_some_and(|s| s.can_back),
            dark,
            |s, _| s.send(Command::Back),
        ))
        .child(button(
            "forward",
            "Forward",
            snapshot.is_some_and(|s| s.can_forward),
            dark,
            |s, _| s.send(Command::Forward),
        ))
        .child(button("reload", "Reload", true, dark, |s, _| {
            s.send(Command::Reload)
        }))
        .child(address)
        .child(button("go", "Go", true, dark, |s, tx| s.navigate(tx)))
        .child(button("console-toggle", "Console", true, dark, |s, _| {
            s.console_open = !s.console_open
        }));
    let mut page = el()
        .key("page")
        .role("document")
        .label("Web page")
        .focusable(true)
        .w_full()
        .grow(1.0)
        .min_h(1.0)
        .fill(rune::Color::WHITE)
        .clip_rounded(0.0)
        .ime_cursor()
        .on_layout_tx(move |s: &mut Browser, rect, tx| {
            let size = (
                rect[2].round().max(1.0) as u32,
                rect[3].round().max(1.0) as u32,
            );
            if size != s.viewport || s.environment.get() != s.applied_environment {
                s.viewport = size;
                s.configure_active();
            }
            tx.live_only();
        })
        .on_drag(|s: &mut Browser, drag, tx| {
            let scale = s.environment.get().0;
            let (x, y) = (drag.x * scale, drag.y * scale);
            match drag.phase {
                Phase::Down => {
                    tx.focus("page", false);
                    s.select = None;
                    s.send(Command::ClearSelection);
                    s.send(Command::Mouse {
                        kind: "mousedown".into(),
                        x,
                        y,
                    });
                    s.send(Command::SelectionStart { x, y });
                }
                Phase::Move => {
                    s.send(Command::SelectionExtend { x, y });
                }
                Phase::Up => {
                    s.send(Command::Mouse {
                        kind: "mouseup".into(),
                        x,
                        y,
                    });
                    if !drag.cancelled() && drag.dx.hypot(drag.dy) < 4.0 {
                        s.send(Command::Click { x, y });
                        if drag.clicks == 2 {
                            s.send(Command::Mouse {
                                kind: "dblclick".into(),
                                x,
                                y,
                            });
                        }
                    }
                }
            }
            tx.live_only();
        })
        .on_pointer_move_local(|s: &mut Browser, event, tx| {
            let scale = s.environment.get().0;
            s.send(Command::Move {
                x: event.x * scale,
                y: event.y * scale,
            });
            tx.live_only();
        })
        .on_secondary_press_local(|s: &mut Browser, event, _| {
            let scale = s.environment.get().0;
            s.send(Command::Mouse {
                kind: "contextmenu".into(),
                x: event.x * scale,
                y: event.y * scale,
            });
        })
        .on_wheel(|s: &mut Browser, event, tx| {
            s.send(Command::Scroll(-event.dy * s.environment.get().0));
            tx.live_only();
            true
        })
        .on_key(page_key);
    if let Some(tab) = state.tab() {
        page = page.pixels(&tab.pixels);
    }
    let status = state
        .status
        .as_deref()
        .or_else(|| snapshot.and_then(|s| s.error.as_deref()))
        .or_else(|| snapshot.and_then(|s| s.hover_link.as_deref()))
        .unwrap_or(if snapshot.is_some_and(|s| s.loading) {
            "Loading…"
        } else {
            "Ready"
        });
    let mut root = col()
        .key("browser")
        .w_full()
        .h_full()
        .min_h(0.0)
        // Rune routes non-editor paste to the application root, rather than
        // bubbling it through the focused element's ancestors.
        .on_paste(|s: &mut Browser, value, tx| {
            if tx.focused() == Some("page") {
                s.send(Command::Text(value.into()));
            }
        })
        .fill(theme.bg)
        .child(tabs)
        .child(toolbar)
        .child(page);
    if let Some(select) = &state.select {
        let node_id = select.node_id;
        let mut options = row()
            .key("select-options")
            .w_full()
            .gap(4.0)
            .padding(6.0)
            .fill(theme.card);
        for (index, label) in select.options.iter().enumerate() {
            options = options.child(button(
                format!("option-{index}"),
                label,
                true,
                dark,
                move |s, tx| {
                    s.send(Command::Select { node_id, index });
                    s.select = None;
                    tx.focus("page", false);
                },
            ));
        }
        root = root.child(options);
    }
    if state.console_open {
        let output = state
            .console
            .iter()
            .rev()
            .take(6)
            .rev()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        root = root.child(
            col()
                .key("console")
                .w_full()
                .h(160.0)
                .padding(8.0)
                .gap(6.0)
                .fill(theme.card)
                .child(text(output).font(type_::LABEL).color(theme.text).grow(1.0))
                .child(
                    text_input(&state.console_input, type_::BODY)
                        .key("console-input")
                        .single_line()
                        .label("JavaScript console")
                        .h(30.0)
                        .w_full()
                        .fill(theme.bg)
                        .color(theme.text)
                        .on_edit(|s: &mut Browser| &mut s.console_input)
                        .on_submit(|s: &mut Browser, _| {
                            let code = s.console_input.text.clone();
                            s.send(Command::Eval(code));
                            s.console_input.replace_text("");
                        }),
                ),
        );
    }
    root.child(
        text(status)
            .key("status")
            .font(type_::LABEL)
            .color(theme.text_secondary)
            .w_full()
            .h(24.0)
            .px(8.0),
    )
}

fn shortcuts(state: &mut Browser, event: &KeyEvent, tx: &mut Tx) -> bool {
    if event.is_command('l') {
        state.focus_address(tx);
        return true;
    }
    if event.is_command('t') {
        state.add_tab("about:blank".into());
        state.focus_address(tx);
        return true;
    }
    if event.is_command('w') {
        if let Some(id) = state.tab().map(|t| t.id) {
            state.close_tab(id);
        }
        return true;
    }
    if event.is_command('r') || matches!(event.key, Key::Function(5)) {
        state.send(Command::Reload);
        return true;
    }
    if matches!(event.key, Key::Function(12)) {
        state.console_open = !state.console_open;
        return true;
    }
    if event.mods.ctrl && matches!(event.key, Key::Tab) && !state.tabs.is_empty() {
        let step = if event.mods.shift {
            state.tabs.len() - 1
        } else {
            1
        };
        state.activate((state.active + step) % state.tabs.len());
        return true;
    }
    if event.mods.alt && matches!(event.key, Key::Left | Key::Right) {
        state.send(if matches!(event.key, Key::Left) {
            Command::Back
        } else {
            Command::Forward
        });
        return true;
    }
    false
}

fn page_key(state: &mut Browser, event: &KeyEvent, tx: &mut Tx) -> bool {
    if shortcuts(state, event, tx) {
        return true;
    }
    if event.is_command('c') {
        state.send(Command::CopySelection);
        return true;
    }
    if matches!(event.key, Key::Escape) {
        state.select = None;
        state.send(Command::ClearSelection);
        return true;
    }
    if let Key::Text(value) = &event.key {
        if !event.mods.cmd || (event.mods.ctrl && event.mods.alt) {
            state.send(Command::Text(value.clone()));
            return true;
        }
    }
    let key = match event.key {
        Key::Enter => "Enter",
        Key::Backspace => "Backspace",
        Key::Delete => "Delete",
        Key::Left => "ArrowLeft",
        Key::Right => "ArrowRight",
        Key::Up => "ArrowUp",
        Key::Down => "ArrowDown",
        Key::Home => "Home",
        Key::End => "End",
        Key::PageUp => "PageUp",
        Key::PageDown => "PageDown",
        _ => return false,
    };
    if !state.tab().is_some_and(|t| t.snapshot.text_focus) {
        let dy = match event.key {
            Key::Down => 40.0,
            Key::Up => -40.0,
            Key::PageDown => state.viewport.1 as f32 * 0.9,
            Key::PageUp => -(state.viewport.1 as f32) * 0.9,
            Key::Home => -1e7,
            Key::End => 1e7,
            _ => return false,
        };
        state.send(Command::Scroll(dy * state.environment.get().0));
    } else {
        state.send(Command::Key {
            key: key.into(),
            code: key.into(),
        });
    }
    true
}

fn on_ime(state: &mut Browser, event: &rune::Ime, tx: &mut Tx) -> bool {
    if tx.focused() != Some("page") {
        return false;
    }
    if let rune::Ime::Commit(text) = event {
        state.send(Command::Text(text.clone()));
    }
    true
}

pub fn app() -> rune::App<Browser> {
    rune::App::new(AppSpec {
        setup: Some(setup),
        poll: Some(poll),
        on_key: Some(shortcuts),
        min_size: Some((640.0, 400.0)),
        ..AppSpec::new("Browser", (1120.0, 800.0), init, view)
    })
    .chrome(rune::Chrome {
        full_size_content: false,
        ..rune::Chrome::default()
    })
    .on_ime(on_ime)
}

#[cfg(test)]
#[path = "ui_tests.rs"]
mod ui_tests;
