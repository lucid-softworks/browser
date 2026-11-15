//! Toolkit-independent desktop bridge. The engine and its JavaScript session stay on a tab's
//! worker thread. Only owned pixels and plain data cross into the window event loop.
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub enum Command {
    Navigate(String),
    Back,
    Forward,
    Reload,
    Viewport { width: u32, height: u32, scale: f32 },
    Appearance(bool),
    Active(bool),
    Click { x: f32, y: f32 },
    Mouse { kind: String, x: f32, y: f32 },
    Move { x: f32, y: f32 },
    Scroll(f32),
    Key { key: String, code: String },
    Text(String),
    SelectionStart { x: f32, y: f32 },
    SelectionExtend { x: f32, y: f32 },
    ClearSelection,
    CopySelection,
    Select { node_id: usize, index: usize },
    Eval(String),
    Close,
}

pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Tightly packed, straight-alpha RGBA8 pixels.
    pub rgba: Vec<u8>,
    pub sequence: u64,
}

#[derive(Clone, Default)]
pub struct Snapshot {
    /// Monotonic navigation identity, including reloads of the same URL.
    pub document_id: u64,
    pub frame: Option<Arc<Frame>>,
    pub url: String,
    pub title: String,
    pub loading: bool,
    pub error: Option<String>,
    pub can_back: bool,
    pub can_forward: bool,
    pub hover_link: Option<String>,
    pub text_focus: bool,
}

pub enum Event {
    Clipboard(String),
    Select(engine::SelectHit),
    Console { document_id: u64, line: String },
}

#[derive(Default)]
struct Mailbox {
    snapshot: Option<Snapshot>,
    events: VecDeque<Event>,
}

pub struct Worker {
    interrupt: Arc<Mutex<Option<engine::InterruptHandle>>>,
    commands: mpsc::SyncSender<Message>,
    mailbox: Arc<Mutex<Mailbox>>,
    shutdown: Arc<AtomicBool>,
}

enum Message {
    Command(Command),
    Barrier(mpsc::Sender<()>),
}

impl Worker {
    pub fn spawn(repaint: Arc<dyn Fn() + Send + Sync>) -> std::io::Result<Self> {
        let interrupt = Arc::new(Mutex::new(None));
        let thread_interrupt = interrupt.clone();
        let (commands, receiver) = mpsc::sync_channel(256);
        let mailbox = Arc::new(Mutex::new(Mailbox::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_mailbox = mailbox.clone();
        let worker_shutdown = shutdown.clone();
        std::thread::Builder::new()
            .name("browser-tab".into())
            .stack_size(16 * 1024 * 1024)
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let tab = Tab::new(worker_mailbox.clone(), repaint.clone());
                    *thread_interrupt.lock().unwrap_or_else(|e| e.into_inner()) = Some(tab.engine.interrupt_handle());
                    tab.run(receiver, &worker_shutdown);
                }));
                if result.is_err() {
                    let mut mailbox = worker_mailbox.lock().unwrap_or_else(|e| e.into_inner());
                    let mut snapshot = mailbox.snapshot.take().unwrap_or_default();
                    snapshot.loading = false;
                    snapshot.error = Some("The page's engine stopped unexpectedly. Close this tab and open a new one.".into());
                    mailbox.snapshot = Some(snapshot);
                    drop(mailbox);
                    repaint();
                }
            })?;
        Ok(Self {
            interrupt,
            commands,
            mailbox,
            shutdown,
        })
    }

    /// False means the tab has closed or its input queue is full; the UI must not block waiting.
    pub fn send(&self, command: Command) -> bool {
        !self.shutdown.load(Ordering::Relaxed)
            && self.commands.try_send(Message::Command(command)).is_ok()
    }

    /// Wait for earlier commands and their rendered frame to reach the mailbox. This is only
    /// for explicitly requested deterministic headless captures; normal window input uses send.
    /// Returns false on a busy/full queue, closed tab, or timeout without discarding queued input.
    pub fn synchronize(&self, timeout: Duration) -> bool {
        let (reply, received) = mpsc::channel();
        !self.shutdown.load(Ordering::Relaxed)
            && self.commands.try_send(Message::Barrier(reply)).is_ok()
            && received.recv_timeout(timeout).is_ok()
    }

    pub fn take_snapshot(&self) -> Option<Snapshot> {
        self.mailbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot
            .take()
    }

    pub fn drain_events(&self) -> Vec<Event> {
        self.mailbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .events
            .drain(..)
            .collect()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self
            .interrupt
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            handle.shutdown();
        }
        let _ = self.commands.try_send(Message::Command(Command::Close));
        // The sticky interpreter interrupt stops author loops. Never block the window waiting
        // for a host network operation to return; the tab thread owns its final teardown.
    }
}

#[derive(Default)]
struct History {
    entries: Vec<String>,
    index: Option<usize>,
}
impl History {
    fn push(&mut self, url: String) {
        if self.current() == Some(url.as_str()) {
            return;
        }
        self.entries.truncate(self.index.map_or(0, |i| i + 1));
        self.entries.push(url);
        self.index = Some(self.entries.len() - 1);
    }
    fn current(&self) -> Option<&str> {
        self.index.map(|i| self.entries[i].as_str())
    }
    fn back(&mut self) -> Option<String> {
        let i = self.index?.checked_sub(1)?;
        self.index = Some(i);
        Some(self.entries[i].clone())
    }
    fn forward(&mut self) -> Option<String> {
        let i = self.index? + 1;
        if i >= self.entries.len() {
            return None;
        }
        self.index = Some(i);
        Some(self.entries[i].clone())
    }
    fn replace(&mut self, url: String) {
        if let Some(i) = self.index {
            self.entries[i] = url;
        } else {
            self.push(url);
        }
    }
    fn can_back(&self) -> bool {
        self.index.is_some_and(|i| i > 0)
    }
    fn can_forward(&self) -> bool {
        self.index.is_some_and(|i| i + 1 < self.entries.len())
    }
}

struct Tab {
    engine: engine::Engine,
    history: History,
    snapshot: Snapshot,
    mailbox: Arc<Mutex<Mailbox>>,
    repaint: Arc<dyn Fn() + Send + Sync>,
    active: bool,
    sequence: u64,
    console_count: usize,
}

impl Tab {
    fn new(mailbox: Arc<Mutex<Mailbox>>, repaint: Arc<dyn Fn() + Send + Sync>) -> Self {
        let mut engine = engine::Engine::new();
        assert!(
            engine.set_font_data(crate::platform::bundled_font().to_vec()),
            "bundled desktop font must parse"
        );
        Self {
            engine,
            history: History::default(),
            snapshot: Snapshot::default(),
            mailbox,
            repaint,
            active: true,
            sequence: 0,
            console_count: 0,
        }
    }

    fn publish(&mut self, render: bool) {
        if render {
            let frame = self.engine.render();
            let row_bytes = frame.width as usize * 4;
            let mut rgba = Vec::with_capacity(row_bytes * frame.height as usize);
            for row in frame
                .pixels
                .chunks(frame.stride as usize)
                .take(frame.height as usize)
            {
                rgba.extend_from_slice(&row[..row_bytes]);
            }
            self.sequence = self.sequence.wrapping_add(1);
            self.snapshot.frame = Some(Arc::new(Frame {
                width: frame.width,
                height: frame.height,
                rgba,
                sequence: self.sequence,
            }));
        }
        self.snapshot.can_back = self.history.can_back();
        self.snapshot.can_forward = self.history.can_forward();
        self.snapshot.text_focus = self.engine.has_text_focus();
        if !self.snapshot.loading {
            self.snapshot.title = self.engine.title().unwrap_or_else(|| "New Tab".into());
        }
        let logs = self.engine.console_lines();
        for line in logs.iter().skip(self.console_count).rev().take(128).rev() {
            self.event(Event::Console {
                document_id: self.snapshot.document_id,
                line: line.clone(),
            });
        }
        self.console_count = logs.len();
        self.mailbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot = Some(self.snapshot.clone());
        (self.repaint)();
    }

    fn event(&self, event: Event) {
        let mut mailbox = self.mailbox.lock().unwrap_or_else(|e| e.into_inner());
        if mailbox.events.len() == 256 {
            mailbox.events.pop_front();
        }
        mailbox.events.push_back(event);
        drop(mailbox);
        (self.repaint)();
    }

    fn navigate(&mut self, url: String, new_entry: bool) {
        self.snapshot.document_id += 1;
        // Ignore any not-yet-published old-document logs when publishing the loading snapshot.
        self.console_count = self.engine.console_lines().len();
        self.snapshot.url = url.clone();
        self.snapshot.loading = true;
        self.snapshot.error = None;
        self.snapshot.hover_link = None;
        self.publish(false);
        let status = self.engine.load_url(&url);
        let final_url = self.engine.current_url().unwrap_or(&url).to_owned();
        if new_entry {
            self.history.push(final_url.clone());
        } else {
            self.history.replace(final_url.clone());
        }
        self.snapshot.url = final_url;
        self.snapshot.loading = false;
        self.snapshot.error = (status < 0).then(|| format!("Could not load {url}"));
        self.console_count = 0;
        self.publish(true);
    }

    fn run(mut self, receiver: mpsc::Receiver<Message>, shutdown: &AtomicBool) {
        self.publish(true);
        let mut pending = None;
        let mut last_tick = Instant::now();
        while !shutdown.load(Ordering::Relaxed) {
            let delay = if self.active {
                Duration::from_millis(16)
            } else {
                Duration::from_millis(250)
            };
            let received = match pending.take() {
                Some(command) => Ok(command),
                None => receiver.recv_timeout(delay.saturating_sub(last_tick.elapsed())),
            };
            match received {
                Ok(Message::Command(Command::Close))
                | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Ok(Message::Barrier(reply)) => {
                    self.engine.tick();
                    self.publish(true);
                    last_tick = Instant::now();
                    let _ = reply.send(());
                }
                Ok(Message::Command(mut command)) => {
                    // Drop obsolete adjacent resize/motion requests, but never reorder a click,
                    // key, or navigation around a viewport change.
                    if matches!(command, Command::Move { .. } | Command::Viewport { .. }) {
                        while let Ok(next) = receiver.try_recv() {
                            if matches!(
                                (&command, &next),
                                (Command::Move { .. }, Message::Command(Command::Move { .. }))
                                    | (
                                        Command::Viewport { .. },
                                        Message::Command(Command::Viewport { .. })
                                    )
                            ) {
                                if let Message::Command(next) = next {
                                    command = next;
                                }
                            } else {
                                pending = Some(next);
                                break;
                            }
                        }
                    }
                    self.handle(command);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            // A steady stream of mouse/scroll commands must not starve page timers.
            if last_tick.elapsed() >= delay && !shutdown.load(Ordering::Relaxed) {
                if self.engine.tick() {
                    self.publish(true);
                }
                last_tick = Instant::now();
            }
        }
    }

    fn handle(&mut self, command: Command) {
        let dirty = match command {
            Command::Navigate(url) => {
                self.navigate(url, true);
                return;
            }
            Command::Back => {
                if let Some(url) = self.history.back() {
                    self.navigate(url, false);
                }
                return;
            }
            Command::Forward => {
                if let Some(url) = self.history.forward() {
                    self.navigate(url, false);
                }
                return;
            }
            Command::Reload => {
                if let Some(url) = self.history.current().map(str::to_owned) {
                    self.navigate(url, false);
                }
                return;
            }
            Command::Viewport {
                width,
                height,
                scale,
            } => {
                self.engine.set_viewport(width, height, scale);
                true
            }
            Command::Appearance(dark) => {
                self.engine.set_color_scheme(dark);
                true
            }
            Command::Active(active) => {
                self.active = active;
                false
            }
            Command::Click { x, y } => {
                if let Some(hit) = self.engine.select_at(x, y) {
                    self.event(Event::Select(hit));
                    return;
                }
                let link = self.engine.link_at(x, y);
                let dirty = self.engine.dispatch_click(x, y);
                // Matches the existing Swift shell: dispatch_click exposes only DOM dirtiness,
                // not the event's defaultPrevented flag. A future engine API can report whether
                // link activation was cancelled without injecting shell-specific page listeners.
                if let Some(url) = link {
                    self.navigate(url, true);
                    return;
                }
                dirty
            }
            Command::Mouse { kind, x, y } => self.engine.dispatch_mouse(&kind, x, y),
            Command::Move { x, y } => {
                let link = self.engine.link_at(x, y);
                let hover_changed = link != self.snapshot.hover_link;
                self.snapshot.hover_link = link;
                let dirty = self.engine.dispatch_move(x, y);
                if hover_changed && !dirty {
                    self.publish(false);
                }
                dirty
            }
            Command::Scroll(dy) => {
                self.engine.scroll_by(dy);
                true
            }
            Command::Key { key, code } => self.engine.dispatch_key(&key, &code),
            Command::Text(text) => {
                let mut dirty = false;
                for ch in text.chars() {
                    dirty |= self.engine.dispatch_key(&ch.to_string(), "");
                }
                dirty
            }
            Command::SelectionStart { x, y } => {
                self.engine.selection_start(x, y);
                true
            }
            Command::SelectionExtend { x, y } => {
                self.engine.selection_extend(x, y);
                true
            }
            Command::ClearSelection => {
                self.engine.selection_clear();
                true
            }
            Command::CopySelection => {
                self.event(Event::Clipboard(self.engine.selected_text()));
                false
            }
            Command::Select { node_id, index } => self.engine.set_select_index(node_id, index),
            Command::Eval(code) => {
                let result = self.engine.console_eval(&code);
                self.event(Event::Console {
                    document_id: self.snapshot.document_id,
                    line: result,
                });
                true
            }
            Command::Close => false,
        };
        if dirty {
            self.publish(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_branches_and_replaces_redirects() {
        let mut history = History::default();
        assert!(history.back().is_none());
        history.push("a".into());
        history.push("b".into());
        history.push("c".into());
        assert_eq!(history.back().as_deref(), Some("b"));
        history.replace("b-redirect".into());
        assert_eq!(history.forward().as_deref(), Some("c"));
        history.back();
        history.push("d".into());
        assert!(history.forward().is_none());
        assert_eq!(history.back().as_deref(), Some("b-redirect"));
        assert_eq!(history.back().as_deref(), Some("a"));
    }

    #[test]
    fn closing_a_tab_stops_its_running_javascript_thread() {
        let worker = Worker::spawn(Arc::new(|| {})).unwrap();
        assert!(worker.send(Command::Navigate("about:blank".into())));
        assert!(worker.synchronize(Duration::from_secs(15)));
        let mailbox = Arc::downgrade(&worker.mailbox);
        assert!(worker.send(Command::Eval("for (;;) {}".into())));
        // Let the worker enter the callback before closing it. The weak mailbox checks
        // actual thread teardown, rather than merely a nonblocking Worker::drop.
        std::thread::sleep(Duration::from_millis(100));
        drop(worker);
        let started = Instant::now();
        while mailbox.strong_count() != 0 && started.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            mailbox.strong_count(),
            0,
            "closed tab thread is still alive"
        );
    }

    fn wait_for(worker: &Worker, predicate: impl Fn(&Snapshot) -> bool) -> Snapshot {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(15) {
            if let Some(snapshot) = worker.take_snapshot() {
                if predicate(&snapshot) {
                    return snapshot;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("worker did not publish expected state");
    }

    #[test]
    fn engine_stays_off_ui_and_produces_owned_frames() {
        let worker = Worker::spawn(Arc::new(|| {})).unwrap();
        assert!(worker.send(Command::Viewport {
            width: 320,
            height: 200,
            scale: 1.5
        }));
        assert!(worker.send(Command::Navigate("about:blank".into())));
        let snapshot = wait_for(&worker, |s| !s.loading && s.url == "about:blank");
        let frame = snapshot.frame.unwrap();
        assert_eq!((frame.width, frame.height), (480, 300));
        assert_eq!(frame.rgba.len(), 480 * 300 * 4);
        assert!(worker.send(Command::Eval(
            "document.body.textContent = 'worker test'; 'executed'".into()
        )));
        let next = wait_for(&worker, |s| {
            s.frame
                .as_ref()
                .is_some_and(|f| f.sequence > frame.sequence)
        });
        assert_eq!(next.url, "about:blank");
        assert!(worker
            .drain_events()
            .iter()
            .any(|e| matches!(e, Event::Console { line, .. } if line == "executed")));
        let start = Instant::now();
        drop(worker);
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn synchronization_publishes_prior_input_before_acknowledging() {
        let worker = Worker::spawn(Arc::new(|| {})).unwrap();
        assert!(worker.send(Command::Navigate("about:blank".into())));
        assert!(worker.send(Command::Viewport {
            width: 250,
            height: 150,
            scale: 1.0
        }));
        assert!(worker.send(Command::Viewport {
            width: 300,
            height: 200,
            scale: 1.0
        }));
        assert!(worker.send(Command::Eval(
            "document.body.style.backgroundColor = 'red'; 'before barrier'".into()
        )));
        assert!(worker.synchronize(Duration::from_secs(15)));
        let snapshot = worker
            .take_snapshot()
            .expect("render published before barrier reply");
        assert!(!snapshot.loading);
        assert_eq!(snapshot.url, "about:blank");
        let frame = snapshot.frame.unwrap();
        assert_eq!((frame.width, frame.height), (300, 200));
        assert_eq!(&frame.rgba[..4], &[255, 0, 0, 255]);
        assert!(worker
            .drain_events()
            .iter()
            .any(|event| matches!(event, Event::Console { line, .. } if line == "before barrier")));
    }

    #[test]
    fn loaded_page_receives_clicks_and_text() {
        let path = std::env::temp_dir().join(format!("browser-worker-{}.html", std::process::id()));
        std::fs::write(&path, r#"<!doctype html><body>
            <button id="button" style="position:absolute;left:10px;top:10px;width:100px;height:40px">before</button>
            <input id="entry" style="position:absolute;left:10px;top:70px;width:180px;height:30px">
            <script>document.getElementById('button').onclick=function(){this.textContent='clicked'};</script>
        "#).unwrap();
        let absolute = path
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let absolute = absolute.strip_prefix("//?/").unwrap_or(&absolute);
        let url = format!("file:///{}", absolute.trim_start_matches('/'));
        let worker = Worker::spawn(Arc::new(|| {})).unwrap();
        assert!(worker.send(Command::Navigate(url.clone())));
        wait_for(&worker, |s| !s.loading && s.url == url);
        assert!(worker.send(Command::Click { x: 30.0, y: 25.0 }));
        assert!(worker.send(Command::Eval(
            "document.getElementById('button').textContent".into()
        )));
        wait_for_console(&worker, "clicked");
        assert!(worker.send(Command::Click { x: 30.0, y: 85.0 }));
        wait_for(&worker, |s| s.text_focus);
        assert!(worker.send(Command::Text("hello".into())));
        assert!(worker.send(Command::Eval(
            "document.getElementById('entry').value".into()
        )));
        wait_for_console(&worker, "hello");
        drop(worker);
        let _ = std::fs::remove_file(path);
    }

    fn wait_for_console(worker: &Worker, expected: &str) {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(15) {
            if worker
                .drain_events()
                .iter()
                .any(|event| matches!(event, Event::Console { line, .. } if line == expected))
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("worker did not return {expected:?}");
    }
}
