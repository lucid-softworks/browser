//! Profiling: time cascade vs layout for a URL. Usage: profile <url> [w] [h]
use std::time::Instant;

fn main() {
    let url = std::env::args()
        .nth(1)
        .expect("usage: profile <url> [w] [h]");
    let w: f32 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1280.0);
    let h: f32 = std::env::args()
        .nth(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(900.0);

    let resp = net::fetch(&url).expect("fetch");
    let html = String::from_utf8_lossy(&resp.body).into_owned();
    let base = resp.final_url.clone();

    let doc = html::parse(&html);
    let (doc, _console) = engine::run_scripts(doc, &base);

    let t = Instant::now();
    let (sheets, _n) = engine::collect_stylesheets(&doc, &base);
    eprintln!(
        "collect_stylesheets: {:?} ({} sheets)",
        t.elapsed(),
        sheets.len()
    );

    // Warm + timed cascade runs.
    let mut last = None;
    for i in 0..3 {
        let t = Instant::now();
        let computed = style::cascade(&doc, &sheets);
        let dt = t.elapsed();
        eprintln!("cascade #{i}: {:?} ({} computed)", dt, computed.len());
        last = Some(computed);
    }
    let computed = last.unwrap();

    let intrinsic = std::collections::HashMap::new();
    for i in 0..3 {
        let t = Instant::now();
        let _root = layout::layout_document(&doc, &computed, w, h, &Stub, &intrinsic, None);
        eprintln!("layout #{i}: {:?}", t.elapsed());
    }

    eprintln!("DOM nodes: {}", doc.len());
}

struct Stub;
impl layout::TextMeasurer for Stub {
    fn text_width(&self, s: &str, fs: f32, _bold: bool) -> f32 {
        s.chars().count() as f32 * fs * 0.5
    }
    fn line_height(&self, fs: f32) -> f32 {
        fs * 1.2
    }
}
