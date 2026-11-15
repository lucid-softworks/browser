//! Per-browser host settings and page outputs shared with its JavaScript worker.
//!
//! A thread-local handle routes legacy host primitives to the current session. The data itself
//! belongs to the browser, not a thread: engine and JS workers share the same context explicitly.
use std::{
    cell::RefCell,
    marker::PhantomData,
    rc::Rc,
    sync::{Arc, Mutex},
};

#[derive(Clone)]
pub struct HostContext(Arc<Mutex<HostData>>);

struct HostData {
    execution: crate::ExecutionControl,
    shutdown: bool,
    metrics: (u32, u32, f32),
    dark: bool,
    isolated: bool,
    response_size: u32,
    selection: Option<(usize, u32, usize, u32)>,
    highlights: Vec<(usize, String)>,
    scroll: Option<f32>,
    navigation: Option<(String, bool)>,
    interaction: (Option<usize>, Option<usize>),
    revision: u64,
    font: Option<Arc<fontdue::Font>>,
}

impl Default for HostContext {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(HostData {
            execution: crate::ExecutionControl::default(),
            shutdown: false,
            metrics: (1200, 780, 2.0),
            dark: false,
            isolated: false,
            response_size: 0,
            selection: None,
            highlights: Vec::new(),
            scroll: None,
            navigation: None,
            interaction: (None, None),
            revision: 0,
            font: None,
        })))
    }
}

thread_local! {
    static CURRENT: RefCell<HostContext> = RefCell::new(HostContext::default());
}

/// Restores the caller's context on this thread, including on unwind.
#[must_use]
pub struct HostContextGuard {
    previous: HostContext,
    _thread_bound: PhantomData<Rc<()>>,
}

impl HostContext {
    /// Cancel the current page execution from any thread. A new Session installs a fresh control.
    pub fn cancel_execution(&self) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .execution
            .cancel();
    }
    /// Permanently stop this browser owner, including sessions created by an in-flight navigation.
    pub fn shutdown_execution(&self) {
        let mut data = self.0.lock().unwrap_or_else(|e| e.into_inner());
        data.shutdown = true;
        data.execution.cancel();
    }
    pub(crate) fn new_execution(&self) -> crate::ExecutionControl {
        let control = crate::ExecutionControl::default();
        let mut data = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if data.shutdown {
            control.cancel();
        }
        data.execution = control.clone();
        control
    }
    pub(crate) fn current() -> Self {
        CURRENT.with(|c| c.borrow().clone())
    }

    pub fn enter(&self) -> HostContextGuard {
        let previous = CURRENT.with(|c| c.replace(self.clone()));
        HostContextGuard {
            previous,
            _thread_bound: PhantomData,
        }
    }
}

impl Drop for HostContextGuard {
    fn drop(&mut self) {
        CURRENT.with(|c| {
            c.replace(self.previous.clone());
        });
    }
}

fn with_data<R>(f: impl FnOnce(&mut HostData) -> R) -> R {
    let context = HostContext::current();
    let mut data = context.0.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut data)
}

pub fn set_response_body_size(bytes: usize) {
    with_data(|d| d.response_size = bytes.min(u32::MAX as usize) as u32);
}
pub(crate) fn response_body_size() -> u32 {
    with_data(|d| d.response_size)
}
pub fn set_cross_origin_isolated(isolated: bool) {
    with_data(|d| d.isolated = isolated);
}
pub(crate) fn cross_origin_isolated() -> bool {
    with_data(|d| d.isolated)
}
pub fn set_active_selection(selection: Option<(usize, u32, usize, u32)>) {
    with_data(|d| d.selection = selection);
}
pub fn active_selection() -> Option<(usize, u32, usize, u32)> {
    with_data(|d| d.selection)
}
pub fn clear_active_highlights() {
    with_data(|d| d.highlights.clear());
}
pub(crate) fn add_active_highlight(node: usize, name: String) {
    with_data(|d| d.highlights.push((node, name)));
}
pub fn active_highlights() -> Vec<(usize, String)> {
    with_data(|d| d.highlights.clone())
}
pub fn take_pending_scroll() -> Option<f32> {
    with_data(|d| d.scroll.take())
}
pub(crate) fn set_pending_scroll(y: f32) {
    with_data(|d| d.scroll = Some(y));
}
/// Take a top-level page navigation request: destination and whether to replace history.
pub fn take_pending_navigation() -> Option<(String, bool)> {
    with_data(|d| d.navigation.take())
}
pub(crate) fn set_pending_navigation(url: String, replace: bool) {
    with_data(|d| d.navigation = Some((url, replace)));
}
pub fn set_device_metrics(width: u32, height: u32, ratio: f32) {
    with_data(|d| {
        let metrics = (width.max(1), height.max(1), ratio.max(0.1));
        if d.metrics != metrics {
            d.metrics = metrics;
            d.revision += 1;
        }
    });
}
pub(crate) fn device_metrics() -> (f64, f64, f64) {
    with_data(|d| (d.metrics.0 as f64, d.metrics.1 as f64, d.metrics.2 as f64))
}
pub fn set_color_scheme_dark(dark: bool) {
    with_data(|d| {
        if d.dark != dark {
            d.dark = dark;
            d.revision += 1;
        }
    });
}
pub(crate) fn color_scheme_dark() -> bool {
    with_data(|d| d.dark)
}
pub fn set_interaction_state(hover: Option<usize>, focus: Option<usize>) {
    with_data(|d| {
        if d.interaction != (hover, focus) {
            d.interaction = (hover, focus);
            d.revision += 1;
        }
    });
}
pub(crate) fn focused_node() -> Option<usize> {
    with_data(|d| d.interaction.1)
}
/// Install decoded font bytes used by the host painter for synchronous JS geometry queries.
pub fn set_font_data(bytes: Vec<u8>) -> bool {
    let Ok(font) = fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()) else {
        return false;
    };
    with_data(|d| {
        d.font = Some(Arc::new(font));
        d.revision += 1;
    });
    true
}
pub(crate) fn font() -> Option<Arc<fontdue::Font>> {
    with_data(|d| d.font.clone())
}

/// Install session settings on the JS thread before computing styles or geometry. A settings
/// change invalidates caches even when the DOM itself has not changed.
pub(crate) fn sync_style_context(state: &crate::HostState) {
    let (metrics, dark, interaction, revision) =
        with_data(|d| (d.metrics, d.dark, d.interaction, d.revision));
    style::set_viewport_metrics(metrics.0 as f32, metrics.1 as f32, metrics.2);
    style::set_color_scheme_dark(dark);
    style::set_interaction_state(interaction.0, interaction.1);
    style::set_document_base_url(Some(&state.page_url.borrow()));
    if state.style_context_revision.replace(revision) != revision {
        *state.computed_cache.borrow_mut() = None;
        state.rects_dom_version.set(u64::MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_cancels_sessions_created_after_an_in_flight_navigation() {
        let host = HostContext::default();
        let initial = host.new_execution();
        host.cancel_execution();
        assert!(initial.is_cancelled());
        let next = host.new_execution();
        assert!(!next.is_cancelled(), "normal cancellation permits reload");

        let navigation_host = host.clone();
        let (resume, delayed) = std::sync::mpsc::channel();
        let navigation = std::thread::spawn(move || {
            delayed.recv().unwrap();
            navigation_host.new_execution()
        });
        host.shutdown_execution();
        assert!(next.is_cancelled());
        resume.send(()).unwrap();
        assert!(navigation.join().unwrap().is_cancelled());
        assert!(
            host.new_execution().is_cancelled(),
            "shutdown stays permanent"
        );
    }
}
